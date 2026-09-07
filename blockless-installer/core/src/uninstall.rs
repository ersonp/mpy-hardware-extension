//! The fail-closed uninstaller. Ports `uninstall-blockless.{zsh,ps1}`'s
//! invariant structure exactly: read the journal BEFORE any deletion, never
//! guess ownership, and stop rather than guess when a check can't be
//! confirmed.
//!
//! Three separate fail-closed postures, all deliberate:
//! - Reading `state.json`: any failure (unreadable, unparseable, or missing
//!   a required field like `profileCreatedByUs`) aborts with NOTHING
//!   removed -- we cannot determine what we own, so we touch nothing.
//! - Editing `storage.json` (removing our profile entry): best-effort and
//!   silent on failure (never replaces an unparseable file) -- there IS a
//!   fallback, the invariant guard right after it.
//! - The invariant guard: after attempting the profile removal, re-check
//!   whether the storage.json entry or the profile directory still
//!   "provably remains". An UNREADABLE storage.json at this point counts as
//!   "still present" (cannot confirm gone = not gone), matching the
//!   Windows script's `catch { $entryPresent = $true }` exactly. If
//!   anything remains, stop before touching `BLK` or the journal, so a
//!   re-run can still finish.

use crate::profile;
use crate::state::State;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// OS-native deletion operations, injected so this is unit-testable without
/// a real filesystem-tree removal or a real VS Code uninstaller.
pub trait UninstallRunner {
    /// Recursively delete a directory tree. `Ok(())` only when the caller
    /// should trust it worked; the invariant guard and the BLK-removal
    /// report both re-check the filesystem afterward rather than trusting
    /// this alone, matching the scripts' "don't claim success over a
    /// partial delete" posture.
    fn remove_dir_all(&self, path: &Path) -> Result<(), String>;
    /// Run the platform's own VS Code uninstaller if one exists at
    /// `vscode_dir` (Windows: `unins000.exe /VERYSILENT`). `Ok(true)` if it
    /// ran, `Ok(false)` if there's no such uninstaller (caller falls back to
    /// `remove_dir_all`), `Err` if it exists but failed to run.
    fn run_vscode_uninstaller(&self, vscode_dir: &Path) -> Result<bool, String>;
}

/// `^[A-Za-z0-9_-]+$`, hand-rolled (no regex dependency for one allowlist
/// check): the profile directory is deleted ONLY when the journaled
/// location matches this shape, so a tampered/corrupt journal (`.`, `..`,
/// anything with a path separator) can never escape `profiles/` on delete.
fn is_safe_location(loc: &str) -> bool {
    !loc.is_empty()
        && loc
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn read_storage(storage_path: &Path) -> Option<Value> {
    let bytes = std::fs::read(storage_path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_atomic(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "storage.json".to_string());
    let tmp = parent.join(format!("{file_name}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, dest)
}

/// Best-effort: filters our entry out if the file exists and parses.
/// Silent on any failure (missing, unreadable, unparseable, wrong shape) --
/// the invariant guard right after this is what actually enforces safety,
/// not this function's return value (it has none).
fn remove_storage_entry(storage_path: &Path, profile_name: &str) {
    let Some(mut root) = read_storage(storage_path) else {
        return;
    };
    let Some(obj) = root.as_object_mut() else {
        return;
    };
    if let Some(list) = obj
        .get_mut("userDataProfiles")
        .and_then(Value::as_array_mut)
    {
        list.retain(|e| e.get("name").and_then(Value::as_str) != Some(profile_name));
    } else {
        return;
    }
    let Ok(body) = serde_json::to_vec_pretty(&root) else {
        return;
    };
    let _ = write_atomic(storage_path, &body);
}

/// Fail-closed: `true` only when we can POSITIVELY confirm the entry is
/// gone. Missing file = confirmed gone. Unreadable/unparseable/wrong-shape
/// = cannot confirm = treated as still present.
fn storage_entry_confirmed_absent(storage_path: &Path, profile_name: &str) -> bool {
    if !storage_path.exists() {
        return true;
    }
    let Some(root) = read_storage(storage_path) else {
        return false; // exists but unreadable/unparseable: cannot confirm
    };
    let present = root
        .get("userDataProfiles")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .any(|e| e.get("name").and_then(Value::as_str) == Some(profile_name))
        })
        .unwrap_or(false);
    !present
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UninstallFlags {
    pub all: bool,
    pub keep_vscode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// VS Code is running: nothing was touched. Quit it and re-run.
    VscodeRunning,
    /// `state.json` exists but is unreadable/unparseable/incomplete: nothing
    /// was touched. Fix it (or delete `BLK` manually) and re-run.
    AbortedUnreadableState,
    Finished {
        profile_removed: bool,
        blk_removed: bool,
        /// `BLK` existed and an attempt was made, but something remains
        /// (locked/protected file) -- reported honestly, not as success.
        blk_removal_partial: bool,
        vscode_removed: bool,
        /// The profile removal could not be confirmed complete (storage.json
        /// entry or profile dir still provably present, or unreadable).
        /// `BLK` and the journal were left untouched so a re-run can finish.
        invariant_guard_tripped: bool,
    },
}

#[allow(clippy::too_many_arguments)]
pub fn uninstall(
    command_runner: &dyn profile::CommandRunner,
    runner: &dyn UninstallRunner,
    state_path: &Path,
    storage_path: &Path,
    profiles_dir: &Path,
    profile_name: &str,
    blk: &Path,
    // Every location this install could plausibly have put VS Code (mac: up
    // to two -- /Applications and ~/Applications, since `vscode.rs`'s own
    // writability fallback can land at either; Windows: always exactly one).
    // Each one that exists on disk gets removed, not just whichever the
    // caller happened to derive from a currently-runnable `code` CLI -- a
    // broken/stale candidate must not leave a real install undetected.
    vscode_dirs: &[PathBuf],
    flags: &UninstallFlags,
) -> UninstallOutcome {
    // Read state BEFORE any deletion. Any failure to positively confirm
    // ownership means we abort with nothing removed.
    let state = match State::read(state_path) {
        Ok(s) => s,
        Err(_) => return UninstallOutcome::AbortedUnreadableState,
    };

    // A running VS Code owns storage.json in memory and would clobber our
    // edit; do nothing and ask the user to quit it and re-run.
    if !command_runner.running_vscode_pids().is_empty() {
        return UninstallOutcome::VscodeRunning;
    }

    let (profile_created_by_us, vscode_installed_by_us, profile_location) = match &state {
        Some(s) => (
            s.profile_created_by_us,
            s.vscode_installed_by_us,
            s.profile_location.clone(),
        ),
        None => (false, false, String::new()),
    };

    let mut profile_removed = false;
    if profile_created_by_us {
        remove_storage_entry(storage_path, profile_name);

        let loc_safe = is_safe_location(&profile_location);
        if loc_safe {
            let dir = profiles_dir.join(&profile_location);
            if dir.exists() {
                let _ = runner.remove_dir_all(&dir);
            }
        }
        // An unsafe (or empty) location is deliberately never touched --
        // refuse to act on a tampered/corrupt journal rather than guess.

        let entry_gone = storage_entry_confirmed_absent(storage_path, profile_name);
        // Mirrors the scripts exactly: for an unsafe location, presence is
        // never even checked (there is nothing we attempted to remove at an
        // untrusted path), so it never blocks the guard on its own.
        let dir_still_present = loc_safe && profiles_dir.join(&profile_location).exists();

        if !entry_gone || dir_still_present {
            return UninstallOutcome::Finished {
                profile_removed: false,
                blk_removed: false,
                blk_removal_partial: false,
                vscode_removed: false,
                invariant_guard_tripped: true,
            };
        }
        profile_removed = true;
    }

    let (blk_removed, blk_removal_partial) = if blk.exists() {
        let attempted = runner.remove_dir_all(blk);
        if attempted.is_ok() && !blk.exists() {
            (true, false)
        } else {
            (false, true)
        }
    } else {
        (true, false)
    };

    let should_remove_vscode = !flags.keep_vscode && (flags.all || vscode_installed_by_us);
    // `true` only once every EXISTING candidate was actually removed -- a
    // partial removal (e.g. a locked file at one location) must not report
    // success, matching this function's existing honesty posture for BLK.
    let vscode_removed = if should_remove_vscode {
        let mut all_removed = true;
        let mut any_existed = false;
        for vscode_dir in vscode_dirs {
            if !vscode_dir.exists() {
                continue;
            }
            any_existed = true;
            let removed = match runner.run_vscode_uninstaller(vscode_dir) {
                Ok(true) => true,
                Ok(false) => runner.remove_dir_all(vscode_dir).is_ok() && !vscode_dir.exists(),
                Err(_) => false,
            };
            all_removed &= removed;
        }
        any_existed && all_removed
    } else {
        false
    };

    UninstallOutcome::Finished {
        profile_removed,
        blk_removed,
        blk_removal_partial,
        vscode_removed,
        invariant_guard_tripped: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Steps;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "blockless-installer-uninstall-test-{name}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `BLK`, VS Code's own data dir, and the VS Code install dir are three
    /// SIBLING trees in reality (never nested in each other) -- state.json
    /// lives inside BLK, but storage.json/profiles and the VS Code install
    /// never do. A fixture that nests them would make BLK removal silently
    /// also delete storage.json or the VS Code dir, masking real bugs (or
    /// creating fake ones) that don't exist in production.
    struct Layout {
        blk: PathBuf,
        state_path: PathBuf,
        storage_path: PathBuf,
        profiles_dir: PathBuf,
        vscode_dir: PathBuf,
    }

    fn layout(name: &str) -> Layout {
        let root = temp_dir(name);
        let blk = root.join("Blockless");
        let code_user = root.join("CodeUser");
        Layout {
            state_path: blk.join("state.json"),
            storage_path: code_user.join("storage.json"),
            profiles_dir: code_user.join("profiles"),
            vscode_dir: root.join("vscode"),
            blk,
        }
    }

    struct FakeCommandRunner {
        running: Vec<u32>,
    }
    impl profile::CommandRunner for FakeCommandRunner {
        fn running_vscode_pids(&self) -> Vec<u32> {
            self.running.clone()
        }
        fn spawn(&self, _code_cli: &Path, _args: &[&str]) -> std::io::Result<u32> {
            unimplemented!()
        }
        fn is_alive(&self, _pid: u32) -> bool {
            unimplemented!()
        }
        fn request_graceful_close(&self, _pid: u32) {}
        fn force_kill(&self, _pid: u32) {}
        fn sleep(&self, _d: Duration) {}
    }
    fn not_running() -> FakeCommandRunner {
        FakeCommandRunner { running: vec![] }
    }

    #[derive(Default)]
    struct FakeUninstallRunner {
        remove_dir_calls: RefCell<Vec<std::path::PathBuf>>,
        fail_remove_for: RefCell<Vec<std::path::PathBuf>>,
        uninstaller_result: RefCell<Option<Result<bool, String>>>,
    }
    impl UninstallRunner for FakeUninstallRunner {
        fn remove_dir_all(&self, path: &Path) -> Result<(), String> {
            self.remove_dir_calls.borrow_mut().push(path.to_path_buf());
            if self.fail_remove_for.borrow().contains(&path.to_path_buf()) {
                return Err("locked".to_string());
            }
            let _ = std::fs::remove_dir_all(path);
            Ok(())
        }
        fn run_vscode_uninstaller(&self, _vscode_dir: &Path) -> Result<bool, String> {
            self.uninstaller_result
                .borrow()
                .clone()
                .unwrap_or(Ok(false))
        }
    }

    fn write_state(path: &Path, state: &State) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        state.write(path).unwrap();
    }

    fn write_storage_with_entry(path: &Path, profile_name: &str, location: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            serde_json::json!({
                "userDataProfiles": [{"location": location, "name": profile_name}],
                "someOtherProfile": {"kept": true},
            })
            .to_string(),
        )
        .unwrap();
    }

    fn default_state() -> State {
        State {
            profile_created_by_us: true,
            vscode_installed_by_us: false,
            profile_location: "blockless".to_string(),
            steps: Steps {
                vscode: true,
                extension: true,
                python: true,
                settings: true,
            },
            ..Default::default()
        }
    }

    // --- fail-closed matrix ---

    #[test]
    fn missing_state_json_proceeds_without_aborting() {
        let l = layout("missing-state");
        std::fs::create_dir_all(&l.blk).unwrap(); // BLK exists, empty
        let runner = FakeUninstallRunner::default();
        let outcome = uninstall(
            &not_running(),
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            std::slice::from_ref(&l.vscode_dir),
            &UninstallFlags::default(),
        );
        match outcome {
            UninstallOutcome::Finished {
                profile_removed, ..
            } => assert!(!profile_removed),
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_state_json_aborts_nothing_removed() {
        use std::os::unix::fs::PermissionsExt;
        let l = layout("unreadable-state");
        std::fs::create_dir_all(l.state_path.parent().unwrap()).unwrap();
        std::fs::write(&l.state_path, b"{}").unwrap();
        std::fs::set_permissions(&l.state_path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let runner = FakeUninstallRunner::default();

        let outcome = uninstall(
            &not_running(),
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            std::slice::from_ref(&l.vscode_dir),
            &UninstallFlags::default(),
        );

        std::fs::set_permissions(&l.state_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(outcome, UninstallOutcome::AbortedUnreadableState);
        assert!(
            runner.remove_dir_calls.borrow().is_empty(),
            "nothing may be removed on abort"
        );
        assert!(l.blk.exists(), "BLK must survive an aborted uninstall");
    }

    #[test]
    fn state_json_missing_profile_created_by_us_key_aborts() {
        let l = layout("missing-key");
        std::fs::create_dir_all(l.state_path.parent().unwrap()).unwrap();
        // valid JSON, but not the State shape at all -- missing every
        // required field including profileCreatedByUs
        std::fs::write(&l.state_path, serde_json::json!({"foo": "bar"}).to_string()).unwrap();
        let runner = FakeUninstallRunner::default();

        let outcome = uninstall(
            &not_running(),
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            std::slice::from_ref(&l.vscode_dir),
            &UninstallFlags::default(),
        );

        assert_eq!(outcome, UninstallOutcome::AbortedUnreadableState);
        assert!(runner.remove_dir_calls.borrow().is_empty());
    }

    #[test]
    fn suspicious_location_is_never_passed_to_remove_dir_all() {
        let l = layout("suspicious-location");
        // A traversal target that genuinely resolves to something outside
        // profiles_dir on this filesystem (profiles_dir/../../etc -> the
        // layout root's "etc"), not just an unsafe-looking string that
        // happens not to exist -- otherwise a broken allowlist check would
        // pass this test for free (the delete would be a no-op regardless,
        // since nothing exists at the traversal target).
        let root = l.blk.parent().unwrap();
        let outside_sentinel = root.join("etc");
        std::fs::create_dir_all(&outside_sentinel).unwrap();
        // profiles_dir itself must exist on disk too: the OS resolves ".."
        // by walking into "profiles" first, so a non-existent profiles_dir
        // makes `.exists()` false at that component alone -- masking the
        // traversal regardless of what the safety check does.
        std::fs::create_dir_all(&l.profiles_dir).unwrap();

        let mut state = default_state();
        state.profile_location = "../../etc".to_string();
        write_state(&l.state_path, &state);
        write_storage_with_entry(&l.storage_path, "Blockless", "../../etc");
        let runner = FakeUninstallRunner::default();

        let outcome = uninstall(
            &not_running(),
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            std::slice::from_ref(&l.vscode_dir),
            &UninstallFlags::default(),
        );

        assert!(
            outside_sentinel.exists(),
            "a suspicious location must never be deleted, traversal target or not"
        );
        for call in runner.remove_dir_calls.borrow().iter() {
            assert!(
                !call.to_string_lossy().contains(".."),
                "must never attempt to delete a path derived from an unsafe location: {call:?}"
            );
        }
        // the storage entry itself WAS removable (valid JSON), so the
        // invariant guard doesn't trip on that alone -- matches the scripts.
        match outcome {
            UninstallOutcome::Finished {
                invariant_guard_tripped,
                ..
            } => {
                assert!(!invariant_guard_tripped)
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn invariant_guard_trips_and_leaves_journal_and_blk_intact() {
        let l = layout("guard-trips");
        write_state(&l.state_path, &default_state());
        // unparseable storage.json: remove_storage_entry can't touch it, and
        // the post-check can't confirm the entry is gone either
        std::fs::create_dir_all(l.storage_path.parent().unwrap()).unwrap();
        std::fs::write(&l.storage_path, b"{ not valid json").unwrap();
        let sentinel = l.blk.join("sentinel.txt");
        std::fs::write(&sentinel, b"still here").unwrap();
        let runner = FakeUninstallRunner::default();

        let outcome = uninstall(
            &not_running(),
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            std::slice::from_ref(&l.vscode_dir),
            &UninstallFlags::default(),
        );

        match outcome {
            UninstallOutcome::Finished {
                invariant_guard_tripped,
                blk_removed,
                profile_removed,
                ..
            } => {
                assert!(invariant_guard_tripped);
                assert!(!blk_removed);
                assert!(!profile_removed);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert!(
            l.state_path.exists(),
            "journal must survive a tripped guard"
        );
        assert!(
            sentinel.exists(),
            "BLK must never be touched once the guard trips"
        );
    }

    // --- VS Code running ---

    #[test]
    fn vscode_running_touches_nothing() {
        let l = layout("vscode-running");
        write_state(&l.state_path, &default_state());
        write_storage_with_entry(&l.storage_path, "Blockless", "blockless");
        let runner = FakeUninstallRunner::default();
        let running = FakeCommandRunner {
            running: vec![4242],
        };

        let outcome = uninstall(
            &running,
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            std::slice::from_ref(&l.vscode_dir),
            &UninstallFlags::default(),
        );

        assert_eq!(outcome, UninstallOutcome::VscodeRunning);
        assert!(runner.remove_dir_calls.borrow().is_empty());
        assert!(l.state_path.exists());
        assert!(l.blk.exists());
    }

    // --- ownership gating ---

    #[test]
    fn profile_not_created_by_us_is_never_touched() {
        let l = layout("not-ours");
        let mut state = default_state();
        state.profile_created_by_us = false;
        write_state(&l.state_path, &state);
        write_storage_with_entry(&l.storage_path, "Blockless", "blockless");
        let before = std::fs::read(&l.storage_path).unwrap();
        let runner = FakeUninstallRunner::default();

        let outcome = uninstall(
            &not_running(),
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            std::slice::from_ref(&l.vscode_dir),
            &UninstallFlags::default(),
        );

        match outcome {
            UninstallOutcome::Finished {
                profile_removed, ..
            } => assert!(!profile_removed),
            other => panic!("expected Finished, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&l.storage_path).unwrap(),
            before,
            "a profile we didn't create must never be edited"
        );
    }

    #[test]
    fn vscode_installed_by_us_true_removes_it() {
        let l = layout("vscode-installed-true");
        let mut state = default_state();
        state.profile_created_by_us = false;
        state.vscode_installed_by_us = true;
        write_state(&l.state_path, &state);
        std::fs::create_dir_all(&l.vscode_dir).unwrap();
        let runner = FakeUninstallRunner::default();

        let outcome = uninstall(
            &not_running(),
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            std::slice::from_ref(&l.vscode_dir),
            &UninstallFlags::default(),
        );

        match outcome {
            UninstallOutcome::Finished { vscode_removed, .. } => assert!(vscode_removed),
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn removes_every_existing_vscode_location_not_just_the_first() {
        // Mirrors the reviewer's finding: a mac install can plausibly have
        // landed at either /Applications or ~/Applications (`vscode.rs`'s
        // own writability fallback), and a stale/broken `code` CLI candidate
        // must not leave a real install at the OTHER location undetected.
        let l = layout("multi-location");
        let mut state = default_state();
        state.profile_created_by_us = false;
        state.vscode_installed_by_us = true;
        write_state(&l.state_path, &state);
        let root = l.blk.parent().unwrap();
        let apps = root.join("Applications");
        let home_apps = root.join("HomeApplications");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::create_dir_all(&home_apps).unwrap();
        let runner = FakeUninstallRunner::default();

        let outcome = uninstall(
            &not_running(),
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            &[apps.clone(), home_apps.clone()],
            &UninstallFlags::default(),
        );

        match outcome {
            UninstallOutcome::Finished { vscode_removed, .. } => assert!(vscode_removed),
            other => panic!("expected Finished, got {other:?}"),
        }
        assert!(!apps.exists(), "the first location must be removed");
        assert!(
            !home_apps.exists(),
            "the second location must ALSO be removed"
        );
    }

    #[test]
    fn a_locked_second_location_reports_vscode_removed_false_not_true() {
        // Honesty check: removing the first location but failing on the
        // second must not report overall success.
        let l = layout("multi-location-partial");
        let mut state = default_state();
        state.profile_created_by_us = false;
        state.vscode_installed_by_us = true;
        write_state(&l.state_path, &state);
        let root = l.blk.parent().unwrap();
        let apps = root.join("Applications");
        let home_apps = root.join("HomeApplications");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::create_dir_all(&home_apps).unwrap();
        let runner = FakeUninstallRunner::default();
        runner.fail_remove_for.borrow_mut().push(home_apps.clone());

        let outcome = uninstall(
            &not_running(),
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            &[apps.clone(), home_apps.clone()],
            &UninstallFlags::default(),
        );

        match outcome {
            UninstallOutcome::Finished { vscode_removed, .. } => assert!(!vscode_removed),
            other => panic!("expected Finished, got {other:?}"),
        }
        assert!(!apps.exists(), "the removable location is still removed");
        assert!(home_apps.exists(), "the locked location must survive");
    }

    #[test]
    fn vscode_installed_by_us_false_leaves_it() {
        let l = layout("vscode-installed-false");
        let mut state = default_state();
        state.profile_created_by_us = false;
        state.vscode_installed_by_us = false;
        write_state(&l.state_path, &state);
        std::fs::create_dir_all(&l.vscode_dir).unwrap();
        let runner = FakeUninstallRunner::default();

        let outcome = uninstall(
            &not_running(),
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            std::slice::from_ref(&l.vscode_dir),
            &UninstallFlags::default(),
        );

        match outcome {
            UninstallOutcome::Finished { vscode_removed, .. } => assert!(!vscode_removed),
            other => panic!("expected Finished, got {other:?}"),
        }
        assert!(l.vscode_dir.exists());
    }

    // --- flag combinations ---

    #[test]
    fn keep_vscode_wins_over_installed_by_us_and_all() {
        let l = layout("keep-vscode-wins");
        let mut state = default_state();
        state.profile_created_by_us = false;
        state.vscode_installed_by_us = true;
        write_state(&l.state_path, &state);
        std::fs::create_dir_all(&l.vscode_dir).unwrap();
        let runner = FakeUninstallRunner::default();

        let outcome = uninstall(
            &not_running(),
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            std::slice::from_ref(&l.vscode_dir),
            &UninstallFlags {
                all: true,
                keep_vscode: true,
            },
        );

        match outcome {
            UninstallOutcome::Finished { vscode_removed, .. } => assert!(!vscode_removed),
            other => panic!("expected Finished, got {other:?}"),
        }
        assert!(l.vscode_dir.exists());
    }

    #[test]
    fn all_flag_forces_removal_even_when_not_installed_by_us() {
        let l = layout("all-flag-forces");
        let mut state = default_state();
        state.profile_created_by_us = false;
        state.vscode_installed_by_us = false;
        write_state(&l.state_path, &state);
        std::fs::create_dir_all(&l.vscode_dir).unwrap();
        let runner = FakeUninstallRunner::default();

        let outcome = uninstall(
            &not_running(),
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            std::slice::from_ref(&l.vscode_dir),
            &UninstallFlags {
                all: true,
                keep_vscode: false,
            },
        );

        match outcome {
            UninstallOutcome::Finished { vscode_removed, .. } => assert!(vscode_removed),
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    // --- BLK removal honesty ---

    #[test]
    fn blk_partial_removal_is_reported_honestly() {
        let l = layout("blk-partial");
        let mut state = default_state();
        state.profile_created_by_us = false;
        write_state(&l.state_path, &state);
        let runner = FakeUninstallRunner::default();
        runner.fail_remove_for.borrow_mut().push(l.blk.clone());

        let outcome = uninstall(
            &not_running(),
            &runner,
            &l.state_path,
            &l.storage_path,
            &l.profiles_dir,
            "Blockless",
            &l.blk,
            std::slice::from_ref(&l.vscode_dir),
            &UninstallFlags::default(),
        );

        match outcome {
            UninstallOutcome::Finished {
                blk_removed,
                blk_removal_partial,
                ..
            } => {
                assert!(!blk_removed);
                assert!(blk_removal_partial);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert!(
            l.blk.exists(),
            "a failed remove_dir_all must leave the directory as evidence, not lie about it"
        );
    }
}
