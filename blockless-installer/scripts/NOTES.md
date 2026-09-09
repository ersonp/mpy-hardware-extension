# M0 spec notes (spec input for installer-core)

Records empirical findings from running the M0 scripts on fresh machines. These are
the facts the later Rust `installer-core` must encode.

## Profile settings mechanism: A vs B

VS Code profile UI-state (Activity Bar pinning, hidden views) has no documented
headless write path, and `.code-profile` import is interactive. So the installer
writes the branded settings by hand and we determine empirically which file VS Code
actually reads for a named profile:

- **Mechanism A** (shipped default): write into the per-profile settings file at
  `<UserDir>/profiles/<id>/settings.json`, where `<id>` is looked up by profile name
  from the `userDataProfiles` array in `<UserDir>/globalStorage/storage.json`. The
  per-profile settings path is documented; the name->id lookup via storage.json is
  internal and could break across VS Code versions.
- **Mechanism B** (insurance): write into the default `<UserDir>/settings.json`.

The visual canary (`workbench.colorTheme` = "Default Dark Modern") makes "did VS
Code read this file?" answerable in two seconds on camera. Dark, not light: Blockless
has no light mode, so the shipped theme must not flip users to light.

### Finding

- Winner: **A**. Settled in the M1 scope (`/scope.md`'s "Settings" decision):
  mechanism A only, shipped as the sole path in `settings.rs`; mechanism B
  (the user's default `settings.json`) was removed from the scripts and was
  never carried into `installer-core`.
- Seeded `userDataProfiles` entry (answerable from the scripts/core directly,
  not rig-dependent — this is exactly what `register_profile_offline`/
  `profile.rs::register_profile_offline` writes, deterministically):

```json
{ "location": "blockless", "name": "Blockless" }
```

- Notes / surprises: the seeded `location` ("blockless") is only the id for
  a fresh, offline-seeded profile. If the offline seed is skipped (a VS Code
  instance was already running) and the window-fallback registration path
  creates the profile instead, VS Code assigns a hashed directory id of its
  own — `profile.rs::resolve_profile_location` reads that back and
  `state.json`'s `profileLocation` is journaled from THAT value, never the
  seed constant, so `verify.rs`/`settings.rs` always resolve the real
  on-disk id regardless of which path registered it.

## Profile registration (no headless create)

A named profile does not exist until VS Code is launched with it: `code --profile <new>
--install-extension <id>` FAILS into a not-yet-registered profile, and the profile only
appears in `storage.json`'s `userDataProfiles` after a `--profile <name> --new-window`
launch. `code --help` says `--profile` creates the profile "if it does not exist", but only
on the window-opening path, not on `--install-extension`.

Mitigation shipped in M0 (`register_profile`): if VS Code is not already running, launch it
hidden + in the background (`open -gj -a "Visual Studio Code" --args --profile <name>
--new-window`), poll `storage.json` for the profile entry (not a blind sleep), then quit VS
Code once it registers. If the user already had VS Code open, open a normal window and leave
their session alone (never quit it), since we cannot safely hide/kill a session we did not
start. `open -gj` is best-effort — a window may still flash briefly on some setups.

installer-core (Rust) should own this cleanly: spawn VS Code as a child process it controls,
keep it off-screen, and terminate exactly that process once the profile registers — no
app-wide quit, no reliance on `open -gj`.

## Per-OS quirks observed

From the first macOS rig runs, 2026-09-09, on macOS 15.6.1 arm64 in UTM.

**A running VS Code is not detectable by its main process.** `pgrep -f` could not
read the main process's argv on one machine while it could on another, so any
pattern aimed at `.../Contents/MacOS/<binary>` is unreliable. The binary is also
named `Code`, not `Electron`, which the M0 scripts assumed in seven places. Match
the app bundle instead: the helper processes are always visible and exist only
while VS Code does.

**The VS Code download is 542 MB.** Any total request timeout is a throughput
floor in disguise. `reqwest`'s blocking client defaults to 30 seconds covering
connect, read and write, which silently required ~152 Mbps sustained; below that
every attempt died mid-body. On this rig the step took 54 seconds.

**Extraction produces a flood of `xattr: Operation not permitted`.** Quarantine
stripping cannot touch individual signed files inside the bundle. Harmless, and
the install completes, but it makes install output hard to read and it hid the
useful lines in the rig logs.

**A `--keep-vscode` uninstall is a one-way door.** It removes `BLK`, which holds
`state.json`, so any later uninstall has no ownership record and cannot remove
VS Code. It ends up orphaned, removable only with `--all`, which deletes
regardless of ownership. Test `--keep-vscode` last, or on its own machine.

**An M0 tree upgrades cleanly.** `verify` returned 7/7 against a real M0 install
from 2026-08-05, and the ownership flags carried forward correctly: sticky
`vscodeInstalledByUs: true` and `profileCreatedByUs: false`, so uninstall removed
VS Code and kept the profile.

**A macOS dialog appears on any Mac without Xcode Command Line Tools, and it
is a known deviation from the acceptance criteria.** `uv python install` calls
`install_name_tool` to patch the managed Python's dylib paths. That binary ships
with CLT, so on a fresh Mac macOS pops:

    The install_name_tool command requires the command line developer tools.
    Would you like to install the tools now?

It does NOT block: dismiss it and the install completes. uv logs
`warning: Failed to patch the install name of the dynamic library`, which is
also harmless for us, because the patch only matters when building native
extensions and the runtime installs `mpremote`, `pyserial` and `platformdirs`,
all pure Python. `verify` check 3 (pinned mpremote in env) passed on every rig
run with the patch having failed.

Two things follow. The acceptance says "cold install with no admin/security
prompt", and this is a prompt, so a fresh-Mac demo will show it. And it is
inherited: M0 uses the same uv-managed Python and takes the identical path.

uv exposes no documented way to skip the patch (checked its environment-variable
and installer references), so avoiding it means changing how the runtime is
provisioned, not passing a flag. Left as a deviation deliberately, rather than
pre-installing CLT on the rig, because that would make the demo pass by no
longer being a fresh Mac while every real user still hits it.

**UTM shared folders serve the guest stale copies.** A file changed on the host
can still read as its old content in the VM, and two processes appending to one
file on the share lose writes. Copy the folder to local disk in the guest and
work there. Executing a binary from the share on the HOST, while the guest has
it mounted, stalls for minutes.

## Acceptance checklist: replace the shape fixture with a real capture

DONE, 2026-09-09, on the macOS rig.
`core/tests/fixtures/vscode-update-api.darwin-universal.json` is now a real
captured response and the hand-authored `.SHAPE.` stand-in is gone.

It was worth doing rather than waving through: the real payload carries a
`notes` field the stand-in did not have. A fixture nobody observed asserts a
shape nobody has seen, so its parse test only proved the parser agreed with
whoever wrote the fixture.

Re-capture only when `live_update_api.rs`'s ignored test fails, which means the
API's shape actually changed. The version-specific values going stale with each
VS Code release is expected and is not a reason to re-record. Steps are in
`core/tests/fixtures/README.md`.

## Pinned versions confirmed working

Observed on the macOS rig, 2026-09-09, macOS 15.6.1 arm64.

- VS Code: 1.136.2 (resolve-at-install, so this is what the API served that day)
- uv: 0.11.29
- Python: 3.12.13 (uv-managed, `cpython-3.12.13-macos-aarch64-none`)
- mpremote: 1.28.0

Arch coverage: **darwin-aarch64 only, one of four.** The uv sha pins for
darwin-x86_64, win32-x64 and win32-arm64 remain unproven by any install.
