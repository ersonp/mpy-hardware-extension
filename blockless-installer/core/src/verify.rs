//! The 7 acceptance checks, ported with M0 semantics from
//! `verify-blockless.{zsh,ps1}`. Read-only throughout: no check ever writes
//! anything, and the profile settings target comes from the JOURNALED
//! `profileLocation` in `state.json`, never a fresh `storage.json` read (the
//! whole point of journaling it in `profile.rs`/`settings.rs` is that
//! `verify` doesn't need to re-derive it).
//!
//! Reuses the capability traits already built for the install steps
//! ([`vscode::VscodeInstaller`] for `--version`, [`extensions::ExtensionsRunner`]
//! for `--list-extensions`, [`runtime::RuntimeRunner`] for `-m mpremote
//! version`) rather than inventing parallel ones -- the parity harness
//! (`core/tests/verify_parity_macos.rs`) is what actually proves this
//! matches the scripts; duplicating "how do I ask code/uv/python something"
//! a fourth time would only be a second place for that logic to drift.

use crate::extensions::ExtensionsRunner;
use crate::runtime::RuntimeRunner;
use crate::state::State;
use crate::vscode::VscodeInstaller;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckId {
    CodeCliRunnable,
    ExtensionsPresent,
    MpremoteImportable,
    EnvContained,
    PythonPathSetting,
    AutoOpenPanelSetting,
    StateStepsOk,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub id: CheckId,
    pub pass: bool,
    pub message: String,
}

impl CheckResult {
    fn pass(id: CheckId, message: impl Into<String>) -> CheckResult {
        CheckResult {
            id,
            pass: true,
            message: message.into(),
        }
    }
    fn fail(id: CheckId, message: impl Into<String>) -> CheckResult {
        CheckResult {
            id,
            pass: false,
            message: message.into(),
        }
    }
}

/// `home = ...` out of `pyvenv.cfg`'s flat `key = value` lines (ports the
/// `grep -E '^home[[:space:]]*=' | sed` / `Select-String '^home\s*=\s*(.+)$'`
/// extraction).
fn parse_pyvenv_home(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("home") {
            let rest = rest.trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// Exact-or-path-boundary containment, case-folded (APFS/NTFS are both
/// case-insensitive by default, so a casing difference alone is the same
/// directory, not a breach). A bare string prefix is NOT enough -- it would
/// also accept a sibling like `Blockless-foreign`, so the boundary character
/// (`/` or `\`) right after `blk` is required. Checks both separators rather
/// than the compile target's native one so this one function is correct
/// (and testable) against a real path from either OS.
pub fn env_is_contained(pyvenv_cfg_home: &str, blk: &Path) -> bool {
    let home_l = pyvenv_cfg_home.to_lowercase();
    let blk_l = blk.to_string_lossy().to_lowercase();
    if home_l == blk_l {
        return true;
    }
    home_l.starts_with(&format!("{blk_l}/")) || home_l.starts_with(&format!("{blk_l}\\"))
}

fn check_code_cli(
    vscode: &dyn VscodeInstaller,
    candidates: &[PathBuf],
) -> (CheckResult, Option<PathBuf>) {
    for c in candidates {
        if let Some(v) = vscode.version(c) {
            return (
                CheckResult::pass(CheckId::CodeCliRunnable, format!("VS Code CLI ({v})")),
                Some(c.clone()),
            );
        }
    }
    (
        CheckResult::fail(CheckId::CodeCliRunnable, "VS Code CLI not runnable"),
        None,
    )
}

fn check_extensions(
    ext_runner: &dyn ExtensionsRunner,
    code_cli: Option<&Path>,
    profile_name: &str,
    ids: [&str; 3],
) -> CheckResult {
    let Some(code_cli) = code_cli else {
        return CheckResult::fail(
            CheckId::ExtensionsPresent,
            "extension check skipped (no code CLI)",
        );
    };
    let list = ext_runner
        .list_extensions(code_cli, profile_name)
        .unwrap_or_default();
    let all_present = ids.iter().all(|id| {
        list.iter()
            .any(|installed| installed.eq_ignore_ascii_case(id))
    });
    if all_present {
        CheckResult::pass(
            CheckId::ExtensionsPresent,
            format!("extensions in profile '{profile_name}' (blockless + python + pylance)"),
        )
    } else {
        CheckResult::fail(
            CheckId::ExtensionsPresent,
            format!("extensions missing in profile '{profile_name}'"),
        )
    }
}

fn check_mpremote(
    runtime_runner: &dyn RuntimeRunner,
    env_python: &Path,
    pinned_version: &str,
) -> CheckResult {
    if runtime_runner
        .mpremote_version(env_python)
        .is_some_and(|v| v.contains(pinned_version))
    {
        CheckResult::pass(
            CheckId::MpremoteImportable,
            format!("mpremote {pinned_version} in env"),
        )
    } else {
        CheckResult::fail(
            CheckId::MpremoteImportable,
            format!(
                "mpremote {pinned_version} not found (envPython='{}')",
                env_python.display()
            ),
        )
    }
}

fn check_env_contained(blk: &Path) -> CheckResult {
    let cfg_path = blk.join("env").join("pyvenv.cfg");
    let Ok(text) = std::fs::read_to_string(&cfg_path) else {
        return CheckResult::fail(
            CheckId::EnvContained,
            "env/pyvenv.cfg not found or unreadable",
        );
    };
    match parse_pyvenv_home(&text) {
        Some(home) if env_is_contained(&home, blk) => CheckResult::pass(
            CheckId::EnvContained,
            format!("env base interpreter is contained ({home})"),
        ),
        Some(home) => CheckResult::fail(
            CheckId::EnvContained,
            format!(
                "env base interpreter NOT contained (home='{home}', expected under {})",
                blk.display()
            ),
        ),
        None => CheckResult::fail(CheckId::EnvContained, "pyvenv.cfg has no home line"),
    }
}

fn read_profile_settings(code_user: &Path, profile_location: &str) -> Option<Value> {
    let target = code_user
        .join("profiles")
        .join(profile_location)
        .join("settings.json");
    let bytes = std::fs::read(target).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn check_python_path_setting(settings: Option<&Value>, env_python: &Path) -> CheckResult {
    let ok = settings
        .and_then(|s| s.get("mpyhw.pythonPath"))
        .and_then(Value::as_str)
        .is_some_and(|p| p == env_python.to_string_lossy() && Path::new(p).exists());
    if ok {
        CheckResult::pass(
            CheckId::PythonPathSetting,
            "mpyhw.pythonPath set in the profile settings and points at a real exe",
        )
    } else {
        CheckResult::fail(CheckId::PythonPathSetting, "mpyhw.pythonPath wrong/missing")
    }
}

fn check_auto_open_panel_setting(settings: Option<&Value>) -> CheckResult {
    let ok = settings.and_then(|s| s.get("mpyhw.autoOpenPanel")) == Some(&Value::Bool(true));
    if ok {
        CheckResult::pass(
            CheckId::AutoOpenPanelSetting,
            "mpyhw.autoOpenPanel enabled in the profile settings",
        )
    } else {
        CheckResult::fail(
            CheckId::AutoOpenPanelSetting,
            "mpyhw.autoOpenPanel not enabled",
        )
    }
}

fn check_state_steps(state: Option<&State>) -> CheckResult {
    match state {
        Some(s) if s.steps.all_ok() => {
            CheckResult::pass(CheckId::StateStepsOk, "state.json marks all four steps ok")
        }
        _ => CheckResult::fail(
            CheckId::StateStepsOk,
            "state.json missing or a step is not ok",
        ),
    }
}

pub struct VerifyInputs<'a> {
    pub code_candidates: &'a [PathBuf],
    pub profile_name: &'a str,
    pub ext_id: &'a str,
    pub py_ext_id: &'a str,
    pub pylance_id: &'a str,
    pub mpremote_version: &'a str,
    pub blk: &'a Path,
    pub code_user: &'a Path,
    pub state_path: &'a Path,
}

/// Runs all 7 checks in the scripts' order. `Ok` for every check iff the
/// whole verify passes (mirrors the scripts' `exit 0` only when `fails==0`).
pub fn run_checks(
    vscode: &dyn VscodeInstaller,
    ext_runner: &dyn ExtensionsRunner,
    runtime_runner: &dyn RuntimeRunner,
    inputs: &VerifyInputs,
) -> Vec<CheckResult> {
    let state = State::read(inputs.state_path).ok().flatten();

    let (check1, code_cli) = check_code_cli(vscode, inputs.code_candidates);
    let check2 = check_extensions(
        ext_runner,
        code_cli.as_deref(),
        inputs.profile_name,
        [inputs.ext_id, inputs.py_ext_id, inputs.pylance_id],
    );

    let env_python = state
        .as_ref()
        .map(|s| PathBuf::from(&s.env_python))
        .unwrap_or_default();
    let check3 = check_mpremote(runtime_runner, &env_python, inputs.mpremote_version);
    let check3b = check_env_contained(inputs.blk);

    let settings = state
        .as_ref()
        .and_then(|s| read_profile_settings(inputs.code_user, &s.profile_location));
    let check4 = check_python_path_setting(settings.as_ref(), &env_python);
    let check4b = check_auto_open_panel_setting(settings.as_ref());

    let check5 = check_state_steps(state.as_ref());

    vec![check1, check2, check3, check3b, check4, check4b, check5]
}

pub fn all_pass(results: &[CheckResult]) -> bool {
    results.iter().all(|r| r.pass)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Steps;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "blockless-installer-verify-test-{name}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- env_is_contained ---

    #[test]
    fn env_contained_exact_match() {
        let blk = Path::new("/Users/erson/Library/Application Support/Blockless");
        assert!(env_is_contained(
            "/Users/erson/Library/Application Support/Blockless",
            blk
        ));
    }

    #[test]
    fn env_contained_subdir_boundary() {
        let blk = Path::new("/Users/erson/Library/Application Support/Blockless");
        assert!(env_is_contained(
            "/Users/erson/Library/Application Support/Blockless/python/cpython-3.12.4",
            blk
        ));
    }

    #[test]
    fn env_contained_case_folded() {
        let blk = Path::new("/Users/erson/Library/Application Support/Blockless");
        assert!(env_is_contained(
            "/USERS/ERSON/LIBRARY/APPLICATION SUPPORT/BLOCKLESS/python",
            blk
        ));
    }

    #[test]
    fn env_contained_windows_separator() {
        let blk = Path::new(r"C:\Users\erson\AppData\Local\Blockless");
        assert!(env_is_contained(
            r"C:\Users\erson\AppData\Local\Blockless\python\cpython-3.12.4",
            blk
        ));
    }

    #[test]
    fn env_not_contained_sibling_dir_negative() {
        // the exact scenario this guard exists for: a bare-prefix match
        // would wrongly accept a sibling directory that merely starts with
        // the same characters.
        let blk = Path::new("/Users/erson/Library/Application Support/Blockless");
        assert!(!env_is_contained(
            "/Users/erson/Library/Application Support/Blockless-foreign/python",
            blk
        ));
    }

    #[test]
    fn env_not_contained_unrelated_path() {
        let blk = Path::new("/Users/erson/Library/Application Support/Blockless");
        assert!(!env_is_contained("/opt/anaconda3", blk));
    }

    // --- parse_pyvenv_home ---

    #[test]
    fn parses_home_line_with_surrounding_whitespace() {
        let cfg = "home = /Users/erson/Library/Application Support/Blockless/python/cpython-3.12.4\nversion = 3.12.4\n";
        assert_eq!(
            parse_pyvenv_home(cfg).as_deref(),
            Some("/Users/erson/Library/Application Support/Blockless/python/cpython-3.12.4")
        );
    }

    #[test]
    fn missing_home_line_returns_none() {
        assert_eq!(parse_pyvenv_home("version = 3.12.4\n"), None);
    }

    // --- fakes shared by the check-level and run_checks tests ---

    struct FakeVscode {
        versions: HashMap<PathBuf, String>,
    }
    impl VscodeInstaller for FakeVscode {
        fn version(&self, code_cli: &Path) -> Option<String> {
            self.versions.get(code_cli).cloned()
        }
        fn is_writable(&self, _dir: &Path) -> bool {
            unimplemented!("verify never installs")
        }
        fn extract_archive(
            &self,
            _archive: &Path,
            _target_dir: &Path,
        ) -> Result<(), crate::vscode::InstallError> {
            unimplemented!("verify never installs")
        }
        fn strip_quarantine(&self, _app_dir: &Path) {
            unimplemented!("verify never installs")
        }
        fn run_silent_installer(
            &self,
            _installer_exe: &Path,
        ) -> Result<(), crate::vscode::InstallError> {
            unimplemented!("verify never installs")
        }
        fn verify_signature(&self, _artifact: &Path) -> Result<(), crate::vscode::SignatureError> {
            unimplemented!("verify never installs")
        }
    }

    struct FakeExtensions {
        installed: RefCell<Vec<String>>,
        fail_listing: bool,
    }
    impl ExtensionsRunner for FakeExtensions {
        fn list_extensions(&self, _code_cli: &Path, _profile_name: &str) -> Option<Vec<String>> {
            if self.fail_listing {
                None
            } else {
                Some(self.installed.borrow().clone())
            }
        }
        fn install_extension(
            &self,
            _code_cli: &Path,
            _profile_name: &str,
            _vsix_or_id: &str,
        ) -> bool {
            unimplemented!("verify never installs")
        }
    }

    struct FakeRuntime {
        mpremote: Option<String>,
    }
    impl RuntimeRunner for FakeRuntime {
        fn mpremote_version(&self, _envpy: &Path) -> Option<String> {
            self.mpremote.clone()
        }
        fn uv_version(&self, _uv_bin: &Path) -> Option<String> {
            unimplemented!("verify never provisions")
        }
        fn extract_uv(&self, _archive: &Path, _dest_dir: &Path) -> Result<(), String> {
            unimplemented!("verify never provisions")
        }
        fn run_uv(&self, _uv_bin: &Path, _args: &[&str], _env: &[(&str, &str)]) -> bool {
            unimplemented!("verify never provisions")
        }
    }

    // --- individual check functions ---

    #[test]
    fn check_code_cli_pass_on_second_candidate() {
        let mut versions = HashMap::new();
        versions.insert(PathBuf::from("/candidate/2"), "1.99.0".to_string());
        let vscode = FakeVscode { versions };
        let candidates = vec![PathBuf::from("/candidate/1"), PathBuf::from("/candidate/2")];
        let (result, resolved) = check_code_cli(&vscode, &candidates);
        assert!(result.pass);
        assert_eq!(resolved, Some(PathBuf::from("/candidate/2")));
    }

    #[test]
    fn check_code_cli_fail_when_no_candidate_resolves() {
        let vscode = FakeVscode {
            versions: HashMap::new(),
        };
        let candidates = vec![PathBuf::from("/candidate/1")];
        let (result, resolved) = check_code_cli(&vscode, &candidates);
        assert!(!result.pass);
        assert_eq!(resolved, None);
    }

    #[test]
    fn check_extensions_skips_when_no_code_cli() {
        let ext = FakeExtensions {
            installed: RefCell::new(vec![]),
            fail_listing: false,
        };
        let result = check_extensions(&ext, None, "Blockless", ["a", "b", "c"]);
        assert!(!result.pass);
        assert!(result.message.contains("skipped"));
    }

    #[test]
    fn check_extensions_case_insensitive_all_three() {
        let ext = FakeExtensions {
            installed: RefCell::new(vec![
                "Blockless.MPY-Hardware-Extension".to_string(),
                "ms-python.python".to_string(),
                "MS-PYTHON.VSCODE-PYLANCE".to_string(),
            ]),
            fail_listing: false,
        };
        let result = check_extensions(
            &ext,
            Some(Path::new("/code")),
            "Blockless",
            [
                "blockless.mpy-hardware-extension",
                "ms-python.python",
                "ms-python.vscode-pylance",
            ],
        );
        assert!(result.pass);
    }

    #[test]
    fn check_extensions_fails_when_one_missing() {
        let ext = FakeExtensions {
            installed: RefCell::new(vec![
                "blockless.mpy-hardware-extension".to_string(),
                "ms-python.python".to_string(),
            ]),
            fail_listing: false,
        };
        let result = check_extensions(
            &ext,
            Some(Path::new("/code")),
            "Blockless",
            [
                "blockless.mpy-hardware-extension",
                "ms-python.python",
                "ms-python.vscode-pylance",
            ],
        );
        assert!(!result.pass);
    }

    #[test]
    fn check_mpremote_pass_and_fail() {
        let ok = FakeRuntime {
            mpremote: Some("mpremote 1.28.0".to_string()),
        };
        assert!(check_mpremote(&ok, Path::new("/env/python"), "1.28.0").pass);

        let wrong = FakeRuntime {
            mpremote: Some("mpremote 1.20.0".to_string()),
        };
        assert!(!check_mpremote(&wrong, Path::new("/env/python"), "1.28.0").pass);

        let missing = FakeRuntime { mpremote: None };
        assert!(!check_mpremote(&missing, Path::new("/env/python"), "1.28.0").pass);
    }

    #[test]
    fn check_env_contained_from_real_file() {
        let dir = temp_dir("env-contained-file");
        let blk = dir.join("Blockless");
        std::fs::create_dir_all(blk.join("env")).unwrap();
        std::fs::write(
            blk.join("env").join("pyvenv.cfg"),
            format!(
                "home = {}\n",
                blk.join("python").join("cpython-3.12.4").display()
            ),
        )
        .unwrap();
        assert!(check_env_contained(&blk).pass);
    }

    #[test]
    fn check_env_contained_missing_file_fails() {
        let dir = temp_dir("env-contained-missing");
        let blk = dir.join("Blockless");
        assert!(!check_env_contained(&blk).pass);
    }

    #[test]
    fn check_python_path_and_auto_open_panel() {
        let dir = temp_dir("settings-checks");
        let envpy = dir.join("python");
        std::fs::write(&envpy, b"#!/bin/sh\n").unwrap();

        let good = serde_json::json!({
            "mpyhw.pythonPath": envpy.to_string_lossy(),
            "mpyhw.autoOpenPanel": true,
        });
        assert!(check_python_path_setting(Some(&good), &envpy).pass);
        assert!(check_auto_open_panel_setting(Some(&good)).pass);

        let wrong = serde_json::json!({
            "mpyhw.pythonPath": "/some/other/python",
            "mpyhw.autoOpenPanel": false,
        });
        assert!(!check_python_path_setting(Some(&wrong), &envpy).pass);
        assert!(!check_auto_open_panel_setting(Some(&wrong)).pass);

        assert!(!check_python_path_setting(None, &envpy).pass);
        assert!(!check_auto_open_panel_setting(None).pass);
    }

    #[test]
    fn check_python_path_fails_when_file_does_not_exist_on_disk() {
        let dir = temp_dir("settings-nonexistent-exe");
        let envpy = dir.join("does-not-exist");
        let settings = serde_json::json!({"mpyhw.pythonPath": envpy.to_string_lossy()});
        assert!(!check_python_path_setting(Some(&settings), &envpy).pass);
    }

    #[test]
    fn check_state_steps_all_variants() {
        let ok = State {
            steps: Steps {
                vscode: true,
                extension: true,
                python: true,
                settings: true,
            },
            ..Default::default()
        };
        assert!(check_state_steps(Some(&ok)).pass);

        let one_false = State {
            steps: Steps {
                vscode: true,
                extension: false,
                python: true,
                settings: true,
            },
            ..Default::default()
        };
        assert!(!check_state_steps(Some(&one_false)).pass);

        assert!(!check_state_steps(None).pass);
    }

    // --- run_checks end to end ---

    fn write_json(path: &Path, value: &serde_json::Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    #[test]
    fn run_checks_all_green_fixture() {
        let dir = temp_dir("run-checks-green");
        let blk = dir.join("Blockless");
        let code_user = dir.join("Code").join("User");
        // Under a `bin/` dir, not beside `Code/`, and the nesting is load-bearing rather than
        // cosmetic: macOS and Windows are case-insensitive, so a file at `dir/code` IS the path
        // `dir/Code`, and creating the profile tree under `code_user` then fails ENOTDIR. This
        // passes on a case-sensitive filesystem, so Linux CI cannot see it. The real CLI lives at
        // .../Resources/app/bin/code anyway, which is why the script-parity fixture never hit this.
        let code_cli = dir.join("bin").join("code");
        let envpy = blk.join("env").join("bin").join("python");
        std::fs::create_dir_all(code_cli.parent().unwrap()).unwrap();
        std::fs::write(&code_cli, b"").unwrap();
        std::fs::create_dir_all(envpy.parent().unwrap()).unwrap();
        std::fs::write(&envpy, b"").unwrap();

        std::fs::create_dir_all(blk.join("env")).unwrap();
        std::fs::write(
            blk.join("env").join("pyvenv.cfg"),
            format!(
                "home = {}\n",
                blk.join("python").join("cpython-3.12.4").display()
            ),
        )
        .unwrap();

        let state = State {
            product_version: "1.99.0".to_string(),
            vscode_installed_by_us: true,
            profile_created_by_us: true,
            profile_location: "blockless".to_string(),
            steps: Steps {
                vscode: true,
                extension: true,
                python: true,
                settings: true,
            },
            mpremote_version: "1.28.0".to_string(),
            env_python: envpy.to_string_lossy().into_owned(),
            ext_vsix_sha256: "abc123".to_string(),
            settings_mechanism: "A".to_string(),
            updated_at: "2026-07-05T12:00:00Z".to_string(),
        };
        let state_path = blk.join("state.json");
        std::fs::create_dir_all(&blk).unwrap();
        state.write(&state_path).unwrap();

        write_json(
            &code_user
                .join("profiles")
                .join("blockless")
                .join("settings.json"),
            &serde_json::json!({
                "mpyhw.pythonPath": envpy.to_string_lossy(),
                "workbench.colorTheme": "Default Dark Modern",
                "mpyhw.autoOpenPanel": true,
                "workbench.secondarySideBar.defaultVisibility": "hidden",
            }),
        );

        let mut versions = HashMap::new();
        versions.insert(code_cli.clone(), "1.99.0".to_string());
        let vscode = FakeVscode { versions };
        let ext = FakeExtensions {
            installed: RefCell::new(vec![
                "blockless.mpy-hardware-extension".to_string(),
                "ms-python.python".to_string(),
                "ms-python.vscode-pylance".to_string(),
            ]),
            fail_listing: false,
        };
        let runtime = FakeRuntime {
            mpremote: Some("mpremote 1.28.0".to_string()),
        };

        let inputs = VerifyInputs {
            code_candidates: &[code_cli],
            profile_name: "Blockless",
            ext_id: "blockless.mpy-hardware-extension",
            py_ext_id: "ms-python.python",
            pylance_id: "ms-python.vscode-pylance",
            mpremote_version: "1.28.0",
            blk: &blk,
            code_user: &code_user,
            state_path: &state_path,
        };

        let results = run_checks(&vscode, &ext, &runtime, &inputs);
        assert_eq!(results.len(), 7);
        assert!(all_pass(&results), "{results:#?}");
    }

    #[test]
    fn run_checks_fails_when_state_json_missing() {
        let dir = temp_dir("run-checks-no-state");
        let blk = dir.join("Blockless");
        let code_user = dir.join("Code").join("User");
        let vscode = FakeVscode {
            versions: HashMap::new(),
        };
        let ext = FakeExtensions {
            installed: RefCell::new(vec![]),
            fail_listing: false,
        };
        let runtime = FakeRuntime { mpremote: None };
        let inputs = VerifyInputs {
            code_candidates: &[],
            profile_name: "Blockless",
            ext_id: "a",
            py_ext_id: "b",
            pylance_id: "c",
            mpremote_version: "1.28.0",
            blk: &blk,
            code_user: &code_user,
            state_path: &blk.join("state.json"),
        };
        let results = run_checks(&vscode, &ext, &runtime, &inputs);
        assert!(!all_pass(&results));
    }

    #[test]
    fn settings_checks_use_the_journaled_location_not_a_seed_constant() {
        // profileLocation is a VS-Code-created fallback hash id, deliberately
        // NOT "blockless" -- settings.json lives ONLY at that hashed path.
        // If the settings checks ever hardcoded "blockless" instead of
        // reading state.profile_location, they'd look in the wrong place
        // and fail even though the real settings are correct.
        let dir = temp_dir("journaled-location");
        let blk = dir.join("Blockless");
        let code_user = dir.join("Code").join("User");
        let envpy = blk.join("env").join("bin").join("python");
        std::fs::create_dir_all(envpy.parent().unwrap()).unwrap();
        std::fs::write(&envpy, b"").unwrap();
        std::fs::create_dir_all(blk.join("env")).unwrap();
        std::fs::write(
            blk.join("env").join("pyvenv.cfg"),
            format!(
                "home = {}\n",
                blk.join("python").join("cpython-3.12.4").display()
            ),
        )
        .unwrap();

        let state = State {
            profile_location: "a1b2c3d4e5f6".to_string(),
            steps: Steps {
                vscode: true,
                extension: true,
                python: true,
                settings: true,
            },
            mpremote_version: "1.28.0".to_string(),
            env_python: envpy.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let state_path = blk.join("state.json");
        std::fs::create_dir_all(&blk).unwrap();
        state.write(&state_path).unwrap();

        write_json(
            &code_user
                .join("profiles")
                .join("a1b2c3d4e5f6")
                .join("settings.json"),
            &serde_json::json!({
                "mpyhw.pythonPath": envpy.to_string_lossy(),
                "mpyhw.autoOpenPanel": true,
            }),
        );
        // deliberately no settings.json at the seed-constant path, so a
        // hardcoded "blockless" lookup would find nothing

        let vscode = FakeVscode {
            versions: HashMap::new(),
        };
        let ext = FakeExtensions {
            installed: RefCell::new(vec![]),
            fail_listing: false,
        };
        let runtime = FakeRuntime {
            mpremote: Some("mpremote 1.28.0".to_string()),
        };
        let inputs = VerifyInputs {
            code_candidates: &[],
            profile_name: "Blockless",
            ext_id: "a",
            py_ext_id: "b",
            pylance_id: "c",
            mpremote_version: "1.28.0",
            blk: &blk,
            code_user: &code_user,
            state_path: &state_path,
        };

        let results = run_checks(&vscode, &ext, &runtime, &inputs);
        let by_id = |id: CheckId| results.iter().find(|r| r.id == id).unwrap();
        assert!(by_id(CheckId::PythonPathSetting).pass, "{results:#?}");
        assert!(by_id(CheckId::AutoOpenPanelSetting).pass, "{results:#?}");
    }
}
