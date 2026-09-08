//! OS/arch detection and the one-folder path set every later module builds on.
//!
//! Ports `resolve_code`/`Resolve-Code` and the path constants at the top of the M0
//! scripts (`scripts/{macos,windows}/install-blockless.{zsh,ps1}`).
//!
//! Every function here takes an injected [`RawEnv`] instead of reading `std::env`
//! directly, so path resolution and arch detection are unit-testable on any host
//! (including the ubuntu CI runner) without touching real environment variables.
//! Path segments are always joined one component at a time (never a single string
//! with embedded `/` or `\`), so the same code is correct whether the process
//! actually runs on macOS/Windows or is exercised in a test on a third host: the
//! separator `Path::join` inserts is always the compile target's real separator,
//! never a literal baked into a string.

use std::path::PathBuf;

/// The two platforms the installer supports. Linux is out of scope for the
/// installer itself (see `/scope.md`'s "Out of scope"); it only ever appears as
/// the CI runner, never as a value this enum takes on there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    MacOs,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    X64,
    Arm64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlatformError {
    #[error("unsupported OS '{0}' (blockless-installer supports macOS and Windows only)")]
    UnsupportedOs(String),
    #[error("required environment variable {0} is not set")]
    MissingEnvVar(&'static str),
}

/// The raw inputs platform resolution needs, gathered once from the real process
/// environment (or fabricated by a test). No module reads `std::env` on its own.
#[derive(Debug, Clone, Default)]
pub struct RawEnv {
    /// `std::env::consts::OS` in production ("macos" | "windows" | ...).
    pub target_os: String,
    /// `std::env::consts::ARCH` in production ("aarch64" | "x86_64" | ...).
    pub target_arch: String,
    /// `$HOME` (macOS).
    pub home: Option<String>,
    /// `%APPDATA%` (Windows).
    pub appdata: Option<String>,
    /// `%LOCALAPPDATA%` (Windows).
    pub local_appdata: Option<String>,
    /// `%PROCESSOR_ARCHITECTURE%` (Windows). Read explicitly rather than inferred
    /// from the compiled target, matching the M0 script's win32-arm64-user vs
    /// win32-x64-user split (`$env:PROCESSOR_ARCHITECTURE -eq "ARM64"`).
    pub processor_architecture: Option<String>,
    /// The system-wide applications directory (macOS), `/Applications` when unset.
    ///
    /// Every other path here derives from an environment variable, so a test can
    /// redirect it by fabricating a `RawEnv`. The system `code` candidate could
    /// not: it was the absolute literal `/Applications`, which no fabricated
    /// value can shadow. On any machine with VS Code actually installed, the
    /// script-parity suite therefore resolved the REAL editor and answered its
    /// checks about that machine rather than the fixture -- invisibly on Linux,
    /// where `/Applications` does not exist and resolution fell through to the
    /// fixture's stub. `BLOCKLESS_APPS_ROOT` is the same override the M0 verify
    /// script reads, so both sides of the parity comparison redirect together.
    pub apps_root: Option<String>,
}

impl RawEnv {
    /// Read the real process environment.
    pub fn from_process() -> Self {
        RawEnv {
            target_os: std::env::consts::OS.to_string(),
            target_arch: std::env::consts::ARCH.to_string(),
            home: std::env::var("HOME").ok(),
            appdata: std::env::var("APPDATA").ok(),
            local_appdata: std::env::var("LOCALAPPDATA").ok(),
            processor_architecture: std::env::var("PROCESSOR_ARCHITECTURE").ok(),
            apps_root: std::env::var("BLOCKLESS_APPS_ROOT").ok(),
        }
    }
}

impl Os {
    pub fn detect(raw: &RawEnv) -> Result<Os, PlatformError> {
        match raw.target_os.as_str() {
            "macos" => Ok(Os::MacOs),
            "windows" => Ok(Os::Windows),
            other => Err(PlatformError::UnsupportedOs(other.to_string())),
        }
    }
}

impl Arch {
    pub fn detect(os: Os, raw: &RawEnv) -> Result<Arch, PlatformError> {
        match os {
            // Windows: PROCESSOR_ARCHITECTURE only, never inferred from the build
            // target -- this is what picks win32-arm64-user vs win32-x64-user in
            // the manifest resolver.
            Os::Windows => {
                let pa = raw
                    .processor_architecture
                    .as_deref()
                    .ok_or(PlatformError::MissingEnvVar("PROCESSOR_ARCHITECTURE"))?;
                Ok(if pa.eq_ignore_ascii_case("ARM64") {
                    Arch::Arm64
                } else {
                    Arch::X64
                })
            }
            // macOS: the manifest's uv pins are split darwin-aarch64/darwin-x86_64,
            // so the process's own arch decides which binary to fetch.
            Os::MacOs => Ok(match raw.target_arch.as_str() {
                "aarch64" => Arch::Arm64,
                _ => Arch::X64,
            }),
        }
    }
}

/// The one-folder path set every step reads or writes. Field names match the
/// script's shell/PS variable names (`BLK`, `CODE_USER`, `STORAGE`, `ENVPY`, ...)
/// so a reviewer can line them up directly against the executable spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `~/Library/Application Support/Blockless` (mac) / `%LOCALAPPDATA%\Blockless` (win).
    pub blk: PathBuf,
    pub downloads: PathBuf,
    pub logs: PathBuf,
    pub state: PathBuf,
    /// VS Code's own per-user data dir (`Code/User`), not ours -- we only ever
    /// read/write specific files under it (`storage.json`, a profile's `settings.json`).
    pub code_user: PathBuf,
    pub storage: PathBuf,
    pub env_python: PathBuf,
}

impl Paths {
    pub fn resolve(os: Os, raw: &RawEnv) -> Result<Paths, PlatformError> {
        match os {
            Os::MacOs => {
                let home = raw
                    .home
                    .as_deref()
                    .ok_or(PlatformError::MissingEnvVar("HOME"))?;
                let home = PathBuf::from(home);
                let blk = home
                    .join("Library")
                    .join("Application Support")
                    .join("Blockless");
                let code_user = home
                    .join("Library")
                    .join("Application Support")
                    .join("Code")
                    .join("User");
                let storage = code_user.join("globalStorage").join("storage.json");
                let env_python = blk.join("env").join("bin").join("python");
                Ok(Paths {
                    downloads: blk.join("downloads"),
                    logs: blk.join("logs"),
                    state: blk.join("state.json"),
                    blk,
                    code_user,
                    storage,
                    env_python,
                })
            }
            Os::Windows => {
                let local_appdata = raw
                    .local_appdata
                    .as_deref()
                    .ok_or(PlatformError::MissingEnvVar("LOCALAPPDATA"))?;
                let appdata = raw
                    .appdata
                    .as_deref()
                    .ok_or(PlatformError::MissingEnvVar("APPDATA"))?;
                let blk = PathBuf::from(local_appdata).join("Blockless");
                let code_user = PathBuf::from(appdata).join("Code").join("User");
                let storage = code_user.join("globalStorage").join("storage.json");
                let env_python = blk.join("env").join("Scripts").join("python.exe");
                Ok(Paths {
                    downloads: blk.join("downloads"),
                    logs: blk.join("logs"),
                    state: blk.join("state.json"),
                    blk,
                    code_user,
                    storage,
                    env_python,
                })
            }
        }
    }
}

/// Explicit `code` CLI install-location candidates, in priority order. Never
/// resolved via `PATH` -- a fresh machine may not have one, and a stale `code`
/// shim earlier in PATH must never shadow the copy this installer manages.
/// Existence/`--version` checking happens in `vscode.rs`; this only enumerates
/// where to look, mirroring `resolve_code`/`Resolve-Code` in the M0 scripts.
pub fn code_cli_candidates(os: Os, raw: &RawEnv) -> Result<Vec<PathBuf>, PlatformError> {
    match os {
        Os::MacOs => {
            let home = raw
                .home
                .as_deref()
                .ok_or(PlatformError::MissingEnvVar("HOME"))?;
            let suffix = |base: PathBuf| -> PathBuf {
                base.join("Visual Studio Code.app")
                    .join("Contents")
                    .join("Resources")
                    .join("app")
                    .join("bin")
                    .join("code")
            };
            let apps_root = raw.apps_root.as_deref().unwrap_or("/Applications");
            Ok(vec![
                suffix(PathBuf::from(apps_root)),
                suffix(PathBuf::from(home).join("Applications")),
            ])
        }
        Os::Windows => {
            let local_appdata = raw
                .local_appdata
                .as_deref()
                .ok_or(PlatformError::MissingEnvVar("LOCALAPPDATA"))?;
            Ok(vec![PathBuf::from(local_appdata)
                .join("Programs")
                .join("Microsoft VS Code")
                .join("bin")
                .join("code.cmd")])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac_env(home: &str) -> RawEnv {
        RawEnv {
            target_os: "macos".to_string(),
            target_arch: "aarch64".to_string(),
            home: Some(home.to_string()),
            ..Default::default()
        }
    }

    fn windows_env(appdata: &str, local_appdata: &str) -> RawEnv {
        RawEnv {
            target_os: "windows".to_string(),
            target_arch: "x86_64".to_string(),
            appdata: Some(appdata.to_string()),
            local_appdata: Some(local_appdata.to_string()),
            processor_architecture: Some("AMD64".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn detects_macos() {
        let raw = mac_env("/Users/erson");
        assert_eq!(Os::detect(&raw), Ok(Os::MacOs));
    }

    #[test]
    fn detects_windows() {
        let raw = windows_env(
            r"C:\Users\erson\AppData\Roaming",
            r"C:\Users\erson\AppData\Local",
        );
        assert_eq!(Os::detect(&raw), Ok(Os::Windows));
    }

    #[test]
    fn rejects_unsupported_os() {
        let raw = RawEnv {
            target_os: "linux".to_string(),
            ..Default::default()
        };
        assert_eq!(
            Os::detect(&raw),
            Err(PlatformError::UnsupportedOs("linux".to_string()))
        );
    }

    #[test]
    fn mac_paths_are_under_home() {
        let raw = mac_env("/Users/erson");
        let paths = Paths::resolve(Os::MacOs, &raw).unwrap();
        let home = PathBuf::from("/Users/erson");
        let blk = home
            .join("Library")
            .join("Application Support")
            .join("Blockless");
        assert_eq!(paths.blk, blk);
        assert_eq!(paths.downloads, blk.join("downloads"));
        assert_eq!(paths.logs, blk.join("logs"));
        assert_eq!(paths.state, blk.join("state.json"));
        assert_eq!(paths.env_python, blk.join("env").join("bin").join("python"));
        let code_user = home
            .join("Library")
            .join("Application Support")
            .join("Code")
            .join("User");
        assert_eq!(paths.code_user, code_user);
        assert_eq!(
            paths.storage,
            code_user.join("globalStorage").join("storage.json")
        );
    }

    #[test]
    fn mac_paths_missing_home_errors() {
        let raw = RawEnv {
            target_os: "macos".to_string(),
            ..Default::default()
        };
        assert_eq!(
            Paths::resolve(Os::MacOs, &raw),
            Err(PlatformError::MissingEnvVar("HOME"))
        );
    }

    #[test]
    fn windows_paths_split_appdata_from_local_appdata() {
        let appdata = r"C:\Users\erson\AppData\Roaming";
        let local_appdata = r"C:\Users\erson\AppData\Local";
        let raw = windows_env(appdata, local_appdata);
        let paths = Paths::resolve(Os::Windows, &raw).unwrap();
        let blk = PathBuf::from(local_appdata).join("Blockless");
        assert_eq!(paths.blk, blk);
        assert_eq!(paths.downloads, blk.join("downloads"));
        assert_eq!(paths.logs, blk.join("logs"));
        assert_eq!(paths.state, blk.join("state.json"));
        assert_eq!(
            paths.env_python,
            blk.join("env").join("Scripts").join("python.exe")
        );
        let code_user = PathBuf::from(appdata).join("Code").join("User");
        assert_eq!(paths.code_user, code_user);
        assert_eq!(
            paths.storage,
            code_user.join("globalStorage").join("storage.json")
        );
    }

    #[test]
    fn windows_paths_missing_local_appdata_errors() {
        let raw = RawEnv {
            target_os: "windows".to_string(),
            appdata: Some(r"C:\Users\erson\AppData\Roaming".to_string()),
            ..Default::default()
        };
        assert_eq!(
            Paths::resolve(Os::Windows, &raw),
            Err(PlatformError::MissingEnvVar("LOCALAPPDATA"))
        );
    }

    #[test]
    fn windows_paths_missing_appdata_errors() {
        let raw = RawEnv {
            target_os: "windows".to_string(),
            local_appdata: Some(r"C:\Users\erson\AppData\Local".to_string()),
            ..Default::default()
        };
        assert_eq!(
            Paths::resolve(Os::Windows, &raw),
            Err(PlatformError::MissingEnvVar("APPDATA"))
        );
    }

    #[test]
    fn mac_code_cli_candidates_prefer_applications_then_home() {
        let raw = mac_env("/Users/erson");
        let candidates = code_cli_candidates(Os::MacOs, &raw).unwrap();
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/Applications")
                    .join("Visual Studio Code.app")
                    .join("Contents")
                    .join("Resources")
                    .join("app")
                    .join("bin")
                    .join("code"),
                PathBuf::from("/Users/erson")
                    .join("Applications")
                    .join("Visual Studio Code.app")
                    .join("Contents")
                    .join("Resources")
                    .join("app")
                    .join("bin")
                    .join("code"),
            ]
        );
    }

    #[test]
    fn windows_code_cli_candidate_is_under_local_appdata() {
        let local_appdata = r"C:\Users\erson\AppData\Local";
        let raw = windows_env(r"C:\Users\erson\AppData\Roaming", local_appdata);
        let candidates = code_cli_candidates(Os::Windows, &raw).unwrap();
        assert_eq!(
            candidates,
            vec![PathBuf::from(local_appdata)
                .join("Programs")
                .join("Microsoft VS Code")
                .join("bin")
                .join("code.cmd")]
        );
    }

    #[test]
    fn windows_arch_reads_processor_architecture_arm64() {
        let raw = RawEnv {
            target_os: "windows".to_string(),
            processor_architecture: Some("ARM64".to_string()),
            ..Default::default()
        };
        assert_eq!(Arch::detect(Os::Windows, &raw), Ok(Arch::Arm64));
    }

    #[test]
    fn windows_arch_reads_processor_architecture_amd64() {
        let raw = RawEnv {
            target_os: "windows".to_string(),
            processor_architecture: Some("AMD64".to_string()),
            ..Default::default()
        };
        assert_eq!(Arch::detect(Os::Windows, &raw), Ok(Arch::X64));
    }

    #[test]
    fn windows_arch_missing_processor_architecture_errors() {
        let raw = RawEnv {
            target_os: "windows".to_string(),
            ..Default::default()
        };
        assert_eq!(
            Arch::detect(Os::Windows, &raw),
            Err(PlatformError::MissingEnvVar("PROCESSOR_ARCHITECTURE"))
        );
    }

    #[test]
    fn mac_arch_never_consults_processor_architecture() {
        // A macOS process would never see PROCESSOR_ARCHITECTURE set, but even if
        // it were (e.g. carried over from a cross-build harness), mac arch must
        // come from the process's own arch, never that Windows-only variable.
        let raw = RawEnv {
            target_os: "macos".to_string(),
            target_arch: "x86_64".to_string(),
            processor_architecture: Some("ARM64".to_string()),
            ..Default::default()
        };
        assert_eq!(Arch::detect(Os::MacOs, &raw), Ok(Arch::X64));
    }

    #[test]
    fn mac_arch_detects_arm64() {
        let raw = RawEnv {
            target_os: "macos".to_string(),
            target_arch: "aarch64".to_string(),
            ..Default::default()
        };
        assert_eq!(Arch::detect(Os::MacOs, &raw), Ok(Arch::Arm64));
    }
}
