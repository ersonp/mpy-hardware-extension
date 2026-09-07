//! The lib API: `install`, `repair`, `repair_runtime`, `update_extension`,
//! `verify`, `diagnostics`, `uninstall`. Each op is thin orchestration over
//! the already-built step modules -- this file's only real content is
//! sequencing (which steps, in what order, journaling after each one) and
//! wiring the pinned values out of the manifest into each step's call.
//!
//! Every op is generic over [`Environment`], the union of every step
//! module's injected trait, so `ops.rs` itself needs no OS-specific code and
//! stays testable the same way every other module here is. The one real
//! implementation of `Environment` for an actual machine is
//! `blockless-installer-core::system` (cfg-gated per OS) or the CLI's own
//! wiring -- neither is exercised by this module's own tests.
//!
//! No silent auto-updates anywhere: every op here is something the user (or
//! the GUI, on their behalf) explicitly asked for. Nothing here polls for a
//! newer pin and swaps it in on its own.

use crate::extensions::{self, ExtensionsError};
use crate::fetch::FetchOptions;
use crate::manifest::Manifest;
use crate::platform::{Arch, Os, Paths};
use crate::profile;
use crate::runtime::{self, RuntimeError};
use crate::settings::{self, SettingsError};
use crate::state::{self, State};
use crate::uninstall::{self, UninstallFlags, UninstallOutcome, UninstallRunner};
use crate::verify::{self, VerifyInputs};
use crate::vscode::{self, VscodeError};
use std::path::{Path, PathBuf};

/// The union of every step module's injected capability, so ops.rs's
/// functions take one trait object instead of five.
pub trait Environment:
    profile::CommandRunner
    + vscode::VscodeInstaller
    + extensions::ExtensionsRunner
    + runtime::RuntimeRunner
    + UninstallRunner
{
}
impl<T> Environment for T where
    T: profile::CommandRunner
        + vscode::VscodeInstaller
        + extensions::ExtensionsRunner
        + runtime::RuntimeRunner
        + UninstallRunner
{
}

#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    #[error(transparent)]
    State(#[from] state::StateError),
    #[error(transparent)]
    Vscode(#[from] VscodeError),
    #[error(transparent)]
    Extensions(#[from] ExtensionsError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error("could not remove {}: {reason}", path.display())]
    RemoveEnv { path: PathBuf, reason: String },
    #[error("could not build diagnostics bundle: {0}")]
    Diagnostics(String),
}

/// Everything an op needs about this machine + this manifest, resolved
/// once by the caller (the CLI, or a future GUI).
pub struct OpsContext<'a> {
    pub os: Os,
    pub arch: Arch,
    pub paths: Paths,
    pub manifest: &'a Manifest,
    pub client: reqwest::blocking::Client,
    pub fetch_opts: FetchOptions,
    pub code_candidates: Vec<PathBuf>,
    /// mac only; empty on Windows (no target-selection concept there).
    pub mac_install_targets: Vec<PathBuf>,
    /// The bundled VSIX path, if supplied (`--vsix`). Our extension can
    /// never come from anywhere else (extensions.rs), so ops that touch
    /// extensions require this to be `Some` and pointing at a real file.
    pub vsix_path: Option<PathBuf>,
}

impl OpsContext<'_> {
    fn profiles_dir(&self) -> PathBuf {
        self.paths.code_user.join("profiles")
    }

    fn update_api_url(&self) -> String {
        self.manifest
            .components
            .vscode
            .update_api_url(self.os, self.arch)
    }
}

/// Read the prior journal leniently for a non-destructive op (install/
/// repair): a corrupt state.json here means "proceed as if fresh", never
/// "abort the whole install" -- unlike `uninstall`, which must fail closed.
fn read_prior_state_lenient(state_path: &Path) -> Option<State> {
    State::read(state_path).ok().flatten()
}

fn stamp_and_write(state: &mut State, path: &Path) -> Result<(), OpsError> {
    state.updated_at = state::now_iso8601();
    state.write(path)?;
    Ok(())
}

/// Steps 1, 2, 4 (never 3/runtime) -- ports `repair`'s scope exactly:
/// detect-skip-do on VS Code, the extension, and settings.
pub fn repair(env: &dyn Environment, ctx: &OpsContext) -> Result<State, OpsError> {
    let prior = read_prior_state_lenient(&ctx.paths.state);
    let seed = state::seed_from_prior(prior.as_ref(), "blockless");
    let mut current = prior.unwrap_or_default();
    current.profile_location = seed.profile_location.clone();

    let vscode_outcome = vscode::ensure_vscode(
        env,
        &ctx.client,
        ctx.os,
        &ctx.code_candidates,
        &ctx.mac_install_targets,
        &ctx.update_api_url(),
        &ctx.paths.downloads,
        &ctx.fetch_opts,
    )?;
    current.product_version = vscode_outcome.product_version.clone();
    current.vscode_installed_by_us = seed.vscode_installed_by_us || vscode_outcome.installed_by_us;
    current.steps.vscode = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;

    let ext_outcome = extensions::ensure_extensions(
        env,
        env,
        &vscode_outcome.code_cli,
        &ctx.paths.storage,
        &ctx.profiles_dir(),
        &ctx.manifest.profile_name,
        "blockless",
        &ctx.manifest.components.extension.id,
        &ctx.manifest.components.python_extension.id,
        PYLANCE_ID,
        ctx.vsix_path.as_deref(),
        &seed.prior_ext_vsix_sha256,
        seed.profile_created_by_us,
        false,
    )?;
    current.profile_created_by_us = ext_outcome.profile_created_by_us;
    current.ext_vsix_sha256 = ext_outcome.ext_vsix_sha256;
    current.steps.extension = true;
    if let Some(loc) =
        profile::resolve_profile_location(&ctx.paths.storage, &ctx.manifest.profile_name)
    {
        current.profile_location = loc;
    }
    stamp_and_write(&mut current, &ctx.paths.state)?;

    let settings_outcome = settings::ensure_settings(
        env,
        &vscode_outcome.code_cli,
        &ctx.paths.storage,
        &ctx.profiles_dir(),
        &ctx.manifest.profile_name,
        "blockless",
        &ctx.paths.env_python,
        &ctx.manifest.settings,
    )?;
    current.profile_location = settings_outcome.profile_location;
    current.settings_mechanism = "A".to_string();
    current.steps.settings = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;

    Ok(current)
}

/// `install` = `repair` (steps 1, 2, 4) plus step 3 (runtime) plus the final
/// foreground open. Steps run in the M0 order (1, 2, 3, 4), not repair's
/// (1, 2, 4) then 3 tacked on, so a fresh machine's env_python already
/// exists by the time step 4 writes `mpyhw.pythonPath`.
pub fn install(env: &dyn Environment, ctx: &OpsContext) -> Result<State, OpsError> {
    let prior = read_prior_state_lenient(&ctx.paths.state);
    let seed = state::seed_from_prior(prior.as_ref(), "blockless");
    let mut current = prior.unwrap_or_default();
    current.profile_location = seed.profile_location.clone();
    current.mpremote_version = ctx.manifest.components.mpremote.version.clone();
    current.env_python = ctx.paths.env_python.to_string_lossy().into_owned();

    let vscode_outcome = vscode::ensure_vscode(
        env,
        &ctx.client,
        ctx.os,
        &ctx.code_candidates,
        &ctx.mac_install_targets,
        &ctx.update_api_url(),
        &ctx.paths.downloads,
        &ctx.fetch_opts,
    )?;
    current.product_version = vscode_outcome.product_version.clone();
    current.vscode_installed_by_us = seed.vscode_installed_by_us || vscode_outcome.installed_by_us;
    current.steps.vscode = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;

    let ext_outcome = extensions::ensure_extensions(
        env,
        env,
        &vscode_outcome.code_cli,
        &ctx.paths.storage,
        &ctx.profiles_dir(),
        &ctx.manifest.profile_name,
        "blockless",
        &ctx.manifest.components.extension.id,
        &ctx.manifest.components.python_extension.id,
        PYLANCE_ID,
        ctx.vsix_path.as_deref(),
        &seed.prior_ext_vsix_sha256,
        seed.profile_created_by_us,
        false,
    )?;
    current.profile_created_by_us = ext_outcome.profile_created_by_us;
    current.ext_vsix_sha256 = ext_outcome.ext_vsix_sha256;
    current.steps.extension = true;
    if let Some(loc) =
        profile::resolve_profile_location(&ctx.paths.storage, &ctx.manifest.profile_name)
    {
        current.profile_location = loc;
    }
    stamp_and_write(&mut current, &ctx.paths.state)?;

    runtime::ensure_runtime(
        env,
        &ctx.client,
        ctx.os,
        ctx.arch,
        &ctx.manifest.components.uv,
        &ctx.manifest.components.python.series,
        &ctx.manifest.components.mpremote.version,
        &ctx.paths.blk,
        &ctx.paths.env_python,
        &ctx.paths.downloads,
        &ctx.fetch_opts,
    )?;
    current.steps.python = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;

    let settings_outcome = settings::ensure_settings(
        env,
        &vscode_outcome.code_cli,
        &ctx.paths.storage,
        &ctx.profiles_dir(),
        &ctx.manifest.profile_name,
        "blockless",
        &ctx.paths.env_python,
        &ctx.manifest.settings,
    )?;
    current.profile_location = settings_outcome.profile_location;
    current.settings_mechanism = "A".to_string();
    current.steps.settings = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;

    // Final: foreground open into the profile. Every earlier step that
    // spawned a child of its own (register_profile's window fallback) has
    // already closed it before returning, so this is always a fresh
    // extension host -- never an attach to a lingering child, never the
    // user's own session.
    let _ = env.spawn(
        &vscode_outcome.code_cli,
        &["--profile", &ctx.manifest.profile_name, "--new-window"],
    );

    Ok(current)
}

/// Remove `env/` first, then re-run step 3 -- deterministic rather than
/// trusting `uv`'s own detect-and-reuse over a possibly-broken existing
/// venv. Steps 1/2/4 are untouched.
pub fn repair_runtime(env: &dyn Environment, ctx: &OpsContext) -> Result<State, OpsError> {
    let prior = read_prior_state_lenient(&ctx.paths.state);
    let mut current = prior.unwrap_or_default();

    let env_dir = ctx.paths.blk.join("env");
    if env_dir.exists() {
        env.remove_dir_all(&env_dir)
            .map_err(|reason| OpsError::RemoveEnv {
                path: env_dir.clone(),
                reason,
            })?;
    }

    runtime::ensure_runtime(
        env,
        &ctx.client,
        ctx.os,
        ctx.arch,
        &ctx.manifest.components.uv,
        &ctx.manifest.components.python.series,
        &ctx.manifest.components.mpremote.version,
        &ctx.paths.blk,
        &ctx.paths.env_python,
        &ctx.paths.downloads,
        &ctx.fetch_opts,
    )?;
    current.mpremote_version = ctx.manifest.components.mpremote.version.clone();
    current.env_python = ctx.paths.env_python.to_string_lossy().into_owned();
    current.steps.python = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;

    Ok(current)
}

/// Force-reinstall the bundled VSIX regardless of the sha-match skip, and
/// re-journal the sha. Steps 1/3/4 are untouched.
pub fn update_extension(env: &dyn Environment, ctx: &OpsContext) -> Result<State, OpsError> {
    let prior = read_prior_state_lenient(&ctx.paths.state);
    let seed = state::seed_from_prior(prior.as_ref(), "blockless");
    let mut current = prior.unwrap_or_default();

    let code_cli = ctx
        .code_candidates
        .iter()
        .find(|c| env.version(c).is_some())
        .cloned()
        .unwrap_or_else(|| ctx.code_candidates[0].clone());

    let ext_outcome = extensions::ensure_extensions(
        env,
        env,
        &code_cli,
        &ctx.paths.storage,
        &ctx.profiles_dir(),
        &ctx.manifest.profile_name,
        "blockless",
        &ctx.manifest.components.extension.id,
        &ctx.manifest.components.python_extension.id,
        PYLANCE_ID,
        ctx.vsix_path.as_deref(),
        &seed.prior_ext_vsix_sha256,
        seed.profile_created_by_us,
        true, // force: bypass the sha-match skip
    )?;
    current.profile_created_by_us = ext_outcome.profile_created_by_us;
    current.ext_vsix_sha256 = ext_outcome.ext_vsix_sha256;
    current.steps.extension = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;

    Ok(current)
}

/// Bundles `logs/` + `state.json` + the resolved manifest + OS/arch/version
/// facts into a zip at `target_zip`. Read-only, like `verify`: nothing here
/// mutates the machine, and no data leaves it unless the caller exports the
/// zip themselves (ARCHITECTURE §9).
pub fn diagnostics(ctx: &OpsContext, target_zip: &Path) -> Result<(), OpsError> {
    let file = std::fs::File::create(target_zip).map_err(|e| {
        OpsError::Diagnostics(format!("could not create {}: {e}", target_zip.display()))
    })?;
    let mut writer = zip::ZipWriter::new(file);
    let options: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let zip_err = |e: zip::result::ZipError| OpsError::Diagnostics(e.to_string());
    let io_err = |e: std::io::Error| OpsError::Diagnostics(e.to_string());

    if ctx.paths.logs.is_dir() {
        let entries = std::fs::read_dir(&ctx.paths.logs).map_err(io_err)?;
        for entry in entries {
            let entry = entry.map_err(io_err)?;
            if !entry.file_type().map_err(io_err)?.is_file() {
                continue;
            }
            let name = entry.file_name();
            writer
                .start_file(format!("logs/{}", name.to_string_lossy()), options)
                .map_err(zip_err)?;
            let bytes = std::fs::read(entry.path()).map_err(io_err)?;
            std::io::Write::write_all(&mut writer, &bytes).map_err(io_err)?;
        }
    }

    if ctx.paths.state.is_file() {
        writer.start_file("state.json", options).map_err(zip_err)?;
        let bytes = std::fs::read(&ctx.paths.state).map_err(io_err)?;
        std::io::Write::write_all(&mut writer, &bytes).map_err(io_err)?;
    }

    writer
        .start_file("manifest.json", options)
        .map_err(zip_err)?;
    let manifest_bytes = serde_json::to_vec_pretty(ctx.manifest)
        .map_err(|e| OpsError::Diagnostics(e.to_string()))?;
    std::io::Write::write_all(&mut writer, &manifest_bytes).map_err(io_err)?;

    let facts = serde_json::json!({
        "os": match ctx.os { Os::MacOs => "macos", Os::Windows => "windows" },
        "arch": match ctx.arch { Arch::X64 => "x64", Arch::Arm64 => "arm64" },
        "installerVersion": ctx.manifest.installer_version,
    });
    writer.start_file("facts.json", options).map_err(zip_err)?;
    let facts_bytes =
        serde_json::to_vec_pretty(&facts).map_err(|e| OpsError::Diagnostics(e.to_string()))?;
    std::io::Write::write_all(&mut writer, &facts_bytes).map_err(io_err)?;

    writer.finish().map_err(zip_err)?;
    Ok(())
}

pub fn verify(env: &dyn Environment, ctx: &OpsContext) -> Vec<verify::CheckResult> {
    let inputs = VerifyInputs {
        code_candidates: &ctx.code_candidates,
        profile_name: &ctx.manifest.profile_name,
        ext_id: &ctx.manifest.components.extension.id,
        py_ext_id: &ctx.manifest.components.python_extension.id,
        pylance_id: PYLANCE_ID,
        mpremote_version: &ctx.manifest.components.mpremote.version,
        blk: &ctx.paths.blk,
        code_user: &ctx.paths.code_user,
        state_path: &ctx.paths.state,
    };
    verify::run_checks(env, env, env, &inputs)
}

pub fn uninstall(
    env: &dyn Environment,
    ctx: &OpsContext,
    flags: &UninstallFlags,
) -> UninstallOutcome {
    // Derive the VS Code install root from whichever candidate is actually
    // runnable right now (mirrors how every other op resolves it), rather
    // than assuming a fixed mac target index or hand-listing a Windows path
    // separately -- one derivation, correct on both OSes.
    let vscode_dir = ctx
        .code_candidates
        .iter()
        .find(|c| env.version(c).is_some())
        .and_then(|working_cli| vscode_app_root(ctx.os, working_cli))
        .unwrap_or_default(); // nothing resolvable: PathBuf::new() never exists, so nothing gets touched
    uninstall::uninstall(
        env,
        env,
        &ctx.paths.state,
        &ctx.paths.storage,
        &ctx.profiles_dir(),
        &ctx.manifest.profile_name,
        &ctx.paths.blk,
        &vscode_dir,
        flags,
    )
}

/// The VS Code install root from its `code` CLI path: mac
/// `<target>/Visual Studio Code.app` (5 ancestors up from
/// `.../Contents/Resources/app/bin/code`); Windows
/// `<LOCALAPPDATA>/Programs/Microsoft VS Code` (2 ancestors up from
/// `.../bin/code.cmd`).
fn vscode_app_root(os: Os, code_cli: &Path) -> Option<PathBuf> {
    let up = match os {
        Os::MacOs => 5,
        Os::Windows => 2,
    };
    code_cli.ancestors().nth(up).map(Path::to_path_buf)
}

/// Pylance ships as a dependency of `ms-python.python`, never pinned in the
/// manifest on its own (ARCHITECTURE §2's `pythonExtension` entry pulls it
/// implicitly) -- its id is a fixed Marketplace identifier, not a version,
/// so it is not a "version literal" in the sense the manifest-only rule
/// guards against.
const PYLANCE_ID: &str = "ms-python.vscode-pylance";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;
    use crate::uninstall::UninstallOutcome;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    const COMMITTED_MANIFEST: &str = include_str!("../../manifest/installer.manifest.json");

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "blockless-installer-ops-test-{name}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// One fake implementing all five step traits, tuned so a full
    /// install/repair run never needs real network: VS Code is already
    /// "installed" at `code_candidates[0]`, and mpremote is already at the
    /// pinned version unless a test says otherwise -- both skip their
    /// download paths entirely, leaving ops.rs's own sequencing/journaling
    /// as what's actually under test (the step behaviors themselves are
    /// already covered in their own modules).
    struct FakeEnvironment {
        code_cli: PathBuf,
        vscode_version: RefCell<Option<String>>,
        vsix_path_str: String,
        ext_id: String,
        installed_extensions: RefCell<HashSet<String>>,
        install_calls: RefCell<Vec<String>>,
        mpremote_version: RefCell<Option<String>>,
        run_uv_calls: RefCell<u32>,
        remove_dir_calls: RefCell<Vec<PathBuf>>,
        spawn_calls: RefCell<Vec<Vec<String>>>,
    }

    impl FakeEnvironment {
        fn new(code_cli: &Path, vsix_path: &Path) -> FakeEnvironment {
            FakeEnvironment {
                code_cli: code_cli.to_path_buf(),
                vscode_version: RefCell::new(Some("1.99.0".to_string())),
                vsix_path_str: vsix_path.to_string_lossy().into_owned(),
                ext_id: "blockless.mpy-hardware-extension".to_string(),
                installed_extensions: RefCell::new(HashSet::new()),
                install_calls: RefCell::new(Vec::new()),
                mpremote_version: RefCell::new(Some("mpremote 1.28.0".to_string())),
                run_uv_calls: RefCell::new(0),
                remove_dir_calls: RefCell::new(Vec::new()),
                spawn_calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl profile::CommandRunner for FakeEnvironment {
        fn running_vscode_pids(&self) -> Vec<u32> {
            vec![]
        }
        fn spawn(&self, _code_cli: &Path, args: &[&str]) -> std::io::Result<u32> {
            self.spawn_calls
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
            Ok(1)
        }
        fn is_alive(&self, _pid: u32) -> bool {
            false
        }
        fn request_graceful_close(&self, _pid: u32) {}
        fn force_kill(&self, _pid: u32) {}
        fn sleep(&self, _d: Duration) {}
    }

    impl vscode::VscodeInstaller for FakeEnvironment {
        fn version(&self, code_cli: &Path) -> Option<String> {
            if code_cli == self.code_cli {
                self.vscode_version.borrow().clone()
            } else {
                None
            }
        }
        fn is_writable(&self, _dir: &Path) -> bool {
            true
        }
        fn extract_archive(
            &self,
            _archive: &Path,
            _target_dir: &Path,
        ) -> Result<(), vscode::InstallError> {
            Ok(())
        }
        fn strip_quarantine(&self, _app_dir: &Path) {}
        fn run_silent_installer(&self, _installer_exe: &Path) -> Result<(), vscode::InstallError> {
            Ok(())
        }
        fn verify_signature(&self, _artifact: &Path) -> Result<(), vscode::SignatureError> {
            Ok(())
        }
    }

    impl extensions::ExtensionsRunner for FakeEnvironment {
        fn list_extensions(&self, _code_cli: &Path, _profile_name: &str) -> Option<Vec<String>> {
            Some(self.installed_extensions.borrow().iter().cloned().collect())
        }
        fn install_extension(
            &self,
            _code_cli: &Path,
            _profile_name: &str,
            vsix_or_id: &str,
        ) -> bool {
            self.install_calls.borrow_mut().push(vsix_or_id.to_string());
            let id = if vsix_or_id == self.vsix_path_str {
                self.ext_id.clone()
            } else {
                vsix_or_id.to_string()
            };
            self.installed_extensions
                .borrow_mut()
                .insert(id.to_lowercase());
            true
        }
    }

    impl runtime::RuntimeRunner for FakeEnvironment {
        fn mpremote_version(&self, _envpy: &Path) -> Option<String> {
            self.mpremote_version.borrow().clone()
        }
        fn uv_version(&self, _uv_bin: &Path) -> Option<String> {
            Some("uv 0.11.29".to_string())
        }
        fn extract_uv(&self, _archive: &Path, _dest_dir: &Path) -> Result<(), String> {
            Ok(())
        }
        fn run_uv(&self, _uv_bin: &Path, _args: &[&str], _env: &[(&str, &str)]) -> bool {
            *self.run_uv_calls.borrow_mut() += 1;
            // the third call in the fixed sequence (python install, venv,
            // pip install) is what "lands" mpremote
            if *self.run_uv_calls.borrow() == 3 {
                *self.mpremote_version.borrow_mut() = Some("mpremote 1.28.0".to_string());
            }
            true
        }
    }

    impl UninstallRunner for FakeEnvironment {
        fn remove_dir_all(&self, path: &Path) -> Result<(), String> {
            self.remove_dir_calls.borrow_mut().push(path.to_path_buf());
            let _ = std::fs::remove_dir_all(path);
            Ok(())
        }
        fn run_vscode_uninstaller(&self, _vscode_dir: &Path) -> Result<bool, String> {
            Ok(false)
        }
    }

    fn test_manifest() -> Manifest {
        Manifest::parse(COMMITTED_MANIFEST).unwrap()
    }

    fn write_vsix(dir: &Path, contents: &[u8]) -> PathBuf {
        let path = dir.join("mpy-hardware-extension.vsix");
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn make_ctx<'a>(dir: &Path, manifest: &'a Manifest, vsix: &Path) -> OpsContext<'a> {
        let code_cli = dir.join("code");
        OpsContext {
            os: Os::MacOs,
            arch: Arch::Arm64,
            paths: Paths {
                blk: dir.join("Blockless"),
                downloads: dir.join("Blockless").join("downloads"),
                logs: dir.join("Blockless").join("logs"),
                state: dir.join("Blockless").join("state.json"),
                code_user: dir.join("CodeUser"),
                storage: dir.join("CodeUser").join("storage.json"),
                env_python: dir.join("Blockless").join("env").join("bin").join("python"),
            },
            manifest,
            client: reqwest::blocking::Client::new(),
            fetch_opts: FetchOptions {
                max_attempts: 1,
                backoff_base: Duration::from_millis(1),
            },
            code_candidates: vec![code_cli],
            mac_install_targets: vec![dir.join("Applications")],
            vsix_path: Some(vsix.to_path_buf()),
        }
    }

    #[test]
    fn install_happy_path_journals_all_four_steps_and_opens_foreground() {
        let dir = temp_dir("install-happy");
        let manifest = test_manifest();
        let vsix = write_vsix(&dir, b"vsix contents");
        let ctx = make_ctx(&dir, &manifest, &vsix);
        let env = FakeEnvironment::new(&ctx.code_candidates[0], &vsix);

        let state = install(&env, &ctx).unwrap();

        assert!(state.steps.all_ok(), "{:?}", state.steps);
        assert_eq!(state.mpremote_version, "1.28.0");
        assert!(
            env.spawn_calls
                .borrow()
                .iter()
                .any(|args| args.contains(&"--new-window".to_string())),
            "install must end with a foreground open"
        );
    }

    #[test]
    fn repair_never_touches_runtime() {
        let dir = temp_dir("repair-no-runtime");
        let manifest = test_manifest();
        let vsix = write_vsix(&dir, b"vsix contents");
        let ctx = make_ctx(&dir, &manifest, &vsix);
        let env = FakeEnvironment::new(&ctx.code_candidates[0], &vsix);
        // mpremote is NOT present -- if repair touched runtime.rs at all, it
        // would try to provision it (run_uv calls > 0).
        *env.mpremote_version.borrow_mut() = None;

        let state = repair(&env, &ctx).unwrap();

        assert!(state.steps.vscode);
        assert!(state.steps.extension);
        assert!(state.steps.settings);
        assert!(!state.steps.python, "repair must never touch step 3");
        assert_eq!(
            *env.run_uv_calls.borrow(),
            0,
            "repair must never invoke uv at all"
        );
    }

    #[test]
    fn repair_runtime_removes_env_dir_before_reprovisioning() {
        let dir = temp_dir("repair-runtime");
        let manifest = test_manifest();
        let vsix = write_vsix(&dir, b"vsix contents");
        let ctx = make_ctx(&dir, &manifest, &vsix);
        let env_dir = ctx.paths.blk.join("env");
        std::fs::create_dir_all(&env_dir).unwrap();
        std::fs::write(env_dir.join("stale-marker"), b"old").unwrap();
        let env = FakeEnvironment::new(&ctx.code_candidates[0], &vsix);
        // force a real re-provision: without this, "mpremote already
        // present" would skip before even checking env/ at all.
        *env.mpremote_version.borrow_mut() = None;

        let state = repair_runtime(&env, &ctx).unwrap();

        assert!(state.steps.python);
        assert!(
            env.remove_dir_calls.borrow().contains(&env_dir),
            "must remove env/ deterministically rather than trust uv over a stale venv"
        );
        assert!(*env.run_uv_calls.borrow() > 0, "must actually reprovision");
    }

    #[test]
    fn update_extension_forces_reinstall_bypassing_the_sha_match_skip() {
        let dir = temp_dir("update-ext-force");
        let manifest = test_manifest();
        let vsix = write_vsix(&dir, b"vsix contents");
        let sha = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(std::fs::read(&vsix).unwrap());
            h.finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let ctx = make_ctx(&dir, &manifest, &vsix);
        let env = FakeEnvironment::new(&ctx.code_candidates[0], &vsix);
        // fully current already: matching sha journaled, all three
        // extensions present -- a non-forced call would skip entirely.
        env.installed_extensions
            .borrow_mut()
            .insert("blockless.mpy-hardware-extension".to_string());
        env.installed_extensions
            .borrow_mut()
            .insert("ms-python.python".to_string());
        env.installed_extensions
            .borrow_mut()
            .insert("ms-python.vscode-pylance".to_string());
        let prior = State {
            ext_vsix_sha256: sha.clone(),
            ..Default::default()
        };
        prior.write(&ctx.paths.state).unwrap();

        let state = update_extension(&env, &ctx).unwrap();

        assert!(
            env.install_calls
                .borrow()
                .iter()
                .any(|c| c == &vsix.to_string_lossy()),
            "force must reinstall even though the sha already matched"
        );
        assert_eq!(state.ext_vsix_sha256, sha);
    }

    #[test]
    fn verify_op_runs_all_seven_checks() {
        let dir = temp_dir("verify-op");
        let manifest = test_manifest();
        let vsix = write_vsix(&dir, b"vsix contents");
        let ctx = make_ctx(&dir, &manifest, &vsix);
        let env = FakeEnvironment::new(&ctx.code_candidates[0], &vsix);

        let results = verify(&env, &ctx);

        assert_eq!(results.len(), 7);
    }

    #[test]
    fn uninstall_op_routes_through_the_real_uninstaller() {
        let dir = temp_dir("uninstall-op");
        let manifest = test_manifest();
        let vsix = write_vsix(&dir, b"vsix contents");
        let ctx = make_ctx(&dir, &manifest, &vsix);
        let env = FakeEnvironment::new(&ctx.code_candidates[0], &vsix);
        // no state.json at all -> the fail-closed "missing" path (proceeds,
        // nothing to attribute), proving this op actually calls through to
        // uninstall::uninstall rather than reimplementing anything.
        let outcome = uninstall(&env, &ctx, &UninstallFlags::default());
        match outcome {
            UninstallOutcome::Finished {
                profile_removed, ..
            } => assert!(!profile_removed),
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn diagnostics_bundle_contains_expected_entries() {
        let dir = temp_dir("diagnostics");
        let manifest = test_manifest();
        let vsix = write_vsix(&dir, b"vsix contents");
        let ctx = make_ctx(&dir, &manifest, &vsix);
        std::fs::create_dir_all(&ctx.paths.logs).unwrap();
        std::fs::write(ctx.paths.logs.join("install.log"), b"log contents").unwrap();
        std::fs::create_dir_all(&ctx.paths.blk).unwrap();
        State::default().write(&ctx.paths.state).unwrap();

        let zip_path = dir.join("diagnostics.zip");
        diagnostics(&ctx, &zip_path).unwrap();

        let file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"logs/install.log".to_string()), "{names:?}");
        assert!(names.contains(&"state.json".to_string()), "{names:?}");
        assert!(names.contains(&"manifest.json".to_string()), "{names:?}");
        assert!(names.contains(&"facts.json".to_string()), "{names:?}");

        let mut facts_file = archive.by_name("facts.json").unwrap();
        let mut facts_content = String::new();
        std::io::Read::read_to_string(&mut facts_file, &mut facts_content).unwrap();
        let facts: serde_json::Value = serde_json::from_str(&facts_content).unwrap();
        assert_eq!(facts["os"], "macos");
        assert_eq!(facts["arch"], "arm64");
    }
}
