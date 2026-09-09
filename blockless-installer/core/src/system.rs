//! The real, OS-native implementation of [`crate::ops::Environment`] for an
//! actual machine. Everything here is a thin `std::process::Command`
//! shell-out to the exact tool + flags the M0 scripts already proved work
//! (`codesign`, `ditto`, `xattr`, `tar`; `taskkill`, `Get-AuthenticodeSignature`,
//! the User Setup installer flags) -- this file is NOT re-deriving behavior,
//! it is porting a command line verbatim per OS.
//!
//! Cfg-gated per target: only the current build's own OS module compiles at
//! all, so neither module can be exercised (compiled OR run) on the other
//! platform, or in this sandbox, which is neither. Every other module in
//! this crate is unit-tested against a fake implementation of the same
//! traits; this file's correctness is verified on the acceptance rig, not
//! here (see `/scope.md`'s "Acceptance"). The mac module's own tar-extraction
//! tests happen to run on any Unix (`tar` isn't mac-specific), but
//! installer-ci.yml only runs `ubuntu-latest` and `windows-latest`, neither
//! of which sets `target_os = "macos"` -- so even those never execute in
//! CI, only when `cargo test` runs for real on a Mac (or in a sandbox like
//! this one, at build time, as they were before landing here).
//!
//! One deliberate, flagged deviation from `/scope.md`'s stated preference:
//! Windows signature verification shells out to PowerShell's
//! `Get-AuthenticodeSignature` (exactly what `install-blockless.ps1` already
//! does) rather than the `windows` crate's `WinVerifyTrust` binding. The
//! `windows` crate needs a real Windows toolchain to build against; this
//! sandbox has none, and shipping an unverifiable FFI integration seemed
//! like the wrong trade against a mechanism M0 already proved on a real
//! Windows Sandbox. The security PROPERTY (gate on a Valid Authenticode
//! signature whose signer's Subject pins `O=Microsoft Corporation`, before
//! anything is extracted or executed) is identical either way. Revisiting
//! this on real Windows hardware is a reasonable follow-up, not a gap in
//! what it protects against.

#[cfg(target_os = "macos")]
pub use mac::MacEnvironment as SystemEnvironment;
#[cfg(target_os = "windows")]
pub use windows::WindowsEnvironment as SystemEnvironment;

#[cfg(target_os = "macos")]
mod mac {
    use crate::extensions::ExtensionsRunner;
    use crate::profile::CommandRunner;
    use crate::runtime::RuntimeRunner;
    use crate::uninstall::UninstallRunner;
    use crate::vscode::{InstallError, SignatureError, VscodeInstaller};
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    pub struct MacEnvironment;

    fn run_ok(cmd: &mut Command) -> bool {
        cmd.status().map(|s| s.success()).unwrap_or(false)
    }

    fn stdout_of(cmd: &mut Command) -> Option<String> {
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout).ok()
    }

    impl CommandRunner for MacEnvironment {
        /// Match the whole app bundle, not the main executable.
        ///
        /// This looked for `.../Contents/MacOS/Electron`, ported faithfully from
        /// the M0 scripts, which use that pattern in seven places. It matches
        /// NOTHING on a real machine, for two compounding reasons: VS Code's
        /// main binary is named `Code`, not `Electron`, and macOS will not let
        /// `pgrep -f` read a hardened main process's argv at all, so even the
        /// corrected path matches nothing. Measured against a running VS Code:
        /// the `Electron` pattern found 0 processes, `.../MacOS/Code` found only
        /// helpers, `pgrep -x Code` found 0, and this pattern found 19.
        ///
        /// So this returned an empty vec ALWAYS, and every guard keyed on it was
        /// inert on macOS: uninstall never refused while VS Code was running, the
        /// profile seed never skipped, and the settings writer's "never write
        /// while VS Code is running" rule never held -- around `storage.json`,
        /// which holds every profile and window state the user has. Observed on a
        /// real VM: uninstall removed the profile with two windows open, and VS
        /// Code recreated it.
        ///
        /// The helper processes are what this matches, and that is fine: they
        /// exist only while VS Code does, which is precisely the question. Every
        /// caller asks `is_empty()`, never for a specific pid.
        ///
        /// No test here can see this. `CommandRunner` is mocked at all eight test
        /// sites, including the one that pins uninstall's refusal, so the trait
        /// that makes the logic testable is what left this unexercised.
        fn running_vscode_pids(&self) -> Vec<u32> {
            let Some(out) = Command::new("pgrep")
                .args(["-f", "Visual Studio Code.app"])
                .output()
                .ok()
            else {
                return vec![];
            };
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect()
        }
        fn spawn(&self, code_cli: &Path, args: &[&str]) -> std::io::Result<u32> {
            Command::new(code_cli).args(args).spawn().map(|c| c.id())
        }
        fn is_alive(&self, pid: u32) -> bool {
            run_ok(Command::new("kill").args(["-0", &pid.to_string()]))
        }
        fn request_graceful_close(&self, _pid: u32) {
            // Ports `stop_code_and_wait`'s exact mechanism
            // (`install-blockless.zsh`): `osascript ... quit`, which
            // provably saves window/profile state, not SIGTERM. A prior
            // version of this sent SIGTERM to the exact resolved PID
            // instead, on the theory that it's more precisely scoped than
            // an app-wide quit -- but that traded a proven mechanism for an
            // unverifiable one: this sandbox has no way to confirm whether
            // VS Code's Electron main process treats SIGTERM as equivalent
            // to a normal quit for state-saving, and shipping that guess on
            // the exact path the panel-auto-open fix depends on is the
            // wrong trade. AppleScript's `tell application ... quit` can
            // only target by application, not PID, but the exact-PID
            // scoping this trait's callers rely on is unaffected: it's
            // `stop_and_wait` (profile.rs) that polls `is_alive` on the one
            // PID it cares about afterward, regardless of which mechanism
            // asked the app to quit.
            // Requires TCC Automation consent for the calling process on a
            // real Mac; denied or unprompted (the normal unattended case),
            // osascript fails and quits nothing. Warn rather than swallow,
            // since a silent failure here can fall through to
            // `stop_and_wait`'s force-kill -- strictly worse for
            // window/profile-state saving than the SIGTERM this replaced.
            // Non-zero isn't always that, though: `register_profile` can
            // call this once per newly-appeared PID, and an app-wide quit
            // means the SECOND call always finds nothing left to quit --
            // harmless (`stop_and_wait`'s own `is_alive` check is what
            // decides whether force-kill actually runs), just noisy.
            if !run_ok(
                Command::new("osascript")
                    .args(["-e", r#"tell application "Visual Studio Code" to quit"#]),
            ) {
                tracing::warn!(
                    "osascript quit returned non-zero (the app may already have quit); \
                     force-kill runs only if the PID is still alive"
                );
            }
        }
        fn force_kill(&self, pid: u32) {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
        fn sleep(&self, d: Duration) {
            std::thread::sleep(d);
        }
    }

    impl VscodeInstaller for MacEnvironment {
        fn version(&self, code_cli: &Path) -> Option<String> {
            stdout_of(Command::new(code_cli).arg("--version"))
                .and_then(|s| s.lines().next().map(str::to_string))
        }
        fn is_writable(&self, dir: &Path) -> bool {
            if std::fs::create_dir_all(dir).is_err() {
                return false;
            }
            let probe = dir.join(".blockless-write-test");
            let ok = std::fs::write(&probe, b"").is_ok();
            let _ = std::fs::remove_file(&probe);
            ok
        }
        fn extract_archive(&self, archive: &Path, target_dir: &Path) -> Result<(), InstallError> {
            std::fs::create_dir_all(target_dir).map_err(|e| {
                InstallError(format!("could not create {}: {e}", target_dir.display()))
            })?;
            if run_ok(
                Command::new("ditto")
                    .args(["-x", "-k"])
                    .arg(archive)
                    .arg(target_dir),
            ) {
                Ok(())
            } else {
                Err(InstallError(format!(
                    "ditto -x -k {} failed",
                    archive.display()
                )))
            }
        }
        fn strip_quarantine(&self, app_dir: &Path) {
            // Best-effort, matches M0's `|| true`: a missing/already-clean
            // xattr is not an error.
            let _ = Command::new("xattr")
                .args(["-dr", "com.apple.quarantine"])
                .arg(app_dir)
                .status();
        }
        fn run_silent_installer(&self, _installer_exe: &Path) -> Result<(), InstallError> {
            Err(InstallError(
                "run_silent_installer is a Windows-only operation".to_string(),
            ))
        }
        fn verify_signature(&self, artifact: &Path) -> Result<(), SignatureError> {
            // NOT `--verify` alone (checks only internal consistency, an
            // ad-hoc/self-signed bundle passes it) and NOT a grep of
            // `-dvvv` (defeatable by a crafted identifier): a codesign
            // REQUIREMENT anchors to Apple's root AND pins Microsoft's Team
            // ID leaf OU, so a compromised update API cannot substitute a
            // differently-signed binary.
            let ok = run_ok(
                Command::new("codesign")
                    .args([
                        "--verify",
                        "-R",
                        "=anchor apple generic and certificate leaf[subject.OU] = UBF8T346G9",
                    ])
                    .arg(artifact),
            );
            if ok {
                Ok(())
            } else {
                Err(SignatureError(format!(
                    "{} is not authentically signed by Microsoft",
                    artifact.display()
                )))
            }
        }
    }

    impl ExtensionsRunner for MacEnvironment {
        fn list_extensions(&self, code_cli: &Path, profile_name: &str) -> Option<Vec<String>> {
            let out = stdout_of(Command::new(code_cli).args([
                "--profile",
                profile_name,
                "--list-extensions",
            ]))?;
            Some(
                out.lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect(),
            )
        }
        fn install_extension(&self, code_cli: &Path, profile_name: &str, vsix_or_id: &str) -> bool {
            run_ok(Command::new(code_cli).args([
                "--profile",
                profile_name,
                "--install-extension",
                vsix_or_id,
                "--force",
            ]))
        }
    }

    impl RuntimeRunner for MacEnvironment {
        fn mpremote_version(&self, envpy: &Path) -> Option<String> {
            stdout_of(Command::new(envpy).args(["-m", "mpremote", "version"]))
        }
        fn uv_version(&self, uv_bin: &Path) -> Option<String> {
            stdout_of(Command::new(uv_bin).arg("--version"))
        }
        fn extract_uv(&self, archive: &Path, dest_dir: &Path) -> Result<(), String> {
            extract_uv_tarball(archive, dest_dir)
        }
        fn run_uv(&self, uv_bin: &Path, args: &[&str], env: &[(&str, &str)]) -> bool {
            let mut cmd = Command::new(uv_bin);
            cmd.args(args);
            for (k, v) in env {
                cmd.env(k, v);
            }
            run_ok(&mut cmd)
        }
        /// Checked by path, not by running anything: invoking the tool is what
        /// raises the install-the-tools dialog we are trying to avoid.
        fn developer_tools_present(&self) -> bool {
            crate::runtime::DEVELOPER_TOOL_PATHS
                .iter()
                .any(|p| Path::new(p).exists())
        }
    }

    impl UninstallRunner for MacEnvironment {
        fn remove_dir_all(&self, path: &Path) -> Result<(), String> {
            std::fs::remove_dir_all(path).map_err(|e| e.to_string())
        }
        fn run_vscode_uninstaller(&self, _vscode_dir: &Path) -> Result<bool, String> {
            // No separate uninstaller on mac: removing the .app IS the
            // uninstall, handled by the caller's remove_dir_all fallback.
            Ok(false)
        }
    }

    /// `tar -xzf` into `dest_dir`, then locate the `uv` binary regardless of
    /// whether the archive wraps it in a `<target-triple>/` directory (the
    /// common shape for Rust cross-release tarballs) or not -- this cannot
    /// be confirmed against the real astral-sh/uv asset without network
    /// access (see `/scope.md`'s manifest section), so it tries both shapes
    /// rather than assuming one.
    fn extract_uv_tarball(archive: &Path, dest_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(dest_dir)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("tar -xzf {} failed", archive.display()));
        }
        relocate_binary_if_nested(dest_dir, "uv")
    }

    /// If `dest_dir/name` doesn't exist but exactly one subdirectory does
    /// and it contains `name`, move it up one level. Handles both "flat"
    /// and "wrapped in one directory" archive layouts without needing to
    /// know in advance which the real release uses.
    fn relocate_binary_if_nested(dest_dir: &Path, name: &str) -> Result<(), String> {
        if dest_dir.join(name).exists() {
            return Ok(());
        }
        let entries = std::fs::read_dir(dest_dir).map_err(|e| e.to_string())?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let nested = path.join(name);
                if nested.exists() {
                    std::fs::rename(&nested, dest_dir.join(name)).map_err(|e| e.to_string())?;
                    return Ok(());
                }
            }
        }
        Err(format!(
            "{name} not found in the extracted archive at {} (neither flat nor one level nested)",
            dest_dir.display()
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering};

        fn temp_dir(name: &str) -> std::path::PathBuf {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "blockless-installer-system-mac-test-{name}-{}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// Builds a real .tar.gz (via the system `tar`, available in this
        /// sandbox even though the rest of this module isn't exercised
        /// here) so extraction is tested against a genuine archive, not a
        /// hand-rolled byte layout.
        /// Tars the CONTENTS of `src_dir` (not `src_dir` itself) so the
        /// archive's internal layout matches what's actually being tested:
        /// tarring the directory BY NAME (`-C parent name`) would add an
        /// extra wrapping level every time, silently turning a "flat"
        /// fixture into a "one level nested" one.
        fn build_tarball(src_dir: &Path, out_path: &Path) {
            let status = Command::new("tar")
                .arg("-czf")
                .arg(out_path)
                .arg("-C")
                .arg(src_dir)
                .arg(".")
                .status()
                .unwrap();
            assert!(status.success());
        }

        #[test]
        fn extract_uv_tarball_handles_a_flat_layout() {
            let dir = temp_dir("flat");
            let src = dir.join("src");
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(src.join("uv"), b"fake uv binary").unwrap();
            let archive = dir.join("uv.tar.gz");
            build_tarball(&src, &archive);

            let dest = dir.join("dest");
            extract_uv_tarball(&archive, &dest).unwrap();

            assert_eq!(std::fs::read(dest.join("uv")).unwrap(), b"fake uv binary");
        }

        #[test]
        fn extract_uv_tarball_handles_a_nested_target_triple_dir() {
            let dir = temp_dir("nested");
            let src = dir.join("src");
            std::fs::create_dir_all(src.join("uv-aarch64-apple-darwin")).unwrap();
            std::fs::write(
                src.join("uv-aarch64-apple-darwin").join("uv"),
                b"fake uv binary",
            )
            .unwrap();
            let archive = dir.join("uv.tar.gz");
            build_tarball(&src, &archive);

            let dest = dir.join("dest");
            extract_uv_tarball(&archive, &dest).unwrap();

            assert_eq!(std::fs::read(dest.join("uv")).unwrap(), b"fake uv binary");
        }

        #[test]
        fn extract_uv_tarball_fails_loudly_when_binary_is_nowhere() {
            let dir = temp_dir("missing-binary");
            let src = dir.join("src");
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(src.join("readme.txt"), b"not the binary").unwrap();
            let archive = dir.join("uv.tar.gz");
            build_tarball(&src, &archive);

            let dest = dir.join("dest");
            let err = extract_uv_tarball(&archive, &dest).unwrap_err();
            assert!(err.contains("not found"), "{err}");
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use crate::extensions::ExtensionsRunner;
    use crate::profile::CommandRunner;
    use crate::runtime::RuntimeRunner;
    use crate::uninstall::UninstallRunner;
    use crate::vscode::{InstallError, SignatureError, VscodeInstaller};
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    pub struct WindowsEnvironment;

    fn run_ok(cmd: &mut Command) -> bool {
        cmd.status().map(|s| s.success()).unwrap_or(false)
    }

    fn stdout_of(cmd: &mut Command) -> Option<String> {
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout).ok()
    }

    fn powershell(script: &str) -> Command {
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
        cmd
    }

    /// Escape a path for interpolation into a PowerShell single-quoted
    /// string: doubling `'` is PowerShell's own escape for it inside a
    /// single-quoted literal. Without this, a path containing `'` (rare but
    /// legal in a Windows username/dir name) breaks out of the string and
    /// its trailing text is interpreted as script rather than data.
    fn ps_quote(path: &Path) -> String {
        path.display().to_string().replace('\'', "''")
    }

    impl CommandRunner for WindowsEnvironment {
        fn running_vscode_pids(&self) -> Vec<u32> {
            let Some(out) = stdout_of(&mut powershell(
                "(Get-Process -Name Code -ErrorAction SilentlyContinue).Id",
            )) else {
                return vec![];
            };
            out.lines().filter_map(|l| l.trim().parse().ok()).collect()
        }
        fn spawn(&self, code_cli: &Path, args: &[&str]) -> std::io::Result<u32> {
            Command::new(code_cli).args(args).spawn().map(|c| c.id())
        }
        fn is_alive(&self, pid: u32) -> bool {
            run_ok(&mut powershell(&format!(
                "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"
            )))
        }
        fn request_graceful_close(&self, pid: u32) {
            // WITHOUT /F: posts WM_CLOSE so VS Code saves window/profile
            // state before exiting (matches M0's Stop-OurCode).
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string()])
                .status();
        }
        fn force_kill(&self, pid: u32) {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .status();
        }
        fn sleep(&self, d: Duration) {
            std::thread::sleep(d);
        }
    }

    impl VscodeInstaller for WindowsEnvironment {
        fn version(&self, code_cli: &Path) -> Option<String> {
            stdout_of(Command::new(code_cli).arg("--version"))
                .and_then(|s| s.lines().next().map(str::to_string))
        }
        fn is_writable(&self, _dir: &Path) -> bool {
            // Unused on Windows: install always targets %LOCALAPPDATA%,
            // never a writability-selected target (vscode.rs never calls
            // this on the Windows branch).
            true
        }
        fn extract_archive(&self, _archive: &Path, _target_dir: &Path) -> Result<(), InstallError> {
            Err(InstallError(
                "extract_archive is a macOS-only operation".to_string(),
            ))
        }
        fn strip_quarantine(&self, _app_dir: &Path) {}
        fn run_silent_installer(&self, installer_exe: &Path) -> Result<(), InstallError> {
            if run_ok(Command::new(installer_exe).args([
                "/VERYSILENT",
                "/NORESTART",
                "/SUPPRESSMSGBOXES",
                "/MERGETASKS=!runcode",
            ])) {
                Ok(())
            } else {
                Err(InstallError(format!(
                    "{} /VERYSILENT failed",
                    installer_exe.display()
                )))
            }
        }
        fn verify_signature(&self, artifact: &Path) -> Result<(), SignatureError> {
            // See this file's module doc: PowerShell Get-AuthenticodeSignature
            // (exactly what install-blockless.ps1 already uses), not the
            // windows crate's WinVerifyTrust binding -- a deliberate,
            // flagged deviation, not a weaker check.
            let script = format!(
                // -LiteralPath, not the positional/-FilePath binding: -FilePath
                // is wildcard-capable, so a path containing `[`/`]` (legal in
                // a Windows username) would resolve as a character class
                // instead of matching itself literally -- same class as the
                // Expand-Archive fix above.
                "$sig = Get-AuthenticodeSignature -LiteralPath '{}'; \
                 if ($sig.Status -ne 'Valid') {{ exit 1 }}; \
                 if ($sig.SignerCertificate.Subject -notmatch 'O=Microsoft Corporation(,|$)') {{ exit 1 }}; \
                 exit 0",
                ps_quote(artifact)
            );
            if run_ok(&mut powershell(&script)) {
                Ok(())
            } else {
                Err(SignatureError(format!(
                    "{} is not authentically signed by Microsoft",
                    artifact.display()
                )))
            }
        }
    }

    impl ExtensionsRunner for WindowsEnvironment {
        fn list_extensions(&self, code_cli: &Path, profile_name: &str) -> Option<Vec<String>> {
            let out = stdout_of(Command::new(code_cli).args([
                "--profile",
                profile_name,
                "--list-extensions",
            ]))?;
            Some(
                out.lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect(),
            )
        }
        fn install_extension(&self, code_cli: &Path, profile_name: &str, vsix_or_id: &str) -> bool {
            run_ok(Command::new(code_cli).args([
                "--profile",
                profile_name,
                "--install-extension",
                vsix_or_id,
                "--force",
            ]))
        }
    }

    impl RuntimeRunner for WindowsEnvironment {
        fn mpremote_version(&self, envpy: &Path) -> Option<String> {
            stdout_of(Command::new(envpy).args(["-m", "mpremote", "version"]))
        }
        fn uv_version(&self, uv_bin: &Path) -> Option<String> {
            stdout_of(Command::new(uv_bin).arg("--version"))
        }
        fn extract_uv(&self, archive: &Path, dest_dir: &Path) -> Result<(), String> {
            std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
            let script = format!(
                // -LiteralPath, not -Path: -Path is wildcard-capable, so a
                // path containing `[`/`]` (legal in a Windows username, and
                // otherwise indistinguishable from an intentional glob)
                // would resolve as a character class instead of matching
                // itself literally.
                "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
                ps_quote(archive),
                ps_quote(dest_dir)
            );
            if !run_ok(&mut powershell(&script)) {
                return Err(format!("Expand-Archive {} failed", archive.display()));
            }
            relocate_binary_if_nested(dest_dir, "uv.exe")
        }
        fn run_uv(&self, uv_bin: &Path, args: &[&str], env: &[(&str, &str)]) -> bool {
            let mut cmd = Command::new(uv_bin);
            cmd.args(args);
            for (k, v) in env {
                cmd.env(k, v);
            }
            run_ok(&mut cmd)
        }
        /// Not a Windows concern: `install_name_tool` is a macOS tool and uv
        /// never reaches for it here. Answering true keeps the shim branch shut
        /// on this platform regardless of anything else.
        fn developer_tools_present(&self) -> bool {
            true
        }
    }

    impl UninstallRunner for WindowsEnvironment {
        fn remove_dir_all(&self, path: &Path) -> Result<(), String> {
            std::fs::remove_dir_all(path).map_err(|e| e.to_string())
        }
        fn run_vscode_uninstaller(&self, vscode_dir: &Path) -> Result<bool, String> {
            let unins = vscode_dir.join("unins000.exe");
            if !unins.exists() {
                return Ok(false);
            }
            if run_ok(Command::new(&unins).args(["/VERYSILENT", "/NORESTART", "/SUPPRESSMSGBOXES"]))
            {
                Ok(true)
            } else {
                Err(format!("{} /VERYSILENT failed", unins.display()))
            }
        }
    }

    /// See the mac module's `relocate_binary_if_nested`: same uncertainty,
    /// same fallback (works whether `Expand-Archive` produced a flat or a
    /// one-level-nested layout).
    fn relocate_binary_if_nested(dest_dir: &Path, name: &str) -> Result<(), String> {
        if dest_dir.join(name).exists() {
            return Ok(());
        }
        let entries = std::fs::read_dir(dest_dir).map_err(|e| e.to_string())?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let nested = path.join(name);
                if nested.exists() {
                    std::fs::rename(&nested, dest_dir.join(name)).map_err(|e| e.to_string())?;
                    return Ok(());
                }
            }
        }
        Err(format!(
            "{name} not found in the extracted archive at {} (neither flat nor one level nested)",
            dest_dir.display()
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ps_quote_doubles_embedded_single_quotes() {
            assert_eq!(
                ps_quote(Path::new(r"C:\Users\it's me\code")),
                r"C:\Users\it''s me\code"
            );
        }

        #[test]
        fn ps_quote_is_identity_on_a_quote_free_path() {
            assert_eq!(
                ps_quote(Path::new(r"C:\Users\normal\code")),
                r"C:\Users\normal\code"
            );
        }

        #[test]
        fn ps_quote_handles_multiple_quotes() {
            assert_eq!(ps_quote(Path::new("a'b'c")), "a''b''c");
        }
    }
}
