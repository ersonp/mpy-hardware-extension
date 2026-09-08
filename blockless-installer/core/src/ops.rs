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
use tracing::{info, warn};

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
    /// Checked upfront, before step 1 runs: without it, a run can seed or
    /// adopt a profile in step 1/2 and only then fail in extensions.rs's own
    /// `MissingBundledVsix`, permanently misattributing `profileCreatedByUs`
    /// (or leaving VS Code installed) for a run that could never have
    /// finished. M0 checks this before doing anything too.
    #[error("no bundled VSIX at {0}; pass --vsix")]
    MissingVsix(PathBuf),
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

/// `VscodeStepOutcome::product_version_mismatch`'s own doc says "logged by
/// the caller, never a failure" -- this is that logging, shared by
/// `install` and `repair` (the two callers of step 1).
fn log_version_mismatch(op: &str, outcome: &vscode::VscodeStepOutcome) {
    if let Some(api) = &outcome.product_version_mismatch {
        info!(
            installed = %outcome.product_version,
            api_reported = %api,
            "{op}: step 1 installed version differs from the update API"
        );
    }
}

/// Checked before any step runs in ops that touch extensions
/// (`install`/`repair`/`update_extension`): our extension only ever installs
/// from this bundled VSIX (never the Marketplace, see `extensions.rs`), so a
/// missing one can never let the run finish. Failing here -- before step 1
/// -- rather than inside `extensions.rs`'s own `MissingBundledVsix` avoids a
/// run that seeds or adopts a profile (and possibly installs VS Code) in
/// steps 1/2, only to fail with no path forward except a full re-run.
fn require_vsix(ctx: &OpsContext) -> Result<(), OpsError> {
    match ctx.vsix_path.as_deref() {
        Some(p) if p.exists() => Ok(()),
        _ => Err(OpsError::MissingVsix(
            ctx.vsix_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("(none supplied)")),
        )),
    }
}

/// Steps 1, 2, 4 (never 3/runtime) -- ports `repair`'s scope exactly:
/// detect-skip-do on VS Code, the extension, and settings.
pub fn repair(env: &dyn Environment, ctx: &OpsContext) -> Result<State, OpsError> {
    info!("repair: starting");
    require_vsix(ctx)?;
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
    )
    .inspect_err(|e| warn!(error = %e, "repair: step 1 (vscode) failed"))?;
    current.product_version = vscode_outcome.product_version.clone();
    current.vscode_installed_by_us = seed.vscode_installed_by_us || vscode_outcome.installed_by_us;
    current.steps.vscode = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;
    log_version_mismatch("repair", &vscode_outcome);
    info!(
        skipped = !vscode_outcome.installed_by_us,
        "repair: step 1 (vscode) done"
    );

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
        &ctx.manifest.components.extension.sha256,
        &seed.prior_ext_vsix_sha256,
        seed.profile_created_by_us,
        false,
    )
    .inspect_err(|e| warn!(error = %e, "repair: step 2 (extension) failed"))?;
    current.profile_created_by_us = ext_outcome.profile_created_by_us;
    current.ext_vsix_sha256 = ext_outcome.ext_vsix_sha256;
    current.steps.extension = true;
    if let Some(loc) =
        profile::resolve_profile_location(&ctx.paths.storage, &ctx.manifest.profile_name)
    {
        current.profile_location = loc;
    }
    stamp_and_write(&mut current, &ctx.paths.state)?;
    info!("repair: step 2 (extension) done");

    let settings_outcome = settings::ensure_settings(
        env,
        &vscode_outcome.code_cli,
        &ctx.paths.storage,
        &ctx.profiles_dir(),
        &ctx.manifest.profile_name,
        "blockless",
        &ctx.paths.env_python,
        &ctx.manifest.settings,
    )
    .inspect_err(|e| warn!(error = %e, "repair: step 4 (settings) failed"))?;
    current.profile_location = settings_outcome.profile_location;
    current.settings_mechanism = "A".to_string();
    current.steps.settings = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;
    info!(
        applied = settings_outcome.applied,
        "repair: step 4 (settings) done, repair finished"
    );

    Ok(current)
}

/// `install` = `repair` (steps 1, 2, 4) plus step 3 (runtime) plus the final
/// foreground open. Steps run in the M0 order (1, 2, 3, 4), not repair's
/// (1, 2, 4) then 3 tacked on, so a fresh machine's env_python already
/// exists by the time step 4 writes `mpyhw.pythonPath`.
pub fn install(env: &dyn Environment, ctx: &OpsContext) -> Result<State, OpsError> {
    info!("install: starting");
    require_vsix(ctx)?;
    let prior = read_prior_state_lenient(&ctx.paths.state);
    let seed = state::seed_from_prior(prior.as_ref(), "blockless");
    let mut current = prior.unwrap_or_default();
    // `install` always re-attempts all four steps, unlike `repair`/
    // `repair_runtime` which intentionally touch only a subset -- so unlike
    // those, a prior journal's step flags carry no meaning for THIS run and
    // must not survive into it. Without this reset, an incremental write
    // from an early step (still holding the stale prior flags for steps not
    // yet reached) can leave e.g. a stale `steps.python: true` on disk if
    // this run then dies before actually re-verifying that step.
    current.steps = state::Steps::default();
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
    )
    .inspect_err(|e| warn!(error = %e, "install: step 1 (vscode) failed"))?;
    current.product_version = vscode_outcome.product_version.clone();
    current.vscode_installed_by_us = seed.vscode_installed_by_us || vscode_outcome.installed_by_us;
    current.steps.vscode = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;
    log_version_mismatch("install", &vscode_outcome);
    info!(
        skipped = !vscode_outcome.installed_by_us,
        "install: step 1 (vscode) done"
    );

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
        &ctx.manifest.components.extension.sha256,
        &seed.prior_ext_vsix_sha256,
        seed.profile_created_by_us,
        false,
    )
    .inspect_err(|e| warn!(error = %e, "install: step 2 (extension) failed"))?;
    current.profile_created_by_us = ext_outcome.profile_created_by_us;
    current.ext_vsix_sha256 = ext_outcome.ext_vsix_sha256;
    current.steps.extension = true;
    if let Some(loc) =
        profile::resolve_profile_location(&ctx.paths.storage, &ctx.manifest.profile_name)
    {
        current.profile_location = loc;
    }
    stamp_and_write(&mut current, &ctx.paths.state)?;
    info!("install: step 2 (extension) done");

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
    )
    .inspect_err(|e| warn!(error = %e, "install: step 3 (runtime) failed"))?;
    current.steps.python = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;
    info!("install: step 3 (runtime) done");

    let settings_outcome = settings::ensure_settings(
        env,
        &vscode_outcome.code_cli,
        &ctx.paths.storage,
        &ctx.profiles_dir(),
        &ctx.manifest.profile_name,
        "blockless",
        &ctx.paths.env_python,
        &ctx.manifest.settings,
    )
    .inspect_err(|e| warn!(error = %e, "install: step 4 (settings) failed"))?;
    current.profile_location = settings_outcome.profile_location;
    current.settings_mechanism = "A".to_string();
    current.steps.settings = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;
    info!(
        applied = settings_outcome.applied,
        "install: step 4 (settings) done"
    );

    // Final: foreground open into the profile. Every earlier step that
    // spawned a child of its own (register_profile's window fallback) has
    // already closed it before returning, so this is always a fresh
    // extension host -- never an attach to a lingering child, never the
    // user's own session.
    if let Err(e) = env.spawn(
        &vscode_outcome.code_cli,
        &["--profile", &ctx.manifest.profile_name, "--new-window"],
    ) {
        warn!(error = %e, "install: final foreground open failed to spawn");
    }
    info!("install: finished");

    Ok(current)
}

/// Remove `env/` first, then re-run step 3 -- deterministic rather than
/// trusting `uv`'s own detect-and-reuse over a possibly-broken existing
/// venv. Steps 1/2/4 are untouched.
pub fn repair_runtime(env: &dyn Environment, ctx: &OpsContext) -> Result<State, OpsError> {
    info!("repair-runtime: starting");
    let prior = read_prior_state_lenient(&ctx.paths.state);
    let mut current = prior.unwrap_or_default();

    let env_dir = ctx.paths.blk.join("env");
    if env_dir.exists() {
        env.remove_dir_all(&env_dir)
            .map_err(|reason| OpsError::RemoveEnv {
                path: env_dir.clone(),
                reason,
            })
            .inspect_err(|e| warn!(error = %e, "repair-runtime: could not remove env/"))?;
        info!("repair-runtime: removed existing env/");
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
    )
    .inspect_err(|e| warn!(error = %e, "repair-runtime: step 3 (runtime) failed"))?;
    current.mpremote_version = ctx.manifest.components.mpremote.version.clone();
    current.env_python = ctx.paths.env_python.to_string_lossy().into_owned();
    current.steps.python = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;
    info!("repair-runtime: step 3 (runtime) done");

    Ok(current)
}

/// Force-reinstall the bundled VSIX regardless of the sha-match skip, and
/// re-journal the sha. Steps 1/3/4 are untouched.
pub fn update_extension(env: &dyn Environment, ctx: &OpsContext) -> Result<State, OpsError> {
    info!("update-extension: starting");
    require_vsix(ctx)?;
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
        &ctx.manifest.components.extension.sha256,
        &seed.prior_ext_vsix_sha256,
        seed.profile_created_by_us,
        true, // force: bypass the sha-match skip
    )
    .inspect_err(|e| warn!(error = %e, "update-extension: step 2 (extension) failed"))?;
    current.profile_created_by_us = ext_outcome.profile_created_by_us;
    current.ext_vsix_sha256 = ext_outcome.ext_vsix_sha256;
    current.steps.extension = true;
    stamp_and_write(&mut current, &ctx.paths.state)?;
    info!("update-extension: step 2 (extension) done");

    Ok(current)
}

/// Bundles `logs/` + `state.json` + the resolved manifest + OS/arch/version
/// facts into a zip at `target_zip`. Read-only, like `verify`: nothing here
/// mutates the machine, and no data leaves it unless the caller exports the
/// zip themselves (ARCHITECTURE §9).
pub fn diagnostics(ctx: &OpsContext, target_zip: &Path) -> Result<(), OpsError> {
    info!(target = %target_zip.display(), "diagnostics: starting");
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
    info!("diagnostics: finished");
    Ok(())
}

pub fn verify(env: &dyn Environment, ctx: &OpsContext) -> Vec<verify::CheckResult> {
    info!("verify: starting");
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
    let results = verify::run_checks(env, env, env, &inputs);
    let failed = results.iter().filter(|r| !r.pass).count();
    if failed == 0 {
        info!(checks = results.len(), "verify: all checks passed");
    } else {
        warn!(checks = results.len(), failed, "verify: some checks failed");
    }
    results
}

pub fn uninstall(
    env: &dyn Environment,
    ctx: &OpsContext,
    flags: &UninstallFlags,
) -> UninstallOutcome {
    info!(
        all = flags.all,
        keep_vscode = flags.keep_vscode,
        "uninstall: starting"
    );
    let vscode_dirs = vscode_install_locations(ctx);
    let outcome = uninstall::uninstall(
        env,
        env,
        &ctx.paths.state,
        &ctx.paths.storage,
        &ctx.profiles_dir(),
        &ctx.manifest.profile_name,
        &ctx.paths.blk,
        &vscode_dirs,
        flags,
    );
    info!(outcome = ?outcome, "uninstall: finished");
    outcome
}

/// Every location this OS's install could plausibly have put VS Code, not
/// just whichever candidate happens to be runnable right now -- a broken or
/// stale `code` CLI must not leave a real install undetected by uninstall.
/// Mac: every `mac_install_targets` entry joined with the app bundle name
/// directly (matches `vscode.rs`'s own writability-fallback candidates
/// one-to-one, independent of runnability -- up to two real locations).
/// Windows: every `code_candidates` entry mapped through `vscode_app_root`
/// unconditionally, the same way -- there is only ever one Windows install
/// location, but it must not depend on `--version` succeeding either, or a
/// corrupt/half-deleted install with `vscodeInstalledByUs: true` is silently
/// left behind.
fn vscode_install_locations(ctx: &OpsContext) -> Vec<PathBuf> {
    match ctx.os {
        Os::MacOs => ctx
            .mac_install_targets
            .iter()
            .map(|t| t.join("Visual Studio Code.app"))
            .collect(),
        Os::Windows => ctx
            .code_candidates
            .iter()
            .filter_map(|c| vscode_app_root(ctx.os, c))
            .collect(),
    }
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
        run_uv_fails: RefCell<bool>,
        remove_dir_calls: RefCell<Vec<PathBuf>>,
        spawn_calls: RefCell<Vec<Vec<String>>>,
        /// Overridable so a test can simulate "VS Code is already running"
        /// (e.g. to force `register_profile_offline` to skip seeding
        /// `storage.json`, leaving `resolve_profile_location` unable to
        /// resolve anything this run).
        running_pids: RefCell<Vec<u32>>,
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
                run_uv_fails: RefCell::new(false),
                remove_dir_calls: RefCell::new(Vec::new()),
                spawn_calls: RefCell::new(Vec::new()),
                running_pids: RefCell::new(Vec::new()),
            }
        }
    }

    impl profile::CommandRunner for FakeEnvironment {
        fn running_vscode_pids(&self) -> Vec<u32> {
            self.running_pids.borrow().clone()
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
            if *self.run_uv_fails.borrow() {
                return false;
            }
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

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The committed manifest's `extension.sha256` is an all-zero placeholder
    /// (the real VSIX's hash is unknown at commit time). Tests that exercise
    /// a real install path need a manifest that actually matches their own
    /// fixture VSIX, or `ensure_extensions`'s authenticity check (the
    /// bundled VSIX's hash must match the manifest's declared value, not
    /// just the journaled prior sha) correctly refuses every one of them.
    fn test_manifest_matching(vsix_bytes: &[u8]) -> Manifest {
        let mut manifest = test_manifest();
        manifest.components.extension.sha256 = sha256_hex(vsix_bytes);
        manifest
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
        let manifest = test_manifest_matching(b"vsix contents");
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

    /// A `tracing` writer that captures formatted output into a shared
    /// buffer, so a test can assert on log content.
    #[derive(Clone, Default)]
    struct CapturingWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The one `tracing` subscriber every log-capturing test in this module
    /// shares, installed at most once process-wide.
    ///
    /// `cargo test`'s default parallel runner has many OTHER tests calling
    /// `install`/`repair` concurrently on other threads, sharing these same
    /// `info!`/`warn!` callsites. A THREAD-LOCAL subscriber
    /// (`tracing::subscriber::with_default`) loses this race: a callsite's
    /// process-wide interest is cached on first-ever use, and a concurrent
    /// thread still running under the no-op default can win that race and
    /// cache it "not interested" out from under a test -- empirically,
    /// roughly 1 run in 3 under the full suite. A single global default
    /// instead makes every callsite's interest resolve once, globally, with
    /// no thread-local toggling and thus no window for the race.
    ///
    /// Only the FIRST caller's `try_init` actually succeeds (global default
    /// can only be set once per process); every caller gets back a clone of
    /// the SAME shared writer regardless of which one won, via `OnceLock`,
    /// so which log-capturing test happens to run first doesn't matter --
    /// they all observe the one real subscriber. Other, non-capturing
    /// parallel tests free-ride on it harmlessly (their lines just add
    /// noise a `contains` check ignores).
    fn capturing_log_writer() -> CapturingWriter {
        static WRITER: std::sync::OnceLock<CapturingWriter> = std::sync::OnceLock::new();
        WRITER
            .get_or_init(|| {
                let writer = CapturingWriter::default();
                let _ = tracing_subscriber::fmt()
                    .with_writer(writer.clone())
                    .with_ansi(false)
                    .try_init();
                writer
            })
            .clone()
    }

    #[test]
    fn install_logs_every_step() {
        // Proves ops.rs actually emits a log line per step (the reviewer's
        // finding: nothing logged, so diagnostics bundled an empty logs/),
        // not just that the tracing macro calls compile.
        let writer = capturing_log_writer();

        let dir = temp_dir("install-logs");
        let manifest = test_manifest_matching(b"vsix contents");
        let vsix = write_vsix(&dir, b"vsix contents");
        let ctx = make_ctx(&dir, &manifest, &vsix);
        let env = FakeEnvironment::new(&ctx.code_candidates[0], &vsix);

        let before = writer.0.lock().unwrap().len();
        install(&env, &ctx).unwrap();
        let logged = String::from_utf8(writer.0.lock().unwrap()[before..].to_vec()).unwrap();
        for expected in [
            "install: starting",
            "install: step 1 (vscode) done skipped=true",
            "install: step 2 (extension) done",
            "install: step 3 (runtime) done",
            "install: step 4 (settings) done applied=true",
            "install: finished",
        ] {
            assert!(
                logged.contains(expected),
                "missing {expected:?} in:\n{logged}"
            );
        }
    }

    #[test]
    fn repair_never_touches_runtime() {
        let dir = temp_dir("repair-no-runtime");
        let manifest = test_manifest_matching(b"vsix contents");
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
        let manifest = test_manifest_matching(b"vsix contents");
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
    fn install_resets_stale_steps_so_an_aborted_run_never_journals_a_step_it_never_reverified() {
        // A prior COMPLETE install left every step true. If this run then
        // dies during step 3 (runtime), the incremental writes from steps 1
        // and 2 (persisted before step 3 is even attempted) must not carry
        // the stale `steps.python: true` forward -- otherwise `verify`'s
        // check 5 would pass against a runtime this run never actually
        // re-confirmed.
        let dir = temp_dir("install-resets-steps");
        let manifest = test_manifest_matching(b"vsix contents");
        let vsix = write_vsix(&dir, b"vsix contents");
        let ctx = make_ctx(&dir, &manifest, &vsix);
        std::fs::create_dir_all(&ctx.paths.blk).unwrap();
        let prior = State {
            steps: crate::state::Steps {
                vscode: true,
                extension: true,
                python: true,
                settings: true,
            },
            ..Default::default()
        };
        prior.write(&ctx.paths.state).unwrap();
        let env = FakeEnvironment::new(&ctx.code_candidates[0], &vsix);
        // force step 3 to fail so the run dies before its own
        // `current.steps.python = true` write -- only the STALE prior value
        // could end up on disk at that point if the reset didn't happen.
        *env.mpremote_version.borrow_mut() = None;
        *env.run_uv_fails.borrow_mut() = true;

        let writer = capturing_log_writer();
        let before = writer.0.lock().unwrap().len();
        let result = install(&env, &ctx);
        let logged = String::from_utf8(writer.0.lock().unwrap()[before..].to_vec()).unwrap();
        assert!(
            logged.contains("install: step 3 (runtime) failed"),
            "the failure warn! line, with its error field, must reach the \
             logs a real run's diagnostics bundle would export; got:\n{logged}"
        );

        assert!(result.is_err(), "test setup: step 3 must actually fail");
        let persisted = State::read(&ctx.paths.state).unwrap().unwrap();
        assert!(
            persisted.steps.vscode && persisted.steps.extension,
            "steps this run genuinely completed must still be journaled true"
        );
        assert!(
            !persisted.steps.python,
            "steps.python must not carry forward stale from the prior \
             journal when this run never got to re-verify it"
        );
    }

    #[test]
    fn install_against_the_unmodified_committed_manifest_refuses_the_real_vsix() {
        // The committed manifest's `extension.sha256` is a deliberate
        // all-zero placeholder (the real VSIX's hash is unknown at commit
        // time; the real value is stamped in outside this repo, before a
        // real rig run, by
        // `mpy-hardware-extension/scripts/stamp-installer-manifest.mjs`).
        // This is a WIRING test, not a placeholder canary by itself: it
        // proves `ops.rs` actually passes
        // `manifest.components.extension.sha256` through to
        // `ensure_extensions` end to end (a fixture vsix, `b"vsix
        // contents"`, would refuse against ANY manifest sha it doesn't
        // equal -- placeholder or a real one that just doesn't match this
        // fixture). `manifest::tests::committed_manifest_sha256_pins_are_the_documented_values`
        // is the actual canary that fails the moment the pin gets stamped
        // without a matching test update.
        let dir = temp_dir("unmodified-manifest-refuses-real-vsix");
        let manifest = test_manifest(); // NOT test_manifest_matching -- the real, unmodified file
        let vsix = write_vsix(&dir, b"vsix contents");
        let ctx = make_ctx(&dir, &manifest, &vsix);
        let env = FakeEnvironment::new(&ctx.code_candidates[0], &vsix);

        let err = install(&env, &ctx).unwrap_err();

        assert!(
            matches!(
                err,
                OpsError::Extensions(ExtensionsError::VsixShaMismatch { .. })
            ),
            "expected VsixShaMismatch against the placeholder sha, got {err:?}"
        );
    }

    #[test]
    fn install_refuses_a_missing_vsix_before_any_step_runs() {
        let dir = temp_dir("missing-vsix-upfront");
        let manifest = test_manifest_matching(b"vsix contents");
        let missing = dir.join("does-not-exist.vsix");
        let ctx = make_ctx(&dir, &manifest, &missing);
        let env = FakeEnvironment::new(&ctx.code_candidates[0], &missing);

        let err = install(&env, &ctx).unwrap_err();

        assert!(matches!(err, OpsError::MissingVsix(p) if p == missing));
        assert!(
            env.spawn_calls.borrow().is_empty(),
            "must never spawn/register a profile when the vsix is missing"
        );
        assert!(
            !ctx.paths.state.exists(),
            "must never journal any step (not even step 1) before the vsix check"
        );
    }

    #[test]
    fn repair_and_update_extension_also_refuse_a_missing_vsix_upfront() {
        let dir = temp_dir("missing-vsix-upfront-others");
        let manifest = test_manifest_matching(b"vsix contents");
        let missing = dir.join("does-not-exist.vsix");
        let ctx = make_ctx(&dir, &manifest, &missing);
        let env = FakeEnvironment::new(&ctx.code_candidates[0], &missing);

        assert!(matches!(
            repair(&env, &ctx).unwrap_err(),
            OpsError::MissingVsix(p) if p == missing
        ));
        assert!(matches!(
            update_extension(&env, &ctx).unwrap_err(),
            OpsError::MissingVsix(p) if p == missing
        ));
    }

    type OpFn = fn(&dyn Environment, &OpsContext) -> Result<State, OpsError>;

    /// `repair` and `install` both re-derive `vscode_installed_by_us` and
    /// `profile_location` with the byte-identical carry-forward expressions
    /// (`ops.rs`'s repair/install bodies are intentionally parallel) -- a
    /// round-2 review proved by mutation that testing only `repair` left
    /// `install`'s copy of each expression uncovered (both mutations shipped
    /// silently under the full suite). Every invariant test below runs
    /// against both ops, not just one.
    const CARRY_FORWARD_OPS: [(&str, OpFn); 2] = [("repair", repair), ("install", install)];

    #[test]
    fn vscode_installed_by_us_stays_sticky_true_across_a_skip_run() {
        // Mutation-tested against the reviewer's own finding: replacing
        // `seed.vscode_installed_by_us || vscode_outcome.installed_by_us`
        // with just `vscode_outcome.installed_by_us` left every existing
        // test green, because none of them seeded a prior journal with the
        // flag already true. This run's own step 1 detects VS Code already
        // present (a skip -- `installed_by_us` is false for THIS run), so
        // only the sticky carry-forward from the prior journal can be what
        // keeps the flag true.
        for (op_name, op) in CARRY_FORWARD_OPS {
            let dir = temp_dir(&format!("vscode-sticky-{op_name}"));
            let manifest = test_manifest_matching(b"vsix contents");
            let vsix = write_vsix(&dir, b"vsix contents");
            let ctx = make_ctx(&dir, &manifest, &vsix);
            std::fs::create_dir_all(&ctx.paths.blk).unwrap();
            let prior = State {
                vscode_installed_by_us: true,
                ..Default::default()
            };
            prior.write(&ctx.paths.state).unwrap();
            let env = FakeEnvironment::new(&ctx.code_candidates[0], &vsix);

            let state = op(&env, &ctx).unwrap();

            assert!(
                state.vscode_installed_by_us,
                "{op_name}: a prior true must survive a run whose own step 1 was a skip"
            );
        }
    }

    #[test]
    fn profile_location_survives_step_2_when_storage_json_never_resolves() {
        // Mutation-tested against the reviewer's own finding: replacing the
        // guarded `if let Some(loc) = resolve_profile_location(..) { current
        // .profile_location = loc }` (right after step 2) with an
        // unconditional `.unwrap_or_default()` (blanking a known-good id
        // whenever resolution fails) left every existing test green too,
        // because none of them exercised a run where step 2 completes
        // without ever producing a resolvable storage.json entry.
        //
        // Simulated by reporting VS Code as already running: extensions.rs's
        // `register_profile_offline` then skips seeding storage.json, and
        // since this fake's extension installs always succeed, the
        // window-registration fallback (which would otherwise create the
        // entry) never triggers either -- so storage.json stays entryless
        // through the point ops.rs's step-2 carry-forward check runs.
        //
        // The run as a whole still fails (settings.rs's OWN fallback also
        // can't resolve a profile against a fake that never actually writes
        // storage.json), so this asserts against the state.json PERSISTED
        // right after step 2 -- exactly the incremental write the carry-
        // forward check protects -- rather than repair()'s return value.
        for (op_name, op) in CARRY_FORWARD_OPS {
            let dir = temp_dir(&format!("profile-location-survives-{op_name}"));
            let manifest = test_manifest_matching(b"vsix contents");
            let vsix = write_vsix(&dir, b"vsix contents");
            let ctx = make_ctx(&dir, &manifest, &vsix);
            std::fs::create_dir_all(&ctx.paths.blk).unwrap();
            let prior = State {
                profile_location: "a1b2c3d4e5f6".to_string(),
                ..Default::default()
            };
            prior.write(&ctx.paths.state).unwrap();
            let env = FakeEnvironment::new(&ctx.code_candidates[0], &vsix);
            *env.running_pids.borrow_mut() = vec![4242];

            let result = op(&env, &ctx);

            assert!(
                result.is_err(),
                "{op_name}: expected settings.rs to also fail to resolve a profile \
                 against a fake that never writes storage.json (test setup check)"
            );
            assert!(
                !ctx.paths.storage.exists(),
                "{op_name}: storage.json must never have been written before step 2's \
                 incremental write (test setup check)"
            );
            let persisted = State::read(&ctx.paths.state).unwrap().unwrap();
            assert_eq!(
                persisted.profile_location, "a1b2c3d4e5f6",
                "{op_name}: a known-good prior location must survive step 2's incremental \
                 write when step 2 never resolved a fresh one"
            );
        }
    }

    #[test]
    fn ext_vsix_sha256_survives_step_1_when_step_2_never_completes() {
        // The third carry-forward invariant `scope.md`'s review focus names
        // alongside `vscodeInstalledByUs`/`profileLocation`
        // (`extVsixSha256`) had zero ops-level coverage: `current =
        // prior.unwrap_or_default()` carries it through every incremental
        // write until step 2 explicitly overwrites it, but nothing asserted
        // that a run whose step 2 never REACHES that overwrite still leaves
        // the prior value on disk after step 1's write. Forced here by
        // using the real (unmodified, placeholder-sha) committed manifest
        // against a fixture vsix that can never match it: `ensure_extensions`
        // refuses with `VsixShaMismatch` before touching
        // `ext_vsix_sha256` at all, so step 1's persisted state is the last
        // word.
        for (op_name, op) in CARRY_FORWARD_OPS {
            let dir = temp_dir(&format!("ext-sha-survives-step1-{op_name}"));
            let manifest = test_manifest(); // NOT test_manifest_matching -- placeholder sha
            let vsix = write_vsix(&dir, b"vsix contents");
            let ctx = make_ctx(&dir, &manifest, &vsix);
            std::fs::create_dir_all(&ctx.paths.blk).unwrap();
            let prior = State {
                ext_vsix_sha256: "deadbeef".to_string(),
                ..Default::default()
            };
            prior.write(&ctx.paths.state).unwrap();
            let env = FakeEnvironment::new(&ctx.code_candidates[0], &vsix);

            let err = op(&env, &ctx).unwrap_err();

            assert!(
                matches!(
                    err,
                    OpsError::Extensions(ExtensionsError::VsixShaMismatch { .. })
                ),
                "{op_name}: expected step 2 to refuse before touching ext_vsix_sha256 \
                 (test setup check), got {err:?}"
            );
            let persisted = State::read(&ctx.paths.state).unwrap().unwrap();
            assert_eq!(
                persisted.ext_vsix_sha256, "deadbeef",
                "{op_name}: a known-good prior sha must survive step 1's incremental \
                 write when step 2 never reached its own overwrite"
            );
        }
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
    fn windows_uninstall_locations_include_a_candidate_that_fails_version() {
        // Mirrors the reviewer's finding: `vscode_install_locations` must not
        // filter Windows candidates through `env.version(c).is_some()` --
        // that only finds a RUNNABLE `code.cmd`, so a corrupt or
        // half-deleted install (candidate path exists, `--version` no longer
        // works) would be silently skipped, the same class already fixed for
        // mac.
        let dir = temp_dir("windows-locations");
        let manifest = test_manifest();
        let vsix = write_vsix(&dir, b"vsix contents");
        let mut ctx = make_ctx(&dir, &manifest, &vsix);
        ctx.os = Os::Windows;
        let code_cli = dir
            .join("LOCALAPPDATA")
            .join("Programs")
            .join("Microsoft VS Code")
            .join("bin")
            .join("code.cmd");
        ctx.code_candidates = vec![code_cli];
        ctx.mac_install_targets = vec![];

        let locations = vscode_install_locations(&ctx);

        assert_eq!(
            locations,
            vec![dir
                .join("LOCALAPPDATA")
                .join("Programs")
                .join("Microsoft VS Code")],
            "the Windows candidate must be included even though no \
             Environment was consulted about its runnability"
        );
    }

    #[test]
    fn windows_uninstall_removes_a_broken_install_version_check_fails() {
        // End-to-end: `ops::uninstall` on Windows, with `vscodeInstalledByUs`
        // true and the install directory present on disk, but the `code` CLI
        // no longer runs (`version()` returns `None`) -- the install must
        // still be found and removed, not silently left behind with
        // `vscode_removed: false`.
        let dir = temp_dir("windows-uninstall-broken");
        let manifest = test_manifest();
        let vsix = write_vsix(&dir, b"vsix contents");
        let mut ctx = make_ctx(&dir, &manifest, &vsix);
        ctx.os = Os::Windows;
        let vscode_dir = dir
            .join("LOCALAPPDATA")
            .join("Programs")
            .join("Microsoft VS Code");
        let code_cli = vscode_dir.join("bin").join("code.cmd");
        ctx.code_candidates = vec![code_cli.clone()];
        ctx.mac_install_targets = vec![];
        std::fs::create_dir_all(&vscode_dir).unwrap();

        let state = State {
            vscode_installed_by_us: true,
            profile_created_by_us: false,
            ..Default::default()
        };
        state.write(&ctx.paths.state).unwrap();

        let env = FakeEnvironment::new(&code_cli, &vsix);
        *env.vscode_version.borrow_mut() = None; // the broken CLI: --version fails

        let outcome = uninstall(&env, &ctx, &UninstallFlags::default());

        match outcome {
            UninstallOutcome::Finished { vscode_removed, .. } => {
                assert!(vscode_removed, "a broken-CLI install must still be removed")
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert!(!vscode_dir.exists());
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
