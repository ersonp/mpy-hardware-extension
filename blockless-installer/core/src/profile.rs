//! The "Blockless" VS Code profile: offline registration, resolving where it
//! really lives on disk, and the window-launch fallback with its teardown.
//! Ports `register_profile_offline`/`Register-ProfileOffline`,
//! `profile_registered`/`Test-ProfileRegistered`,
//! `resolve_profile_location`/`Get-ProfileLocation`, and
//! `register_profile`/`Register-Profile` + `stop_code_and_wait`/`Stop-OurCode`.
//!
//! The window-launch fallback diverges from M0 on purpose, per `NOTES.md`:
//! M0 (both platforms) finds "the" VS Code process by name/pattern after the
//! fact (`pgrep -f`, `Get-Process -Name Code`) because a shell script has no
//! handle to what it launched. This core spawns VS Code itself and holds the
//! real child PID from the start, so teardown targets that exact PID -- never
//! a pattern match that could also catch a window the user opened. All
//! process operations go through [`CommandRunner`] so this is testable
//! without a real VS Code binary or real wall-clock waits.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("could not create profile directory {path}: {source}")]
    CreateDir {
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
}

/// Process operations `profile.rs` needs, injected so every code path here is
/// unit-testable without a real VS Code binary. A production implementation
/// is OS-specific (mac: SIGTERM + `pgrep`; Windows: `taskkill` + `Get-Process`)
/// and lands with `vscode.rs`/`ops.rs`, which are the first callers that run
/// for real.
pub trait CommandRunner {
    /// PIDs of any currently-running VS Code app processes on this machine.
    /// Re-checked immediately before every spawn (never cached from an
    /// earlier point) -- the user may have opened VS Code during a long
    /// prior download.
    fn running_vscode_pids(&self) -> Vec<u32>;
    /// Spawn `code_cli` with `args`, returning its PID.
    fn spawn(&self, code_cli: &Path, args: &[&str]) -> std::io::Result<u32>;
    /// Is this exact PID still alive?
    fn is_alive(&self, pid: u32) -> bool;
    /// Ask this exact PID to close gracefully (mac: SIGTERM; Windows:
    /// `taskkill` without `/F`) so VS Code gets a chance to save window and
    /// profile state before exiting.
    fn request_graceful_close(&self, pid: u32);
    /// Force-kill this exact PID (last resort, only ever a PID we spawned).
    fn force_kill(&self, pid: u32);
    /// Injected so poll loops don't burn real wall-clock time in tests.
    fn sleep(&self, d: Duration);
}

/// Read `storage.json` leniently: `None` for both "does not exist" and
/// "exists but failed to read or parse". Matches M0's uniform swallow (JXA's
/// `try { JSON.parse } catch { return }`, or Windows' `try {...} catch {return
/// ""}`): both platforms treat an existing-but-broken file as "can't use it",
/// never as fatal -- there is always a fallback path (the window launch).
fn read_storage_lenient(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The profile's on-disk location (the `profiles/<location>` directory id),
/// looked up by name in `storage.json`'s `userDataProfiles` array.
pub fn resolve_profile_location(storage_path: &Path, profile_name: &str) -> Option<String> {
    let root = read_storage_lenient(storage_path)?;
    let list = root.get("userDataProfiles")?.as_array()?;
    for entry in list {
        let name = entry.get("name").and_then(Value::as_str);
        if name == Some(profile_name) {
            return entry
                .get("location")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
    None
}

pub fn profile_registered(storage_path: &Path, profile_name: &str) -> bool {
    resolve_profile_location(storage_path, profile_name).is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterOfflineOutcome {
    AlreadyRegistered,
    /// A VS Code instance is running: it owns `storage.json` in memory and
    /// would clobber our edit on its own next save. Do nothing; the window
    /// fallback ([`register_profile`]) covers this case.
    SkippedVscodeRunning,
    /// `storage.json` exists but could not be read/parsed. Never replace an
    /// unparseable file (it may hold other profiles' window state); the
    /// window fallback covers this case too.
    SkippedUnparseableStorage,
    Registered,
}

/// Register the profile WITHOUT launching VS Code: seed `storage.json`'s
/// `userDataProfiles` entry directly and create `profiles/<seed_location>/`.
/// This is the fix for "panel never auto-opens" (see `NOTES.md`): a live
/// window created then closed mid-startup leaves half-written window state,
/// so the real first launch resolves against the default profile. Seeding
/// means no window exists before the final launch.
pub fn register_profile_offline(
    runner: &dyn CommandRunner,
    storage_path: &Path,
    profiles_dir: &Path,
    profile_name: &str,
    seed_location: &str,
) -> Result<RegisterOfflineOutcome, ProfileError> {
    if profile_registered(storage_path, profile_name) {
        return Ok(RegisterOfflineOutcome::AlreadyRegistered);
    }
    if !runner.running_vscode_pids().is_empty() {
        return Ok(RegisterOfflineOutcome::SkippedVscodeRunning);
    }

    let mut root = match std::fs::read(storage_path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(v) => v,
            Err(_) => return Ok(RegisterOfflineOutcome::SkippedUnparseableStorage),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Default::default()),
        Err(_) => return Ok(RegisterOfflineOutcome::SkippedUnparseableStorage),
    };

    let profile_dir = profiles_dir.join(seed_location);
    std::fs::create_dir_all(&profile_dir).map_err(|source| ProfileError::CreateDir {
        path: profile_dir,
        source,
    })?;

    // A storage.json that parses but isn't a JSON object (VS Code never
    // writes one that isn't, but a hand-edited or foreign file could be) has
    // nowhere to hold userDataProfiles -- treat it the same as unparseable
    // rather than panic or silently drop the registration.
    let Some(obj) = root.as_object_mut() else {
        return Ok(RegisterOfflineOutcome::SkippedUnparseableStorage);
    };
    if !obj.get("userDataProfiles").is_some_and(Value::is_array) {
        obj.insert("userDataProfiles".to_string(), Value::Array(Vec::new()));
    }
    let list = obj
        .get_mut("userDataProfiles")
        .unwrap()
        .as_array_mut()
        .unwrap();
    let already_present = list
        .iter()
        .any(|e| e.get("name").and_then(Value::as_str) == Some(profile_name));
    if !already_present {
        let mut entry = serde_json::Map::new();
        entry.insert(
            "location".to_string(),
            Value::String(seed_location.to_string()),
        );
        entry.insert("name".to_string(), Value::String(profile_name.to_string()));
        list.push(Value::Object(entry));
    }

    let body = serde_json::to_vec(&root).expect("Value always serializes");
    write_atomic(storage_path, &body).map_err(|source| ProfileError::Write {
        path: storage_path.to_path_buf(),
        source,
    })?;
    Ok(RegisterOfflineOutcome::Registered)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterProfileOutcome {
    AlreadyRegistered,
    Registered,
    TimedOut,
    SpawnFailed,
}

/// Fallback only: a live VS Code owns `storage.json`, or the offline seed
/// otherwise didn't take. Launch `code_cli --profile <name> --new-window`,
/// poll for registration, then close the process WE spawned -- but only if
/// nothing was already running before we launched.
///
/// Attach-to-existing-instance caveat (documented, not fixable by PID
/// bookkeeping): if the user opens VS Code during the poll below while ours
/// is still starting, `--new-window` attaches to our not-yet-registered
/// instance rather than spawning a second one, so closing "our" PID would
/// close theirs too. The only guard against ever tearing down a session we
/// didn't start alone is the re-probe immediately before spawning: if
/// anything was already running at that instant, we never call
/// [`stop_and_wait`] at all.
pub fn register_profile(
    runner: &dyn CommandRunner,
    code_cli: &Path,
    storage_path: &Path,
    profile_name: &str,
) -> RegisterProfileOutcome {
    if profile_registered(storage_path, profile_name) {
        return RegisterProfileOutcome::AlreadyRegistered;
    }
    let before = runner.running_vscode_pids();
    let spawned_pid = match runner.spawn(code_cli, &["--profile", profile_name, "--new-window"]) {
        Ok(pid) => pid,
        Err(_) => return RegisterProfileOutcome::SpawnFailed,
    };

    let mut registered = false;
    for _ in 0..60 {
        if profile_registered(storage_path, profile_name) {
            registered = true;
            break;
        }
        runner.sleep(Duration::from_millis(500));
    }

    if before.is_empty() {
        stop_and_wait(runner, spawned_pid);
    }

    if registered {
        RegisterProfileOutcome::Registered
    } else {
        RegisterProfileOutcome::TimedOut
    }
}

/// Graceful close, then force-kill if that doesn't take within the wait
/// window, then wait again to confirm exit -- so the caller's next launch is
/// a fresh instance (a running extension host won't load a newly installed
/// extension). Ports `stop_code_and_wait`/`Stop-OurCode`; always targets the
/// exact PID passed in, never a pattern match.
pub fn stop_and_wait(runner: &dyn CommandRunner, pid: u32) {
    runner.request_graceful_close(pid);
    for _ in 0..60 {
        if !runner.is_alive(pid) {
            return;
        }
        runner.sleep(Duration::from_millis(250));
    }
    runner.force_kill(pid);
    for _ in 0..20 {
        if !runner.is_alive(pid) {
            return;
        }
        runner.sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "blockless-installer-profile-test-{name}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct FakeRunner {
        running_before: Vec<u32>,
        spawn_result: std::io::Result<u32>,
        graceful_close_calls: RefCell<Vec<u32>>,
        force_kill_calls: RefCell<Vec<u32>>,
        /// Index into `alive_sequence` advanced by each `is_alive` call.
        alive_sequence: RefCell<std::vec::IntoIter<bool>>,
        /// Runs on every injected `sleep()`, letting a test seed
        /// `storage.json` partway through a poll loop (simulating "VS Code,
        /// which we spawned, registered the profile").
        on_sleep: RefCell<Box<dyn FnMut()>>,
    }

    impl FakeRunner {
        fn new(running_before: Vec<u32>, spawn_result: std::io::Result<u32>) -> FakeRunner {
            FakeRunner {
                running_before,
                spawn_result,
                graceful_close_calls: RefCell::new(Vec::new()),
                force_kill_calls: RefCell::new(Vec::new()),
                alive_sequence: RefCell::new(Vec::new().into_iter()),
                on_sleep: RefCell::new(Box::new(|| {})),
            }
        }

        fn with_alive_sequence(self, seq: Vec<bool>) -> Self {
            *self.alive_sequence.borrow_mut() = seq.into_iter();
            self
        }
    }

    impl CommandRunner for FakeRunner {
        fn running_vscode_pids(&self) -> Vec<u32> {
            self.running_before.clone()
        }
        fn spawn(&self, _code_cli: &Path, _args: &[&str]) -> std::io::Result<u32> {
            match &self.spawn_result {
                Ok(pid) => Ok(*pid),
                Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
            }
        }
        fn is_alive(&self, _pid: u32) -> bool {
            self.alive_sequence.borrow_mut().next().unwrap_or(false)
        }
        fn request_graceful_close(&self, pid: u32) {
            self.graceful_close_calls.borrow_mut().push(pid);
        }
        fn force_kill(&self, pid: u32) {
            self.force_kill_calls.borrow_mut().push(pid);
        }
        fn sleep(&self, _d: Duration) {
            (self.on_sleep.borrow_mut())();
        }
    }

    fn write_json(path: &Path, value: &Value) {
        std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    // --- register_profile_offline: seed into every storage.json shape ---

    #[test]
    fn seeds_into_missing_storage_json() {
        let dir = temp_dir("missing");
        let storage = dir.join("storage.json");
        let profiles_dir = dir.join("profiles");
        let runner = FakeRunner::new(vec![], Ok(1));

        let outcome =
            register_profile_offline(&runner, &storage, &profiles_dir, "Blockless", "blockless")
                .unwrap();

        assert_eq!(outcome, RegisterOfflineOutcome::Registered);
        let written: Value = serde_json::from_slice(&std::fs::read(&storage).unwrap()).unwrap();
        assert_eq!(
            written["userDataProfiles"][0],
            serde_json::json!({"location": "blockless", "name": "Blockless"})
        );
        assert!(profiles_dir.join("blockless").is_dir());
    }

    #[test]
    fn seeds_into_empty_object_storage_json() {
        let dir = temp_dir("empty");
        let storage = dir.join("storage.json");
        let profiles_dir = dir.join("profiles");
        write_json(&storage, &serde_json::json!({}));
        let runner = FakeRunner::new(vec![], Ok(1));

        let outcome =
            register_profile_offline(&runner, &storage, &profiles_dir, "Blockless", "blockless")
                .unwrap();

        assert_eq!(outcome, RegisterOfflineOutcome::Registered);
        let written: Value = serde_json::from_slice(&std::fs::read(&storage).unwrap()).unwrap();
        assert_eq!(written["userDataProfiles"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn seeds_into_populated_storage_json_preserving_unknown_fields() {
        let dir = temp_dir("populated");
        let storage = dir.join("storage.json");
        let profiles_dir = dir.join("profiles");
        let before = serde_json::json!({
            "userDataProfiles": [
                {"location": "abc123", "name": "SomeOtherProfile"}
            ],
            "windowsState": {"lastActiveWindow": {"backupPath": "/foo/bar", "deep": {"nested": [1,2,3]}}},
            "backupWorkspaces": {"workspaces": [], "folders": ["/a", "/b"]},
            "telemetry.machineId": "deadbeef"
        });
        write_json(&storage, &before);
        let runner = FakeRunner::new(vec![], Ok(1));

        let outcome =
            register_profile_offline(&runner, &storage, &profiles_dir, "Blockless", "blockless")
                .unwrap();

        assert_eq!(outcome, RegisterOfflineOutcome::Registered);
        let after: Value = serde_json::from_slice(&std::fs::read(&storage).unwrap()).unwrap();
        // untouched keys preserved exactly (deep equality, not just presence)
        assert_eq!(after["windowsState"], before["windowsState"]);
        assert_eq!(after["backupWorkspaces"], before["backupWorkspaces"]);
        assert_eq!(after["telemetry.machineId"], before["telemetry.machineId"]);
        assert_eq!(after["userDataProfiles"][0], before["userDataProfiles"][0]);
        // ours appended, not replacing the existing entry
        assert_eq!(after["userDataProfiles"].as_array().unwrap().len(), 2);
        assert_eq!(
            after["userDataProfiles"][1],
            serde_json::json!({"location": "blockless", "name": "Blockless"})
        );
    }

    #[test]
    fn skips_unparseable_storage_json_never_replaces_it() {
        let dir = temp_dir("unparseable");
        let storage = dir.join("storage.json");
        let profiles_dir = dir.join("profiles");
        std::fs::write(&storage, b"{ not valid json at all").unwrap();
        let original_bytes = std::fs::read(&storage).unwrap();
        let runner = FakeRunner::new(vec![], Ok(1));

        let outcome =
            register_profile_offline(&runner, &storage, &profiles_dir, "Blockless", "blockless")
                .unwrap();

        assert_eq!(outcome, RegisterOfflineOutcome::SkippedUnparseableStorage);
        assert_eq!(
            std::fs::read(&storage).unwrap(),
            original_bytes,
            "an unparseable file must never be touched"
        );
    }

    #[test]
    fn skips_when_already_registered() {
        let dir = temp_dir("already");
        let storage = dir.join("storage.json");
        let profiles_dir = dir.join("profiles");
        write_json(
            &storage,
            &serde_json::json!({"userDataProfiles": [{"location": "blockless", "name": "Blockless"}]}),
        );
        let before = std::fs::read(&storage).unwrap();
        let runner = FakeRunner::new(vec![], Ok(1));

        let outcome =
            register_profile_offline(&runner, &storage, &profiles_dir, "Blockless", "blockless")
                .unwrap();

        assert_eq!(outcome, RegisterOfflineOutcome::AlreadyRegistered);
        assert_eq!(std::fs::read(&storage).unwrap(), before);
    }

    #[test]
    fn skips_when_vscode_is_running() {
        let dir = temp_dir("running");
        let storage = dir.join("storage.json");
        let profiles_dir = dir.join("profiles");
        let runner = FakeRunner::new(vec![4242], Ok(1));

        let outcome =
            register_profile_offline(&runner, &storage, &profiles_dir, "Blockless", "blockless")
                .unwrap();

        assert_eq!(outcome, RegisterOfflineOutcome::SkippedVscodeRunning);
        assert!(
            !storage.exists(),
            "must not create storage.json while VS Code is running"
        );
    }

    #[test]
    fn write_is_atomic_no_tmp_left_behind() {
        let dir = temp_dir("atomic");
        let storage = dir.join("storage.json");
        let profiles_dir = dir.join("profiles");
        let runner = FakeRunner::new(vec![], Ok(1));

        register_profile_offline(&runner, &storage, &profiles_dir, "Blockless", "blockless")
            .unwrap();

        assert!(storage.exists());
        assert!(!dir.join("storage.json.tmp").exists());
    }

    // --- register_profile: window-launch fallback + exact-PID teardown ---

    #[test]
    fn fallback_polls_until_registered_then_gracefully_closes_the_spawned_pid() {
        let dir = temp_dir("fallback-poll");
        let storage = dir.join("storage.json");
        let code_cli = dir.join("code");

        let runner = FakeRunner::new(vec![], Ok(4242)).with_alive_sequence(vec![false]);
        let storage_for_sleep = storage.clone();
        let calls = std::rc::Rc::new(RefCell::new(0u32));
        let calls_for_closure = calls.clone();
        *runner.on_sleep.borrow_mut() = Box::new(move || {
            let mut c = calls_for_closure.borrow_mut();
            *c += 1;
            if *c == 3 {
                write_json(
                    &storage_for_sleep,
                    &serde_json::json!({"userDataProfiles": [{"location": "blockless", "name": "Blockless"}]}),
                );
            }
        });

        let outcome = register_profile(&runner, &code_cli, &storage, "Blockless");

        assert_eq!(outcome, RegisterProfileOutcome::Registered);
        assert_eq!(*runner.graceful_close_calls.borrow(), vec![4242]);
        assert!(
            runner.force_kill_calls.borrow().is_empty(),
            "graceful close succeeded (is_alive -> false); force_kill must not run"
        );
    }

    #[test]
    fn attach_to_existing_instance_never_torn_down() {
        let dir = temp_dir("attach-existing");
        let storage = dir.join("storage.json");
        let code_cli = dir.join("code");

        // Something was ALREADY running before we spawned -- the re-probe
        // must see this and register_profile must never attempt teardown,
        // since --new-window may have attached to that existing session.
        let runner = FakeRunner::new(vec![999], Ok(4242));
        let storage_for_sleep = storage.clone();
        *runner.on_sleep.borrow_mut() = Box::new(move || {
            if !storage_for_sleep.exists() {
                write_json(
                    &storage_for_sleep,
                    &serde_json::json!({"userDataProfiles": [{"location": "blockless", "name": "Blockless"}]}),
                );
            }
        });

        let outcome = register_profile(&runner, &code_cli, &storage, "Blockless");

        assert_eq!(outcome, RegisterProfileOutcome::Registered);
        assert!(
            runner.graceful_close_calls.borrow().is_empty(),
            "must never close a process when something was already running before spawn"
        );
        assert!(runner.force_kill_calls.borrow().is_empty());
    }

    #[test]
    fn register_profile_already_registered_never_spawns() {
        let dir = temp_dir("already-registered");
        let storage = dir.join("storage.json");
        let code_cli = dir.join("code");
        write_json(
            &storage,
            &serde_json::json!({"userDataProfiles": [{"location": "blockless", "name": "Blockless"}]}),
        );
        // spawn_result is Err, so if register_profile spawned anyway this
        // test would see SpawnFailed instead of AlreadyRegistered.
        let runner = FakeRunner::new(vec![], Err(std::io::Error::other("must not be called")));

        let outcome = register_profile(&runner, &code_cli, &storage, "Blockless");
        assert_eq!(outcome, RegisterProfileOutcome::AlreadyRegistered);
    }

    // --- stop_and_wait: graceful-then-force teardown ---

    #[test]
    fn stop_and_wait_returns_once_graceful_close_takes() {
        let runner = FakeRunner::new(vec![], Ok(1)).with_alive_sequence(vec![true, true, false]);
        stop_and_wait(&runner, 777);
        assert_eq!(*runner.graceful_close_calls.borrow(), vec![777]);
        assert!(runner.force_kill_calls.borrow().is_empty());
    }

    #[test]
    fn stop_and_wait_force_kills_when_graceful_close_never_takes() {
        // `is_alive` stays true for the whole graceful-wait window (60
        // polls), so stop_and_wait must fall back to force_kill.
        let alive_forever = vec![true; 60];
        let runner = FakeRunner::new(vec![], Ok(1)).with_alive_sequence(alive_forever);
        stop_and_wait(&runner, 555);
        assert_eq!(*runner.graceful_close_calls.borrow(), vec![555]);
        assert_eq!(*runner.force_kill_calls.borrow(), vec![555]);
    }
}
