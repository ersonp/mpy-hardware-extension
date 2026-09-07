//! Step 1: VS Code itself. Detect via the explicit candidate paths (never
//! PATH); if absent, resolve the update API, download, sha256-verify,
//! authenticate the signature, install silently, then confirm the CLI is
//! runnable. Ports `step1_vscode`/`Step-VSCode`.
//!
//! Every OS-native operation (running `--version`, checking writability,
//! extracting the archive, stripping the quarantine xattr, running the
//! silent Windows installer, verifying the code signature) goes through
//! [`VscodeInstaller`], injected so the whole step is unit-testable without a
//! real VS Code binary, a real `codesign`/`WinVerifyTrust`, or a real
//! network. The production implementation is OS-specific and lands with
//! `ops.rs` (commit 12), the first caller that runs for real.
//!
//! `ensure_vscode` takes the update-API URL as a plain string built by the
//! caller (`manifest::VscodeComponent::update_api_url`), not a manifest
//! reference -- this is the only thing this step needs from the manifest,
//! and taking it directly (rather than manifest + os + arch) is what makes
//! the whole step testable against a local server instead of the real,
//! sandbox-unreachable `update.code.visualstudio.com`.

use crate::fetch::{self, FetchOptions};
use crate::manifest::{self, VscodeUpdateApiResponse};
use crate::platform::Os;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SignatureError(pub String);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InstallError(pub String);

#[derive(Debug, thiserror::Error)]
pub enum VscodeError {
    #[error("could not reach the VS Code update API at {url}: {source}")]
    UpdateApiRequest {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("update API response at {url} did not match the expected shape: {source}")]
    UpdateApiParse {
        url: String,
        #[source]
        source: manifest::ManifestError,
    },
    #[error("VS Code download failed: {0}")]
    Download(#[from] fetch::FetchError),
    /// The sha256 above only matches what the update API returned over TLS
    /// (trust-on-first-use): it catches a corrupt/MITM'd download, not a
    /// compromised API response serving a malicious URL + its own matching
    /// digest. The signature chains to Apple/Microsoft independent of that
    /// response, so it gates before anything is extracted or executed.
    #[error("VS Code artifact failed signature verification: {0}")]
    Signature(SignatureError),
    #[error("no writable install target (tried: {0:?})")]
    NoWritableInstallTarget(Vec<PathBuf>),
    #[error("could not install VS Code: {0}")]
    Install(InstallError),
    #[error("VS Code CLI not runnable after install")]
    NotRunnableAfterInstall,
}

/// Every OS-native operation this step needs. Mac-only methods are unused on
/// the Windows branch and vice versa; `ensure_vscode` only ever calls the
/// subset that matches `os`.
pub trait VscodeInstaller {
    /// Run `code_cli --version`. `Some(first line of output)` if runnable.
    fn version(&self, code_cli: &Path) -> Option<String>;
    /// Is `dir` writable (mac's `/Applications` vs `~/Applications` fallback;
    /// unused on Windows, which always installs under `%LOCALAPPDATA%`).
    fn is_writable(&self, dir: &Path) -> bool;
    /// Extract the downloaded archive into `target_dir` (mac: `ditto -x -k`).
    fn extract_archive(&self, archive: &Path, target_dir: &Path) -> Result<(), InstallError>;
    /// Strip the quarantine xattr recursively (mac: `xattr -dr
    /// com.apple.quarantine`; a fresh `ditto` extract is otherwise Gatekeeper
    /// quarantined). No-op on Windows.
    fn strip_quarantine(&self, app_dir: &Path);
    /// Run the downloaded installer silently (Windows User Setup:
    /// `/VERYSILENT /NORESTART /SUPPRESSMSGBOXES /MERGETASKS=!runcode`).
    /// Unused on mac (extraction alone is the install).
    fn run_silent_installer(&self, installer_exe: &Path) -> Result<(), InstallError>;
    /// Authenticate the artifact against its OS-native signing chain (mac: a
    /// codesign REQUIREMENT string anchored to Apple + pinned to Microsoft's
    /// Team ID leaf OU; Windows: `WinVerifyTrust` + a subject pin on
    /// `O=Microsoft Corporation`). Never weakened to a bare integrity check.
    fn verify_signature(&self, artifact: &Path) -> Result<(), SignatureError>;
}

fn detect(installer: &dyn VscodeInstaller, candidates: &[PathBuf]) -> Option<(PathBuf, String)> {
    candidates
        .iter()
        .find_map(|c| installer.version(c).map(|v| (c.clone(), v)))
}

fn archive_file_name(os: Os) -> &'static str {
    match os {
        Os::MacOs => "VSCode-darwin-universal.zip",
        Os::Windows => "VSCodeUserSetup.exe",
    }
}

fn fetch_update_api_response(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<VscodeUpdateApiResponse, VscodeError> {
    let body = client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .map_err(|source| VscodeError::UpdateApiRequest {
            url: url.to_string(),
            source,
        })?;
    VscodeUpdateApiResponse::parse(&body).map_err(|source| VscodeError::UpdateApiParse {
        url: url.to_string(),
        source,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VscodeStepOutcome {
    pub code_cli: PathBuf,
    pub product_version: String,
    /// True only on the path that actually installed VS Code this run --
    /// never forced true or reset on a skip, so `state.rs`'s sticky
    /// carry-forward stays correct across repair runs.
    pub installed_by_us: bool,
    /// `Some(api-reported version)` when a fresh install's own `--version`
    /// disagrees with what the update API reported. Logged by the caller,
    /// never a failure -- ports `log "note: installed $X, API reported $Y"`.
    pub product_version_mismatch: Option<String>,
}

/// The full step: detect -> skip, or resolve+download+verify+authenticate+
/// install -> confirm runnable. `update_api_url` is built by the caller via
/// `manifest::VscodeComponent::update_api_url(os, arch)`.
#[allow(clippy::too_many_arguments)]
pub fn ensure_vscode(
    installer: &dyn VscodeInstaller,
    client: &reqwest::blocking::Client,
    os: Os,
    code_candidates: &[PathBuf],
    mac_install_targets: &[PathBuf],
    update_api_url: &str,
    downloads_dir: &Path,
    fetch_opts: &FetchOptions,
) -> Result<VscodeStepOutcome, VscodeError> {
    if let Some((code_cli, product_version)) = detect(installer, code_candidates) {
        return Ok(VscodeStepOutcome {
            code_cli,
            product_version,
            installed_by_us: false,
            product_version_mismatch: None,
        });
    }

    let api = fetch_update_api_response(client, update_api_url)?;

    let archive_path = downloads_dir.join(archive_file_name(os));
    fetch::fetch_and_verify(
        client,
        &api.url,
        &api.sha256_hash,
        &archive_path,
        fetch_opts,
    )?;

    installer
        .verify_signature(&archive_path)
        .map_err(VscodeError::Signature)?;

    match os {
        Os::MacOs => {
            let target = mac_install_targets
                .iter()
                .find(|t| installer.is_writable(t))
                .ok_or_else(|| {
                    VscodeError::NoWritableInstallTarget(mac_install_targets.to_vec())
                })?;
            installer
                .extract_archive(&archive_path, target)
                .map_err(VscodeError::Install)?;
            let app_dir = target.join("Visual Studio Code.app");
            installer.strip_quarantine(&app_dir);
        }
        Os::Windows => {
            installer
                .run_silent_installer(&archive_path)
                .map_err(VscodeError::Install)?;
        }
    }

    let (code_cli, product_version) =
        detect(installer, code_candidates).ok_or(VscodeError::NotRunnableAfterInstall)?;

    let product_version_mismatch = if product_version != api.product_version {
        Some(api.product_version.clone())
    } else {
        None
    };

    Ok(VscodeStepOutcome {
        code_cli,
        product_version,
        installed_by_us: true,
        product_version_mismatch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    struct TestServer {
        addr: SocketAddr,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn start(responses: Vec<(u16, Vec<u8>)>) -> TestServer {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            listener.set_nonblocking(true).unwrap();
            let handle = std::thread::spawn(move || {
                for (status, body) in responses {
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((s, _)) => break s,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                if std::time::Instant::now() >= deadline {
                                    return;
                                }
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => return,
                        }
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut buf = [0u8; 4096];
                    let mut seen = Vec::new();
                    loop {
                        let n = stream.read(&mut buf).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        seen.extend_from_slice(&buf[..n]);
                        if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let reason = if status == 200 { "OK" } else { "Error" };
                    let head = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                    let _ = stream.flush();
                }
            });
            TestServer {
                addr,
                handle: Some(handle),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.addr, path)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "blockless-installer-vscode-test-{name}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn candidates() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/Applications/code"),
            PathBuf::from("/home/x/Applications/code"),
        ]
    }

    fn fast_opts() -> FetchOptions {
        FetchOptions {
            max_attempts: 1,
            backoff_base: Duration::from_millis(1),
        }
    }

    struct FakeInstaller {
        /// Which candidate currently resolves, and to what version.
        resolved: RefCell<Option<(PathBuf, String)>>,
        writable: RefCell<Vec<PathBuf>>,
        verify_signature_result: RefCell<Result<(), String>>,
        extract_calls: RefCell<Vec<(PathBuf, PathBuf)>>,
        strip_quarantine_calls: RefCell<Vec<PathBuf>>,
        run_installer_calls: RefCell<Vec<PathBuf>>,
        verify_signature_calls: RefCell<u32>,
        /// Version to report AFTER extract_archive/run_silent_installer runs
        /// (simulating "now it's really installed").
        post_install_version: RefCell<Option<String>>,
    }

    impl Default for FakeInstaller {
        fn default() -> Self {
            FakeInstaller {
                resolved: RefCell::new(None),
                writable: RefCell::new(Vec::new()),
                verify_signature_result: RefCell::new(Ok(())),
                extract_calls: RefCell::new(Vec::new()),
                strip_quarantine_calls: RefCell::new(Vec::new()),
                run_installer_calls: RefCell::new(Vec::new()),
                verify_signature_calls: RefCell::new(0),
                post_install_version: RefCell::new(None),
            }
        }
    }

    impl VscodeInstaller for FakeInstaller {
        fn version(&self, code_cli: &Path) -> Option<String> {
            self.resolved
                .borrow()
                .as_ref()
                .filter(|(p, _)| p == code_cli)
                .map(|(_, v)| v.clone())
        }
        fn is_writable(&self, dir: &Path) -> bool {
            self.writable.borrow().iter().any(|d| d == dir)
        }
        fn extract_archive(&self, archive: &Path, target_dir: &Path) -> Result<(), InstallError> {
            self.extract_calls
                .borrow_mut()
                .push((archive.to_path_buf(), target_dir.to_path_buf()));
            if let Some(v) = self.post_install_version.borrow().clone() {
                *self.resolved.borrow_mut() = Some((candidates()[0].clone(), v));
            }
            Ok(())
        }
        fn strip_quarantine(&self, app_dir: &Path) {
            self.strip_quarantine_calls
                .borrow_mut()
                .push(app_dir.to_path_buf());
        }
        fn run_silent_installer(&self, installer_exe: &Path) -> Result<(), InstallError> {
            self.run_installer_calls
                .borrow_mut()
                .push(installer_exe.to_path_buf());
            if let Some(v) = self.post_install_version.borrow().clone() {
                *self.resolved.borrow_mut() = Some((candidates()[0].clone(), v));
            }
            Ok(())
        }
        fn verify_signature(&self, _artifact: &Path) -> Result<(), SignatureError> {
            *self.verify_signature_calls.borrow_mut() += 1;
            self.verify_signature_result
                .borrow()
                .clone()
                .map_err(SignatureError)
        }
    }

    /// Spins up a fake "update API" server plus a fake "binary host" server,
    /// wired together so `ensure_vscode`'s two GETs (metadata, then archive)
    /// both land on local servers instead of the real, sandbox-unreachable
    /// Microsoft hosts. Returns (meta_server, binary_server) -- both must
    /// stay alive for the duration of the call.
    fn servers_for(
        body: Vec<u8>,
        sha256hash: &str,
        product_version: &str,
    ) -> (TestServer, TestServer) {
        let binary_server = TestServer::start(vec![(200, body)]);
        let api_json = serde_json::json!({
            "url": binary_server.url("/vscode-archive"),
            "sha256hash": sha256hash,
            "productVersion": product_version,
        })
        .to_string();
        let meta_server = TestServer::start(vec![(200, api_json.into_bytes())]);
        (meta_server, binary_server)
    }

    #[test]
    fn detect_skip_leaves_installed_by_us_false() {
        let installer = FakeInstaller::default();
        *installer.resolved.borrow_mut() = Some((candidates()[0].clone(), "1.99.0".to_string()));
        let client = reqwest::blocking::Client::new();
        let dir = temp_dir("detect-skip");

        let outcome = ensure_vscode(
            &installer,
            &client,
            Os::MacOs,
            &candidates(),
            &[],
            "http://unused.invalid/should-never-be-fetched",
            &dir,
            &fast_opts(),
        )
        .unwrap();

        assert_eq!(outcome.code_cli, candidates()[0]);
        assert_eq!(outcome.product_version, "1.99.0");
        assert!(!outcome.installed_by_us);
        assert_eq!(
            *installer.verify_signature_calls.borrow(),
            0,
            "skip must never touch the network or signature check"
        );
        assert!(installer.extract_calls.borrow().is_empty());
    }

    #[test]
    fn sha_mismatch_fails_before_signature_check() {
        let installer = FakeInstaller::default(); // resolved stays None: not detected
        let client = reqwest::blocking::Client::new();
        let dir = temp_dir("sha-mismatch");
        let (meta_server, _binary_server) =
            servers_for(b"wrong bytes".to_vec(), &"0".repeat(64), "1.99.0");

        let outcome = ensure_vscode(
            &installer,
            &client,
            Os::MacOs,
            &candidates(),
            &[],
            &meta_server.url("/meta"),
            &dir,
            &fast_opts(),
        );

        match outcome {
            Err(VscodeError::Download(fetch::FetchError::Sha256Mismatch { .. })) => {}
            other => panic!("expected Sha256Mismatch, got {other:?}"),
        }
        assert_eq!(*installer.verify_signature_calls.borrow(), 0);
    }

    #[test]
    fn signature_check_gate_blocks_install_even_with_correct_sha() {
        let installer = FakeInstaller::default();
        *installer.verify_signature_result.borrow_mut() =
            Err("not signed by Microsoft".to_string());
        let client = reqwest::blocking::Client::new();
        let dir = temp_dir("sig-gate");
        let body = b"a totally real vs code zip".to_vec();
        let sha = sha256_hex(&body);
        let (meta_server, _binary_server) = servers_for(body, &sha, "1.99.0");

        let outcome = ensure_vscode(
            &installer,
            &client,
            Os::MacOs,
            &candidates(),
            &[],
            &meta_server.url("/meta"),
            &dir,
            &fast_opts(),
        );

        match outcome {
            Err(VscodeError::Signature(_)) => {}
            other => panic!("expected Signature error, got {other:?}"),
        }
        assert_eq!(*installer.verify_signature_calls.borrow(), 1);
        assert!(
            installer.extract_calls.borrow().is_empty(),
            "must never extract an unauthenticated artifact"
        );
    }

    #[test]
    fn applications_writability_fallback_picks_the_second_writable_target() {
        let installer = FakeInstaller::default();
        installer
            .writable
            .borrow_mut()
            .push(PathBuf::from("/home/x/Applications")); // only the fallback is writable
        *installer.verify_signature_result.borrow_mut() = Ok(());
        *installer.post_install_version.borrow_mut() = Some("1.99.0".to_string());
        let client = reqwest::blocking::Client::new();
        let dir = temp_dir("writability-fallback");
        let body = b"a totally real vs code zip".to_vec();
        let sha = sha256_hex(&body);
        let (meta_server, _binary_server) = servers_for(body, &sha, "1.99.0");
        let targets = vec![
            PathBuf::from("/Applications"),
            PathBuf::from("/home/x/Applications"),
        ];

        let outcome = ensure_vscode(
            &installer,
            &client,
            Os::MacOs,
            &candidates(),
            &targets,
            &meta_server.url("/meta"),
            &dir,
            &fast_opts(),
        )
        .unwrap();

        assert!(outcome.installed_by_us);
        let calls = installer.extract_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].1,
            PathBuf::from("/home/x/Applications"),
            "must fall back to the writable target"
        );
    }

    #[test]
    fn no_writable_target_fails_loudly() {
        let installer = FakeInstaller::default(); // writable list stays empty
        *installer.verify_signature_result.borrow_mut() = Ok(());
        let client = reqwest::blocking::Client::new();
        let dir = temp_dir("no-writable-target");
        let body = b"a totally real vs code zip".to_vec();
        let sha = sha256_hex(&body);
        let (meta_server, _binary_server) = servers_for(body, &sha, "1.99.0");
        let targets = vec![PathBuf::from("/Applications")];

        let outcome = ensure_vscode(
            &installer,
            &client,
            Os::MacOs,
            &candidates(),
            &targets,
            &meta_server.url("/meta"),
            &dir,
            &fast_opts(),
        );

        assert!(matches!(
            outcome,
            Err(VscodeError::NoWritableInstallTarget(_))
        ));
        assert!(installer.extract_calls.borrow().is_empty());
    }

    #[test]
    fn fresh_install_records_installed_by_us_true() {
        let installer = FakeInstaller::default();
        installer
            .writable
            .borrow_mut()
            .push(PathBuf::from("/Applications"));
        *installer.verify_signature_result.borrow_mut() = Ok(());
        *installer.post_install_version.borrow_mut() = Some("1.99.0".to_string());
        let client = reqwest::blocking::Client::new();
        let dir = temp_dir("fresh-install");
        let body = b"a totally real vs code zip".to_vec();
        let sha = sha256_hex(&body);
        let (meta_server, _binary_server) = servers_for(body, &sha, "1.99.0");
        let targets = vec![PathBuf::from("/Applications")];

        let outcome = ensure_vscode(
            &installer,
            &client,
            Os::MacOs,
            &candidates(),
            &targets,
            &meta_server.url("/meta"),
            &dir,
            &fast_opts(),
        )
        .unwrap();

        assert!(
            outcome.installed_by_us,
            "installed_by_us must be true on the install path"
        );
        assert_eq!(outcome.product_version, "1.99.0");
        assert_eq!(outcome.product_version_mismatch, None);
        assert_eq!(installer.strip_quarantine_calls.borrow().len(), 1);
    }

    #[test]
    fn product_version_mismatch_is_logged_not_failed() {
        let installer = FakeInstaller::default();
        installer
            .writable
            .borrow_mut()
            .push(PathBuf::from("/Applications"));
        *installer.verify_signature_result.borrow_mut() = Ok(());
        // the API said 1.99.0, but --version after install reports something else
        *installer.post_install_version.borrow_mut() = Some("1.99.1".to_string());
        let client = reqwest::blocking::Client::new();
        let dir = temp_dir("version-mismatch");
        let body = b"a totally real vs code zip".to_vec();
        let sha = sha256_hex(&body);
        let (meta_server, _binary_server) = servers_for(body, &sha, "1.99.0");
        let targets = vec![PathBuf::from("/Applications")];

        let outcome = ensure_vscode(
            &installer,
            &client,
            Os::MacOs,
            &candidates(),
            &targets,
            &meta_server.url("/meta"),
            &dir,
            &fast_opts(),
        )
        .unwrap();

        assert!(
            outcome.installed_by_us,
            "a version mismatch must not fail the step"
        );
        assert_eq!(outcome.product_version, "1.99.1");
        assert_eq!(outcome.product_version_mismatch, Some("1.99.0".to_string()));
    }

    #[test]
    fn windows_branch_runs_silent_installer_not_extract() {
        let installer = FakeInstaller::default();
        *installer.verify_signature_result.borrow_mut() = Ok(());
        *installer.post_install_version.borrow_mut() = Some("1.99.0".to_string());
        let client = reqwest::blocking::Client::new();
        let dir = temp_dir("windows-install");
        let body = b"a totally real vs code installer".to_vec();
        let sha = sha256_hex(&body);
        let (meta_server, _binary_server) = servers_for(body, &sha, "1.99.0");

        let outcome = ensure_vscode(
            &installer,
            &client,
            Os::Windows,
            &candidates(),
            &[], // mac_install_targets unused on Windows
            &meta_server.url("/meta"),
            &dir,
            &fast_opts(),
        )
        .unwrap();

        assert!(outcome.installed_by_us);
        assert_eq!(installer.run_installer_calls.borrow().len(), 1);
        assert!(installer.extract_calls.borrow().is_empty());
        assert!(installer.strip_quarantine_calls.borrow().is_empty());
    }
}
