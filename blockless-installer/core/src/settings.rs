//! Step 4: the branded profile settings (mechanism A only -- the per-profile
//! `settings.json`, never the user's default settings.json; mechanism B was
//! removed after the fresh-VM A/B experiment settled it). Ports
//! `merge_settings`/`Merge-Settings` and `settings_already`/(the inline
//! PS equivalent), plus the profile-location resolution at the top of
//! `step4_settings`/`Step-Settings`.
//!
//! Reuses `profile.rs`'s resolver + both registration paths directly: step 4
//! can be what FINALLY registers the profile if steps 2/3 somehow left it
//! unregistered, so it needs the same seed-first/window-fallback-second
//! sequence available to it too.

use crate::manifest::ManifestSettings;
use crate::profile;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("could not resolve the '{0}' profile location")]
    ProfileNotResolved(String),
    /// An EXISTING settings.json we cannot parse (VS Code settings are
    /// JSONC, may carry comments) must never be clobbered down to just our
    /// four keys -- it may hold the user's own edits. Refuse loudly instead.
    #[error("refusing to overwrite an unreadable settings.json at {}", .0.display())]
    Unparseable(PathBuf),
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not register the profile: {0}")]
    Profile(#[from] profile::ProfileError),
}

fn desired_entries(env_python: &Path, s: &ManifestSettings) -> Vec<(&'static str, Value)> {
    vec![
        (
            "mpyhw.pythonPath",
            Value::String(env_python.to_string_lossy().into_owned()),
        ),
        ("workbench.colorTheme", Value::String(s.color_theme.clone())),
        ("mpyhw.autoOpenPanel", Value::Bool(s.auto_open_panel)),
        (
            "workbench.secondarySideBar.defaultVisibility",
            Value::String(s.secondary_side_bar_default_visibility.clone()),
        ),
    ]
}

/// All FOUR installer-owned keys must match, not just `mpyhw.pythonPath`: a
/// repair run must re-apply the theme, `autoOpenPanel`, or secondary side
/// bar visibility if any was removed or changed, or the branded UI / panel
/// auto-open stays broken while this reports "already applied".
fn settings_already(target: &Path, env_python: &Path, s: &ManifestSettings) -> bool {
    let Ok(bytes) = std::fs::read(target) else {
        return false;
    };
    let Ok(existing) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    let Some(obj) = existing.as_object() else {
        return false;
    };
    desired_entries(env_python, s)
        .iter()
        .all(|(k, v)| obj.get(*k) == Some(v))
}

fn merge_settings(
    target: &Path,
    env_python: &Path,
    s: &ManifestSettings,
) -> Result<(), SettingsError> {
    let mut root = if target.exists() {
        let bytes = std::fs::read(target).map_err(|source| SettingsError::Read {
            path: target.to_path_buf(),
            source,
        })?;
        serde_json::from_slice::<Value>(&bytes)
            .map_err(|_| SettingsError::Unparseable(target.to_path_buf()))?
    } else {
        Value::Object(Default::default())
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| SettingsError::Unparseable(target.to_path_buf()))?;
    for (k, v) in desired_entries(env_python, s) {
        obj.insert(k.to_string(), v);
    }
    let body = serde_json::to_vec_pretty(&root).expect("Value always serializes");
    write_atomic(target, &body).map_err(|source| SettingsError::Write {
        path: target.to_path_buf(),
        source,
    })
}

fn write_atomic(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.json".to_string());
    let tmp = parent.join(format!("{file_name}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, dest)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsStepOutcome {
    /// The RESOLVED on-disk id -- journal THIS, not the seed constant. Step
    /// 4 may be what finally registers the profile (via the same seed/
    /// window-fallback sequence `profile.rs` exposes), so this can differ
    /// from whatever location earlier steps assumed.
    pub profile_location: String,
    pub applied: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn ensure_settings(
    command_runner: &dyn profile::CommandRunner,
    code_cli: &Path,
    storage_path: &Path,
    profiles_dir: &Path,
    profile_name: &str,
    seed_location: &str,
    env_python: &Path,
    manifest_settings: &ManifestSettings,
) -> Result<SettingsStepOutcome, SettingsError> {
    let mut loc = profile::resolve_profile_location(storage_path, profile_name);
    if loc.is_none() {
        profile::register_profile_offline(
            command_runner,
            storage_path,
            profiles_dir,
            profile_name,
            seed_location,
        )?;
        loc = profile::resolve_profile_location(storage_path, profile_name);
    }
    if loc.is_none() {
        profile::register_profile(command_runner, code_cli, storage_path, profile_name);
        loc = profile::resolve_profile_location(storage_path, profile_name);
    }
    let loc = loc.ok_or_else(|| SettingsError::ProfileNotResolved(profile_name.to_string()))?;

    let target = profiles_dir.join(&loc).join("settings.json");

    if settings_already(&target, env_python, manifest_settings) {
        return Ok(SettingsStepOutcome {
            profile_location: loc,
            applied: false,
        });
    }

    merge_settings(&target, env_python, manifest_settings)?;

    Ok(SettingsStepOutcome {
        profile_location: loc,
        applied: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "blockless-installer-settings-test-{name}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct NoopCommandRunner;
    impl profile::CommandRunner for NoopCommandRunner {
        fn running_vscode_pids(&self) -> Vec<u32> {
            vec![]
        }
        fn spawn(&self, _code_cli: &Path, _args: &[&str]) -> std::io::Result<u32> {
            Ok(1)
        }
        fn is_alive(&self, _pid: u32) -> bool {
            false
        }
        fn request_graceful_close(&self, _pid: u32) {}
        fn force_kill(&self, _pid: u32) {}
        fn sleep(&self, _d: Duration) {}
    }

    fn manifest_settings() -> ManifestSettings {
        ManifestSettings {
            color_theme: "Default Dark Modern".to_string(),
            auto_open_panel: true,
            secondary_side_bar_default_visibility: "hidden".to_string(),
        }
    }

    fn register(dir: &Path, loc: &str, name: &str) {
        let storage = dir.join("storage.json");
        std::fs::write(
            &storage,
            serde_json::json!({"userDataProfiles": [{"location": loc, "name": name}]}).to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("profiles").join(loc)).unwrap();
    }

    #[test]
    fn merge_preserves_foreign_keys() {
        let dir = temp_dir("preserve-foreign");
        register(&dir, "blockless", "Blockless");
        let target = dir.join("profiles").join("blockless").join("settings.json");
        std::fs::write(
            &target,
            serde_json::json!({
                "editor.fontSize": 14,
                "files.autoSave": "onFocusChange",
                "some.deeply.nested": {"a": [1, 2, {"b": true}]}
            })
            .to_string(),
        )
        .unwrap();
        let envpy = dir.join("env").join("bin").join("python");

        ensure_settings(
            &NoopCommandRunner,
            &dir.join("code"),
            &dir.join("storage.json"),
            &dir.join("profiles"),
            "Blockless",
            "blockless",
            &envpy,
            &manifest_settings(),
        )
        .unwrap();

        let after: Value = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(after["editor.fontSize"], serde_json::json!(14));
        assert_eq!(after["files.autoSave"], serde_json::json!("onFocusChange"));
        assert_eq!(
            after["some.deeply.nested"],
            serde_json::json!({"a": [1, 2, {"b": true}]})
        );
        assert_eq!(after["mpyhw.autoOpenPanel"], serde_json::json!(true));
        assert_eq!(
            after["mpyhw.pythonPath"],
            serde_json::json!(envpy.to_string_lossy())
        );
    }

    #[test]
    fn refuses_unparseable_existing_settings() {
        let dir = temp_dir("unparseable");
        register(&dir, "blockless", "Blockless");
        let target = dir.join("profiles").join("blockless").join("settings.json");
        std::fs::write(&target, b"{ not valid json, has // comments too").unwrap();
        let original = std::fs::read(&target).unwrap();
        let envpy = dir.join("env").join("bin").join("python");

        let err = ensure_settings(
            &NoopCommandRunner,
            &dir.join("code"),
            &dir.join("storage.json"),
            &dir.join("profiles"),
            "Blockless",
            "blockless",
            &envpy,
            &manifest_settings(),
        )
        .unwrap_err();

        assert!(matches!(err, SettingsError::Unparseable(_)));
        assert_eq!(
            std::fs::read(&target).unwrap(),
            original,
            "an unparseable settings.json must never be touched"
        );
    }

    #[test]
    fn skip_requires_all_four_keys_to_match() {
        let dir = temp_dir("four-keys");
        register(&dir, "blockless", "Blockless");
        let target = dir.join("profiles").join("blockless").join("settings.json");
        let envpy = dir.join("env").join("bin").join("python");

        // first call applies all four keys
        let first = ensure_settings(
            &NoopCommandRunner,
            &dir.join("code"),
            &dir.join("storage.json"),
            &dir.join("profiles"),
            "Blockless",
            "blockless",
            &envpy,
            &manifest_settings(),
        )
        .unwrap();
        assert!(first.applied);
        let after_first = std::fs::read(&target).unwrap();

        // second call with everything already correct: must skip (no write)
        let second = ensure_settings(
            &NoopCommandRunner,
            &dir.join("code"),
            &dir.join("storage.json"),
            &dir.join("profiles"),
            "Blockless",
            "blockless",
            &envpy,
            &manifest_settings(),
        )
        .unwrap();
        assert!(!second.applied);
        assert_eq!(
            std::fs::read(&target).unwrap(),
            after_first,
            "skip must not rewrite the file"
        );

        // now break just ONE of the four keys (autoOpenPanel) directly on
        // disk -- must be detected as no-longer-current and re-applied
        let mut broken: Value = serde_json::from_slice(&after_first).unwrap();
        broken["mpyhw.autoOpenPanel"] = serde_json::json!(false);
        std::fs::write(&target, serde_json::to_vec(&broken).unwrap()).unwrap();

        let third = ensure_settings(
            &NoopCommandRunner,
            &dir.join("code"),
            &dir.join("storage.json"),
            &dir.join("profiles"),
            "Blockless",
            "blockless",
            &envpy,
            &manifest_settings(),
        )
        .unwrap();
        assert!(
            third.applied,
            "one wrong key out of four must still trigger a re-apply"
        );
        let repaired: Value = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(repaired["mpyhw.autoOpenPanel"], serde_json::json!(true));
    }

    #[test]
    fn journal_adopts_the_resolved_id_not_the_seed_constant() {
        let dir = temp_dir("resolved-id");
        // simulate a VS-Code-created fallback profile: a hashed id, not "blockless"
        register(&dir, "a1b2c3d4e5f6", "Blockless");
        let envpy = dir.join("env").join("bin").join("python");

        let outcome = ensure_settings(
            &NoopCommandRunner,
            &dir.join("code"),
            &dir.join("storage.json"),
            &dir.join("profiles"),
            "Blockless",
            "blockless", // the seed constant -- must NOT be what gets journaled
            &envpy,
            &manifest_settings(),
        )
        .unwrap();

        assert_eq!(outcome.profile_location, "a1b2c3d4e5f6");
        assert!(
            dir.join("profiles")
                .join("a1b2c3d4e5f6")
                .join("settings.json")
                .exists(),
            "must write to the RESOLVED location"
        );
        assert!(!dir.join("profiles").join("blockless").exists());
    }

    #[test]
    fn resolves_via_offline_seed_when_profile_not_yet_registered() {
        let dir = temp_dir("via-seed");
        let envpy = dir.join("env").join("bin").join("python");
        // nothing registered yet at all: ensure_settings must itself seed it

        let outcome = ensure_settings(
            &NoopCommandRunner,
            &dir.join("code"),
            &dir.join("storage.json"),
            &dir.join("profiles"),
            "Blockless",
            "blockless",
            &envpy,
            &manifest_settings(),
        )
        .unwrap();

        assert_eq!(outcome.profile_location, "blockless");
        assert!(outcome.applied);
    }

    #[test]
    fn write_is_atomic_no_tmp_left_behind() {
        let dir = temp_dir("atomic");
        register(&dir, "blockless", "Blockless");
        let envpy = dir.join("env").join("bin").join("python");

        ensure_settings(
            &NoopCommandRunner,
            &dir.join("code"),
            &dir.join("storage.json"),
            &dir.join("profiles"),
            "Blockless",
            "blockless",
            &envpy,
            &manifest_settings(),
        )
        .unwrap();

        let target = dir.join("profiles").join("blockless").join("settings.json");
        assert!(target.exists());
        assert!(!dir
            .join("profiles")
            .join("blockless")
            .join("settings.json.tmp")
            .exists());
    }
}
