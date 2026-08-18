//! `cargo turbo` makes a cold build reuse work it has already done and use the
//! cores the dependency graph leaves idle.
//!
//! Two measurements motivate everything here, taken on a ten-core Apple M4
//! checking rust-analyzer from an empty target directory:
//!
//! * The build uses 3.9 of ten cores. Dependencies fan out and saturate the
//!   machine for the first third, then concurrency collapses to one while the
//!   workspace crates compile in a chain, each waiting on the metadata of the
//!   one below it.
//! * Of 307 units, 269 are third-party and account for most of the CPU. Those are
//!   immutable: a given version of a crate, built with the same features,
//!   profile and compiler, produces the same bytes every time.
//!
//! The first is addressed by giving each rustc invocation a share of the machine
//! sized to how many others are running. The second twice over: by snapshotting
//! the target directory and restoring it when the inputs that produced it are
//! unchanged, and by sharing those third-party units with every other project on
//! the machine, so a checkout the store has never seen still starts with them
//! built.
//!
//! Nothing here patches cargo or rustc. Freshness decisions stay with cargo,
//! which is what keeps this honest: the snapshot only pre-populates a directory,
//! and cargo then decides for itself what is stale.

mod key;
mod snapshot;
mod units;
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
        Some("prepare") => exit(prepare(&args[1..])),
        Some("store") => exit(store(&args[1..])),
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

    let (hit, mut freshness) = snapshot::restore(&plan);

    // A project the store has never seen has no snapshot, but its dependencies
    // may well have been built by some other project on this machine.
    if hit == snapshot::Hit::None {
        // Timestamps, because that is what the shared unit store needs and it is
        // the larger win: on a rust-analyzer checkout the store has never seen,
        // timestamps take 22.68s to 8.9s, where content hashing manages 16.1s
        // because every supplied unit is rejected.
        //
        // Content hashing is better in exactly one situation, and it can be asked
        // for: a snapshot restored into a checkout whose files all have new
        // timestamps, such as a cache unpacked over a fresh clone, where tokio
        // measured 0.11s against 0.97s. The choice is recorded with the snapshot,
        // so later builds of the same project keep whichever was used.
        freshness = match env::var("CARGO_TURBO_FRESHNESS").as_deref() {
            Ok("checksum") => snapshot::Freshness::Checksum,
            _ => snapshot::Freshness::Mtime,
        };
        units::seed(&plan, freshness);
    }

    let status = snapshot::forward(&plan, args, freshness);

    // Only a successful build is worth keeping, and only if it differs from what
    // was already there.
    if status == 0 {
        if hit != snapshot::Hit::Exact {
            snapshot::save(&plan, freshness);
        }
        // An exact hit compiles nothing, so there is nothing new to add unless the
        // store has been emptied since.
        units::record(&plan, freshness, hit != snapshot::Hit::Exact);
    }
    status
}

/// Fills the target directory with prebuilt dependencies, then stops.
///
/// For builds this cannot run itself. A release pipeline usually invokes cargo
/// through a script or a build tool, and rewriting that to go through `cargo turbo`
/// is often not worth it, so the two halves are available as their own steps:
/// `cargo turbo prepare <the cargo arguments>` before the build and `cargo turbo
/// store <the same arguments>` after it. The arguments are needed because they say
/// which profile and which target the build is for, and so where the units belong.
fn prepare(args: &[String]) -> i32 {
    let plan = match key::Plan::resolve(args) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("cargo-turbo: {e}");
            // Preparation is an optimisation, so failing to do it is not an error:
            // the build that follows is still correct, only slower.
            return 0;
        }
    };
    // Timestamps, matching what a plain `cargo` run does afterwards. Content
    // hashing would reject every unit supplied here.
    let seeded = units::seed(&plan, snapshot::Freshness::Mtime);
    if seeded == 0 {
        eprintln!("cargo-turbo: nothing to supply");
    }
    0
}

/// Adds what the target directory now holds to the shared store.
fn store(args: &[String]) -> i32 {
    let plan = match key::Plan::resolve(args) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("cargo-turbo: {e}");
            return 0;
        }
    };
    units::record(&plan, snapshot::Freshness::Mtime, true);
    eprintln!("cargo-turbo: stored the third-party units of this build");
    0
}

fn print_help() {
    println!(
        "\
cargo turbo — faster cold Rust builds, without patching cargo or rustc

USAGE:
    cargo turbo <cargo-command> [args…]     run a cargo command, accelerated
    cargo turbo status                      what is stored, and how much space
    cargo turbo clean                       remove every snapshot and unit

FOR A BUILD THIS CANNOT RUN ITSELF:
    cargo turbo prepare <cargo-args…>       supply prebuilt dependencies, then stop
    cargo turbo store <cargo-args…>         add this build's dependencies to the store

    Use these when cargo is invoked by a script or another build tool. Pass the
    same arguments the build will use, since they say which profile and target it
    is for. Between the two, run the build however you already do.

      cargo turbo prepare build --release --target x86_64-unknown-linux-musl
      ./my-build-script
      cargo turbo store build --release --target x86_64-unknown-linux-musl

EXAMPLES:
    cargo turbo check --workspace
    cargo turbo build --release

WHAT IT DOES:
    Restores a previously recorded target directory when the inputs that
    produced it are unchanged, supplies dependencies other projects on this
    machine have already built, and gives each rustc invocation a share of the
    machine based on how many others are running at that moment.

    Cargo still decides what is stale, so an edited file is always rebuilt.

ENVIRONMENT:
    CARGO_TURBO_DIR       where snapshots live (default: cache dir)
    CARGO_TURBO_JOBS      cores to divide between invocations (default: all)
    CARGO_TURBO_THREADS   set to 0 to leave rustc single-threaded
    CARGO_TURBO_NEAR      set to 0 to require an exact key, never a near match
    CARGO_TURBO_KEEP      snapshots kept per workspace and profile (default: 5)
    CARGO_TURBO_FRESHNESS set to checksum to judge freshness by content instead
                          of timestamps: better for a cache unpacked over a
                          fresh clone, worse for sharing between projects
    CARGO_TURBO_OFF       set to 1 to forward to cargo unchanged
    CARGO_TURBO_TIME      set to report how long each phase of this tool took"
    );
}
