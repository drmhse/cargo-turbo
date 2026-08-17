//! `cargo turbo` makes a cold build reuse work it has already done and use the
//! cores the dependency graph leaves idle.
//!
//! Two measurements motivate everything here, taken on a ten-core Apple M4
//! checking rust-analyzer from an empty target directory:
//!
//! * The build uses 2.34 of ten cores. Dependencies fan out and saturate the
//!   machine for the first third, then concurrency collapses to one while the
//!   workspace crates compile in a chain, each waiting on the metadata of the
//!   one below it.
//! * Of 288 units, 250 are third-party and account for 77% of the CPU. Those are
//!   immutable: a given version of a crate, built with the same features,
//!   profile and compiler, produces the same bytes every time.
//!
//! The first is addressed by giving each rustc invocation a share of the machine
//! sized to how many others are running. The second by snapshotting the target
//! directory and restoring it when the inputs that produced it are unchanged.
//!
//! Nothing here patches cargo or rustc. Freshness decisions stay with cargo,
//! which is what keeps this honest: the snapshot only pre-populates a directory,
//! and cargo then decides for itself what is stale.

mod key;
mod snapshot;
mod wrapper;

use std::env;
use std::process::exit;

/// Set on the environment when this binary installs itself as `RUSTC_WRAPPER`,
/// so wrapper mode is never inferred from the shape of the arguments.
const WRAPPER_MARKER: &str = "CARGO_TURBO_WRAPPER";

fn main() {
    if env::var_os(WRAPPER_MARKER).is_some() {
        exit(wrapper::run());
    }

    // Cargo invokes an external subcommand as `cargo-turbo turbo <args…>`.
    let args: Vec<String> = env::args().skip(1).skip_while(|a| a == "turbo").collect();

    match args.first().map(String::as_str) {
        Some("--help") | Some("-h") | None => {
            print_help();
            exit(0);
        }
        Some("--version") | Some("-V") => {
            println!("cargo-turbo {}", env!("CARGO_PKG_VERSION"));
            exit(0);
        }
        Some("clean") => exit(snapshot::clean()),
        Some("status") => exit(snapshot::status()),
        _ => exit(run_build(&args)),
    }
}

/// Restores what is already known, runs cargo, and records the result.
fn run_build(args: &[String]) -> i32 {
    let plan = match key::Plan::resolve(args) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("cargo-turbo: {e}");
            // A build that cannot be accelerated still has to happen.
            return snapshot::forward_plain(args);
        }
    };

    let restored = snapshot::restore(&plan);
    let status = snapshot::forward(&plan, args);

    // Only a successful build is worth keeping, and only if it differs from what
    // was already there.
    if status == 0 && !restored {
        snapshot::save(&plan);
    }
    status
}

fn print_help() {
    println!(
        "\
cargo turbo — faster cold Rust builds, without patching cargo or rustc

USAGE:
    cargo turbo <cargo-command> [args…]     run a cargo command, accelerated
    cargo turbo status                      what is stored, and how much space
    cargo turbo clean                       remove every snapshot

EXAMPLES:
    cargo turbo check --workspace
    cargo turbo build --release

WHAT IT DOES:
    Restores a previously recorded target directory when the inputs that
    produced it are unchanged, and gives each rustc invocation a share of the
    machine based on how many others are running at that moment.

    Cargo still decides what is stale, so an edited file is always rebuilt.

ENVIRONMENT:
    CARGO_TURBO_DIR       where snapshots live (default: cache dir)
    CARGO_TURBO_JOBS      cores to divide between invocations (default: all)
    CARGO_TURBO_THREADS   set to 0 to leave rustc single-threaded
    CARGO_TURBO_OFF       set to 1 to forward to cargo unchanged"
    );
}
