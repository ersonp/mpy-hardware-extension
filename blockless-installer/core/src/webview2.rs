//! Provisioning the WebView2 runtime the GUI needs on Windows.
//!
//! WHY THIS EXISTS AT ALL. The GUI used to ship as an NSIS bundle whose only
//! real job was installing this runtime, and it did that job by reading a
//! registry string:
//!
//! ```nsis
//! ReadRegStr $4 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-...}" "pv"
//! ${If} $4 == ""
//!   ; every install mode lives in here
//! ```
//!
//! Microsoft Edge registers that same client GUID. On a machine where Edge has
//! registered it but the runtime was never installed -- a stock Windows
//! Sandbox image, and anything like it -- the probe finds a version string,
//! concludes the runtime is present, and provisions nothing. Found on the rig,
//! 2026-09-21: the bundle "installed" in 1.1s and the app opened a bare window
//! titled `Error`. No `webviewInstallMode` value avoids it; all three sit
//! inside that same `${If}`.
//!
//! So the bundle could not be trusted with the one task it existed for, while
//! costing a whole install-the-installer step: an Add/Remove Programs entry, a
//! Start-menu shortcut, a sidecar co-location contract, and an installer that
//! outlived the product it installed (nothing in `uninstall.rs` removes it).
//!
//! Doing it here instead is strictly better on every axis that matters:
//! detection is Microsoft's own API rather than a registry guess (injected via
//! [`Webview2Runner::runtime_available`], since only the GUI crate can call
//! it), provisioning is the official Evergreen bootstrapper, and the whole
//! thing works from a portable executable that is never installed.
//!
//! SECURITY POSTURE. The bootstrapper is downloaded without a sha256 pin -- it
//! comes from a redirector that always serves the current build, so no stable
//! digest exists to pin, and fabricating one is exactly what the rig
//! documentation forbids. Integrity comes from its Authenticode signature,
//! verified BEFORE the file is executed, through the same
//! `VscodeInstaller::verify_signature` gate the VS Code installer passes
//! (Microsoft subject pin included). A failed signature is a hard stop: the
//! artifact is never run.
//!
//! CONSENT is the caller's job, not this module's. `ensure_webview2` downloads
//! and installs as soon as it is called; the GUI asks the user first. Keeping
//! the prompt out of here is what lets this be unit-tested without a UI.

use crate::vscode::{InstallError, SignatureError};
use std::path::{Path, PathBuf};

/// Microsoft's Evergreen bootstrapper. A redirector, always current.
pub const BOOTSTRAPPER_URL: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";

/// The OS-native half, injected so this module is testable without a real
/// download, a real signature check, or a real runtime installation.
pub trait Webview2Runner {
    /// Is the runtime ACTUALLY available? Implemented over Microsoft's
    /// `GetAvailableCoreWebView2BrowserVersionString` (which is what
    /// `tauri::webview_version()` calls), never over the EdgeUpdate registry
    /// key -- reading that key is the bug this module exists to avoid.
    fn runtime_available(&self) -> bool;
    /// Fetch the Evergreen bootstrapper to `dest`.
    ///
    /// Injected rather than called directly so this module has NO network
    /// dependency in tests. The first version of these tests called
    /// `fetch::download_unverified` for real: on a machine with internet the
    /// download succeeded and the test asserted the wrong branch, while on an
    /// offline runner it would have "passed" for the wrong reason. A test that
    /// reads the machine it runs on instead of its fixture is exactly the
    /// defect class the rig documentation keeps flagging, so the seam moved
    /// here.
    fn download_bootstrapper(&self, dest: &Path) -> Result<(), String>;
    /// Authenticode-verify a downloaded artifact, pinned to Microsoft.
    fn verify_signature(&self, artifact: &Path) -> Result<(), SignatureError>;
    /// Run the bootstrapper so it installs PER USER and without a prompt.
    fn run_bootstrapper(&self, exe: &Path) -> Result<(), InstallError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Webview2Outcome {
    /// Nothing to do.
    AlreadyPresent,
    /// Installed just now, and confirmed present afterwards.
    Installed,
    /// Ran, but the runtime still is not there. Distinct from `Failed`: every
    /// step reported success, so the machine is in a stranger state than a
    /// download or signature failure leaves it, and the message says so.
    RanButStillMissing,
    /// Could not provision it. The string is user-facing.
    Failed(String),
}

/// Ensure the WebView2 runtime is present, installing it if it is not.
///
/// Re-checks availability AFTER running the bootstrapper rather than trusting
/// its exit code -- the same "confirm, do not assume" posture the uninstall
/// learned the hard way, and the reason `RanButStillMissing` is its own
/// outcome rather than being folded into success.
pub fn ensure_webview2(runner: &dyn Webview2Runner, download_dir: &Path) -> Webview2Outcome {
    if runner.runtime_available() {
        return Webview2Outcome::AlreadyPresent;
    }

    let dest: PathBuf = download_dir.join("MicrosoftEdgeWebview2Setup.exe");
    if let Err(e) = runner.download_bootstrapper(&dest) {
        return Webview2Outcome::Failed(format!("could not download the WebView2 installer: {e}"));
    }

    // Signature BEFORE execution, always. This is the only integrity gate on
    // this artifact, so a failure here must never fall through to running it.
    if let Err(e) = runner.verify_signature(&dest) {
        let _ = std::fs::remove_file(&dest);
        return Webview2Outcome::Failed(format!(
            "the downloaded WebView2 installer is not authentically signed by Microsoft, \
             so it was discarded and not run: {e}"
        ));
    }

    if let Err(e) = runner.run_bootstrapper(&dest) {
        let _ = std::fs::remove_file(&dest);
        return Webview2Outcome::Failed(format!("the WebView2 installer failed: {e}"));
    }
    let _ = std::fs::remove_file(&dest);

    if runner.runtime_available() {
        Webview2Outcome::Installed
    } else {
        Webview2Outcome::RanButStillMissing
    }
}

#[cfg(test)]
#[path = "tests/webview2.rs"]
mod tests;
