//! clap subcommand per op (the same operations a future GUI would call
//! through `ops.rs` directly): `install`, `repair`, `repair-runtime`,
//! `update-extension`, `verify`, `diagnostics`, `uninstall`. The argument
//! grammar itself lives in `cli.rs`, ungated, so it's unit-tested on every
//! target; only the dispatch body below needs an OS.
//!
//! The real body only compiles on macOS/Windows: it references
//! `blockless_installer_core::system::SystemEnvironment`, which is itself
//! cfg-gated per target (see `core/src/system.rs`'s module doc for why). On
//! any other target -- this workspace's own `cargo test`/`clippy` sandbox
//! included -- `main` is a trivial stub, so the crate still compiles and
//! lints cleanly everywhere the ubuntu CI job runs it, without pretending
//! this binary is meant to run there.

mod cli;

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn main() {
    real_main::run();
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn main() {
    eprintln!("blockless-installer only supports macOS and Windows.");
    std::process::exit(1);
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod real_main {
    use crate::cli::{Cli, Command};
    use blockless_installer_core::fetch::FetchOptions;
    use blockless_installer_core::manifest::Manifest;
    use blockless_installer_core::platform::{Arch, Os, Paths, RawEnv};
    use blockless_installer_core::state::State;
    use blockless_installer_core::system::SystemEnvironment;
    use blockless_installer_core::uninstall::{UninstallFlags, UninstallOutcome};
    use blockless_installer_core::verify::CheckResult;
    use blockless_installer_core::{ops, verify};
    use clap::Parser;
    use std::path::PathBuf;

    fn default_manifest_path() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|dir| dir.join("installer.manifest.json")))
            .unwrap_or_else(|| PathBuf::from("installer.manifest.json"))
    }

    fn die(msg: impl std::fmt::Display) -> ! {
        eprintln!("blockless-installer: {msg}");
        std::process::exit(1);
    }

    /// A single ever-appended `logs/installer.log` (ARCHITECTURE §9), so a
    /// repeat run's steps land alongside the first, matching §13's "a
    /// second run logs every step as a skip". The returned guard must stay
    /// alive for the process's lifetime -- dropping it early stops the
    /// background writer thread and silently loses buffered log lines.
    fn init_logging(logs_dir: &std::path::Path) -> tracing_appender::non_blocking::WorkerGuard {
        let _ = std::fs::create_dir_all(logs_dir);
        let file_appender = tracing_appender::rolling::never(logs_dir, "installer.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        tracing_subscriber::fmt()
            .with_writer(non_blocking)
            .with_ansi(false)
            .init();
        guard
    }

    fn mac_install_targets(os: Os, raw: &RawEnv) -> Vec<PathBuf> {
        match os {
            Os::MacOs => {
                let home = raw.home.as_deref().unwrap_or_default();
                vec![
                    PathBuf::from("/Applications"),
                    PathBuf::from(home).join("Applications"),
                ]
            }
            Os::Windows => vec![],
        }
    }

    fn print_verify_results(results: &[CheckResult]) -> bool {
        for r in results {
            let tag = if r.pass { "PASS" } else { "FAIL" };
            println!("{tag}: {}", r.message);
        }
        println!("----");
        let ok = verify::all_pass(results);
        if ok {
            println!("ALL PASS");
        } else {
            let n = results.iter().filter(|r| !r.pass).count();
            println!("{n} FAILED");
        }
        ok
    }

    fn report_state(result: Result<State, ops::OpsError>) {
        match result {
            Ok(state) => {
                println!(
                    "done: vscode={} extension={} python={} settings={}",
                    state.steps.vscode,
                    state.steps.extension,
                    state.steps.python,
                    state.steps.settings
                );
            }
            Err(e) => die(e),
        }
    }

    pub fn run() {
        let cli = Cli::parse();

        let manifest_path = cli.manifest.clone().unwrap_or_else(default_manifest_path);
        let manifest_json = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
            die(format!(
                "could not read manifest at {}: {e}",
                manifest_path.display()
            ))
        });
        let manifest = Manifest::parse(&manifest_json).unwrap_or_else(|e| die(e));

        let raw = RawEnv::from_process();
        let os = Os::detect(&raw).unwrap_or_else(|e| die(e));
        let arch = Arch::detect(os, &raw).unwrap_or_else(|e| die(e));
        let paths = Paths::resolve(os, &raw).unwrap_or_else(|e| die(e));
        let _log_guard = init_logging(&paths.logs);
        let code_candidates = blockless_installer_core::platform::code_cli_candidates(os, &raw)
            .unwrap_or_else(|e| die(e));
        let targets = mac_install_targets(os, &raw);

        let ctx = ops::OpsContext {
            os,
            arch,
            paths,
            manifest: &manifest,
            client: reqwest::blocking::Client::new(),
            fetch_opts: FetchOptions::default(),
            code_candidates,
            mac_install_targets: targets,
            vsix_path: cli.vsix.clone(),
        };

        let env = SystemEnvironment;

        match &cli.command {
            Command::Install => report_state(ops::install(&env, &ctx)),
            Command::Repair => report_state(ops::repair(&env, &ctx)),
            Command::RepairRuntime => report_state(ops::repair_runtime(&env, &ctx)),
            Command::UpdateExtension => report_state(ops::update_extension(&env, &ctx)),
            Command::Verify => {
                let results = ops::verify(&env, &ctx);
                if !print_verify_results(&results) {
                    std::process::exit(1);
                }
            }
            Command::Diagnostics { output } => match ops::diagnostics(&ctx, output) {
                Ok(()) => println!("diagnostics bundle written to {}", output.display()),
                Err(e) => die(e),
            },
            Command::Uninstall { all, keep_vscode } => {
                let flags = UninstallFlags {
                    all: *all,
                    keep_vscode: *keep_vscode,
                };
                match ops::uninstall(&env, &ctx, &flags) {
                    UninstallOutcome::VscodeRunning => {
                        println!("VS Code is running; quit it and re-run to uninstall. Nothing was removed.");
                    }
                    UninstallOutcome::AbortedUnreadableState => {
                        die("state.json exists but is unreadable/incomplete; cannot determine what to remove. Nothing was removed.");
                    }
                    UninstallOutcome::Finished {
                        profile_removed,
                        blk_removed,
                        blk_removal_partial,
                        vscode_removed,
                        invariant_guard_tripped,
                    } => {
                        if invariant_guard_tripped {
                            die("could not confirm the profile was fully removed; the ownership journal was left intact so a re-run can finish. Nothing else was removed.");
                        }
                        println!(
                            "done: profile_removed={profile_removed} blk_removed={blk_removed} blk_removal_partial={blk_removal_partial} vscode_removed={vscode_removed}"
                        );
                    }
                }
            }
        }
    }
}
