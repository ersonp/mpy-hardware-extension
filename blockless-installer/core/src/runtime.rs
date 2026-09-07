//! Step 3: a contained Python + mpremote, provisioned entirely through `uv`.
//! Ports `step3_python`/`Step-Python`.
//!
//! Diverges from M0 on purpose (`/scope.md` "uv delivery"): M0 fetches the
//! `astral.sh` install SCRIPT (sha-pinned at the script level); this core
//! pins the uv BINARY itself, downloaded straight from a GitHub release
//! asset and sha256-verified against the manifest, then extracted. The
//! astral.sh script path does not carry over.
//!
//! No system Python is ever invoked: the venv is always built with
//! `--managed-python --python <series>` against the interpreter uv itself
//! just installed under `BLK/python`, and every subsequent operation targets
//! `ENVPY` (`BLK/env/...`) explicitly. `verify.rs` (commit 10) is the
//! acceptance-level guard for this invariant (`pyvenv.cfg`'s `home`
//! containment check); this module's contribution is simply never
//! constructing a path that could point anywhere else.

use crate::fetch::{self, FetchOptions};
use crate::manifest::{UvComponent, UvPlatformKey};
use crate::platform::{Arch, Os};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("uv download failed: {0}")]
    Download(#[from] fetch::FetchError),
    #[error("could not extract uv: {0}")]
    Extract(String),
    #[error("uv not runnable after install")]
    UvNotRunnableAfterInstall,
    #[error("uv {0} failed")]
    UvCommandFailed(String),
    #[error("mpremote verify failed after install")]
    MpremoteVerifyFailed,
}

/// `uv`/mpremote operations this step needs, injected so it is unit-testable
/// without a real `uv` binary or a real Python. Production impl lands with
/// `ops.rs`.
pub trait RuntimeRunner {
    /// `<envpy> -m mpremote version`. `None` if `envpy` doesn't exist or
    /// isn't runnable.
    fn mpremote_version(&self, envpy: &Path) -> Option<String>;
    /// `<uv_bin> --version`. `None` if not executable.
    fn uv_version(&self, uv_bin: &Path) -> Option<String>;
    /// Extract the downloaded uv release archive into `dest_dir` (mac:
    /// `tar -xzf`; Windows: `Expand-Archive`).
    fn extract_uv(&self, archive: &Path, dest_dir: &Path) -> Result<(), String>;
    /// Run `<uv_bin> <args>` with `env` set in addition to the inherited
    /// environment. `true` on success.
    fn run_uv(&self, uv_bin: &Path, args: &[&str], env: &[(&str, &str)]) -> bool;
}

fn uv_binary_name(os: Os) -> &'static str {
    match os {
        Os::MacOs => "uv",
        Os::Windows => "uv.exe",
    }
}

fn uv_archive_name(os: Os) -> &'static str {
    match os {
        Os::MacOs => "uv.tar.gz",
        Os::Windows => "uv.zip",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStepOutcome {
    AlreadyPresent,
    Provisioned,
}

/// The full step: detect/skip on the pinned mpremote -> ensure uv (detect or
/// download+verify+extract) -> `uv python install` -> `uv venv` -> `uv pip
/// install mpremote==<pinned>` -> verify.
#[allow(clippy::too_many_arguments)]
pub fn ensure_runtime(
    runner: &dyn RuntimeRunner,
    client: &reqwest::blocking::Client,
    os: Os,
    arch: Arch,
    manifest_uv: &UvComponent,
    python_series: &str,
    mpremote_version: &str,
    blk: &Path,
    env_python: &Path,
    downloads_dir: &Path,
    fetch_opts: &FetchOptions,
) -> Result<RuntimeStepOutcome, RuntimeError> {
    if runner
        .mpremote_version(env_python)
        .is_some_and(|v| v.contains(mpremote_version))
    {
        return Ok(RuntimeStepOutcome::AlreadyPresent);
    }

    let uv_dir = blk.join("uv");
    let uv_bin = uv_dir.join(uv_binary_name(os));
    let uv_current = runner
        .uv_version(&uv_bin)
        .is_some_and(|v| v.contains(&manifest_uv.version));
    if !uv_current {
        let key = UvPlatformKey::for_target(os, arch);
        let archive_path = downloads_dir.join(uv_archive_name(os));
        fetch::fetch_and_verify(
            client,
            &manifest_uv.download_url(key),
            manifest_uv.sha256_for(key),
            &archive_path,
            fetch_opts,
        )?;
        runner
            .extract_uv(&archive_path, &uv_dir)
            .map_err(RuntimeError::Extract)?;
        if runner.uv_version(&uv_bin).is_none() {
            return Err(RuntimeError::UvNotRunnableAfterInstall);
        }
    }

    // Contained on purpose: the interpreter installs under BLK/python
    // (UV_PYTHON_INSTALL_DIR), and --managed-python forces the venv to build
    // on THAT interpreter. Without --managed-python, `uv venv --python
    // <series>` would match any discoverable interpreter (a dev's Anaconda,
    // a system python) and the env would silently depend on it.
    let python_install_dir = blk.join("python").to_string_lossy().into_owned();
    let env_dir = blk.join("env").to_string_lossy().into_owned();
    let env_python_str = env_python.to_string_lossy().into_owned();
    let uv_env = [("UV_PYTHON_INSTALL_DIR", python_install_dir.as_str())];

    if !runner.run_uv(
        &uv_bin,
        &["python", "install", "--no-bin", python_series],
        &uv_env,
    ) {
        return Err(RuntimeError::UvCommandFailed("python install".to_string()));
    }
    if !runner.run_uv(
        &uv_bin,
        &[
            "venv",
            &env_dir,
            "--managed-python",
            "--python",
            python_series,
        ],
        &uv_env,
    ) {
        return Err(RuntimeError::UvCommandFailed("venv".to_string()));
    }
    let mpremote_spec = format!("mpremote=={mpremote_version}");
    if !runner.run_uv(
        &uv_bin,
        &[
            "pip",
            "install",
            "--python",
            &env_python_str,
            &mpremote_spec,
        ],
        &uv_env,
    ) {
        return Err(RuntimeError::UvCommandFailed("pip install".to_string()));
    }

    if runner
        .mpremote_version(env_python)
        .is_some_and(|v| v.contains(mpremote_version))
    {
        Ok(RuntimeStepOutcome::Provisioned)
    } else {
        Err(RuntimeError::MpremoteVerifyFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "blockless-installer-runtime-test-{name}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manifest_uv() -> UvComponent {
        const MANIFEST: &str = include_str!("../../manifest/installer.manifest.json");
        Manifest::parse(MANIFEST).unwrap().components.uv
    }

    fn fast_opts() -> FetchOptions {
        FetchOptions {
            max_attempts: 1,
            backoff_base: Duration::from_millis(1),
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedCall {
        uv_bin: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
    }

    struct FakeRuntimeRunner {
        mpremote_version_result: RefCell<Option<String>>,
        uv_version_result: RefCell<Option<String>>,
        extract_uv_result: RefCell<Result<(), String>>,
        run_uv_result: RefCell<bool>,
        calls: RefCell<Vec<RecordedCall>>,
        /// After a successful `python install` + `venv` + `pip install`
        /// sequence, flips `mpremote_version_result` to this (simulating a
        /// real provision landing).
        version_after_provision: RefCell<Option<String>>,
    }

    impl FakeRuntimeRunner {
        fn new() -> FakeRuntimeRunner {
            FakeRuntimeRunner {
                mpremote_version_result: RefCell::new(None),
                uv_version_result: RefCell::new(None),
                extract_uv_result: RefCell::new(Ok(())),
                run_uv_result: RefCell::new(true),
                calls: RefCell::new(Vec::new()),
                version_after_provision: RefCell::new(None),
            }
        }
    }

    impl RuntimeRunner for FakeRuntimeRunner {
        fn mpremote_version(&self, _envpy: &Path) -> Option<String> {
            self.mpremote_version_result.borrow().clone()
        }
        fn uv_version(&self, _uv_bin: &Path) -> Option<String> {
            self.uv_version_result.borrow().clone()
        }
        fn extract_uv(&self, _archive: &Path, _dest_dir: &Path) -> Result<(), String> {
            let r = self.extract_uv_result.borrow().clone();
            if r.is_ok() {
                *self.uv_version_result.borrow_mut() = Some("0.11.29".to_string());
            }
            r
        }
        fn run_uv(&self, uv_bin: &Path, args: &[&str], env: &[(&str, &str)]) -> bool {
            self.calls.borrow_mut().push(RecordedCall {
                uv_bin: uv_bin.to_path_buf(),
                args: args.iter().map(|s| s.to_string()).collect(),
                env: env
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            });
            let ok = *self.run_uv_result.borrow();
            if ok && self.calls.borrow().len() == 3 {
                // the third call is always pip install mpremote in this
                // step's fixed sequence
                if let Some(v) = self.version_after_provision.borrow().clone() {
                    *self.mpremote_version_result.borrow_mut() = Some(v);
                }
            }
            ok
        }
    }

    fn mac_blk() -> PathBuf {
        PathBuf::from("/Users/erson/Library/Application Support/Blockless")
    }
    fn mac_envpy(blk: &Path) -> PathBuf {
        blk.join("env").join("bin").join("python")
    }
    fn win_blk() -> PathBuf {
        PathBuf::from(r"C:\Users\erson\AppData\Local\Blockless")
    }
    fn win_envpy(blk: &Path) -> PathBuf {
        blk.join("env").join("Scripts").join("python.exe")
    }

    #[test]
    fn detect_skip_when_pinned_mpremote_already_present() {
        let runner = FakeRuntimeRunner::new();
        *runner.mpremote_version_result.borrow_mut() = Some("mpremote 1.28.0".to_string());
        let client = reqwest::blocking::Client::new();
        let blk = mac_blk();
        let envpy = mac_envpy(&blk);
        let dir = temp_dir("detect-skip");

        let outcome = ensure_runtime(
            &runner,
            &client,
            Os::MacOs,
            Arch::Arm64,
            &manifest_uv(),
            "3.12",
            "1.28.0",
            &blk,
            &envpy,
            &dir,
            &fast_opts(),
        )
        .unwrap();

        assert_eq!(outcome, RuntimeStepOutcome::AlreadyPresent);
        assert!(
            runner.calls.borrow().is_empty(),
            "skip must never invoke uv"
        );
    }

    #[test]
    fn wrong_mpremote_version_does_not_skip() {
        let runner = FakeRuntimeRunner::new();
        *runner.mpremote_version_result.borrow_mut() = Some("mpremote 1.20.0".to_string());
        *runner.uv_version_result.borrow_mut() = Some("uv 0.11.29".to_string());
        *runner.version_after_provision.borrow_mut() = Some("mpremote 1.28.0".to_string());
        let client = reqwest::blocking::Client::new();
        let blk = mac_blk();
        let envpy = mac_envpy(&blk);
        let dir = temp_dir("wrong-version");

        let outcome = ensure_runtime(
            &runner,
            &client,
            Os::MacOs,
            Arch::Arm64,
            &manifest_uv(),
            "3.12",
            "1.28.0",
            &blk,
            &envpy,
            &dir,
            &fast_opts(),
        )
        .unwrap();

        assert_eq!(outcome, RuntimeStepOutcome::Provisioned);
        assert_eq!(
            runner.calls.borrow().len(),
            3,
            "a stale pin must trigger the full provision sequence"
        );
    }

    #[test]
    fn exact_uv_invocation_args_and_env_mac() {
        let runner = FakeRuntimeRunner::new();
        *runner.uv_version_result.borrow_mut() = Some("uv 0.11.29".to_string());
        *runner.version_after_provision.borrow_mut() = Some("mpremote 1.28.0".to_string());
        let client = reqwest::blocking::Client::new();
        let blk = mac_blk();
        let envpy = mac_envpy(&blk);
        let dir = temp_dir("exact-args-mac");

        ensure_runtime(
            &runner,
            &client,
            Os::MacOs,
            Arch::Arm64,
            &manifest_uv(),
            "3.12",
            "1.28.0",
            &blk,
            &envpy,
            &dir,
            &fast_opts(),
        )
        .unwrap();

        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 3);
        let python_install_dir = blk.join("python").to_string_lossy().into_owned();
        let env_dir = blk.join("env").to_string_lossy().into_owned();
        let envpy_str = envpy.to_string_lossy().into_owned();

        assert_eq!(calls[0].args, vec!["python", "install", "--no-bin", "3.12"]);
        assert_eq!(
            calls[0].env,
            vec![(
                "UV_PYTHON_INSTALL_DIR".to_string(),
                python_install_dir.clone()
            )]
        );

        assert_eq!(
            calls[1].args,
            vec![
                "venv",
                env_dir.as_str(),
                "--managed-python",
                "--python",
                "3.12"
            ]
        );
        assert_eq!(
            calls[1].env,
            vec![(
                "UV_PYTHON_INSTALL_DIR".to_string(),
                python_install_dir.clone()
            )]
        );

        assert_eq!(
            calls[2].args,
            vec![
                "pip",
                "install",
                "--python",
                envpy_str.as_str(),
                "mpremote==1.28.0"
            ]
        );
        assert_eq!(
            calls[2].env,
            vec![("UV_PYTHON_INSTALL_DIR".to_string(), python_install_dir)]
        );
    }

    #[test]
    fn contained_env_paths_differ_correctly_per_os() {
        let runner = FakeRuntimeRunner::new();
        *runner.uv_version_result.borrow_mut() = Some("uv 0.11.29".to_string());
        *runner.version_after_provision.borrow_mut() = Some("mpremote 1.28.0".to_string());
        let client = reqwest::blocking::Client::new();
        let blk = win_blk();
        let envpy = win_envpy(&blk);
        let dir = temp_dir("win-paths");

        ensure_runtime(
            &runner,
            &client,
            Os::Windows,
            Arch::X64,
            &manifest_uv(),
            "3.12",
            "1.28.0",
            &blk,
            &envpy,
            &dir,
            &fast_opts(),
        )
        .unwrap();

        let calls = runner.calls.borrow();
        let expected_env_dir = blk.join("env").to_string_lossy().into_owned();
        let expected_python_dir = blk.join("python").to_string_lossy().into_owned();
        let expected_envpy = envpy.to_string_lossy().into_owned();

        assert_eq!(
            calls[1].args[1], expected_env_dir,
            "venv target must be under this OS's BLK"
        );
        assert_eq!(
            calls[2].args[3], expected_envpy,
            "pip --python must be this OS's ENVPY"
        );
        assert_eq!(calls[0].env[0].1, expected_python_dir);
        assert!(expected_env_dir.starts_with(&blk.to_string_lossy().into_owned()));
        assert!(expected_envpy.starts_with(&blk.to_string_lossy().into_owned()));
    }

    #[test]
    fn uv_already_current_skips_download() {
        let runner = FakeRuntimeRunner::new();
        *runner.uv_version_result.borrow_mut() = Some("uv 0.11.29".to_string());
        *runner.version_after_provision.borrow_mut() = Some("mpremote 1.28.0".to_string());
        let client = reqwest::blocking::Client::new();
        let blk = mac_blk();
        let envpy = mac_envpy(&blk);
        let dir = temp_dir("uv-current");

        // extract_uv would error if called (no real network/archive here);
        // if the step tried to re-download uv this test would fail loudly.
        *runner.extract_uv_result.borrow_mut() = Err("must not be called".to_string());

        ensure_runtime(
            &runner,
            &client,
            Os::MacOs,
            Arch::Arm64,
            &manifest_uv(),
            "3.12",
            "1.28.0",
            &blk,
            &envpy,
            &dir,
            &fast_opts(),
        )
        .unwrap();
    }

    #[test]
    fn mpremote_verify_failure_after_provision_is_reported() {
        let runner = FakeRuntimeRunner::new();
        *runner.uv_version_result.borrow_mut() = Some("uv 0.11.29".to_string());
        // no version_after_provision set: mpremote_version stays None even
        // after the uv sequence "succeeds"
        let client = reqwest::blocking::Client::new();
        let blk = mac_blk();
        let envpy = mac_envpy(&blk);
        let dir = temp_dir("verify-fail");

        let err = ensure_runtime(
            &runner,
            &client,
            Os::MacOs,
            Arch::Arm64,
            &manifest_uv(),
            "3.12",
            "1.28.0",
            &blk,
            &envpy,
            &dir,
            &fast_opts(),
        )
        .unwrap_err();

        assert!(matches!(err, RuntimeError::MpremoteVerifyFailed));
    }
}
