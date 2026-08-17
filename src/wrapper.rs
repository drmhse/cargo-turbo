//! Giving each rustc invocation a share of the machine.
//!
//! A cold build is latency-bound rather than throughput-bound. Measured on a
//! ten-core Apple M4 checking rust-analyzer: 27.51s of wall clock against 59.15s
//! of CPU, so 2.34 cores. Dependencies fan out and saturate the machine for
//! 17.5s, averaging 15.9 units in flight, and then concurrency falls to 1.46 for
//! the remaining 25s while workspace crates compile in a chain.
//!
//! rustc's frontend can use several threads, and during that tail every core but
//! one is idle. Cold, one crate goes from 3.53s at one thread to 1.56s at eight.
//!
//! # Why the share is counted rather than computed
//!
//! The obvious approach derives it from the dependency graph: find each unit's
//! depth, count how many units share that depth, and divide. That was measured
//! at 1.30x. Counting how many invocations are *actually* running reaches 1.42x,
//! because the graph says how wide a build could be and the count says how wide
//! it is. A unit whose siblings finished early gets the machine either way.
//!
//! Handing every invocation eight threads is worse than doing nothing: the
//! fan-out phase is already full, and blanket `-Zthreads=8` took the same build
//! from 27.51s to 39.08s.
//!
//! # Why there is no threshold
//!
//! A build with no tail has nothing to gain here and pays the wrapper's cost
//! anyway: ripgrep takes 2.85s under plain cargo, 3.09s with this, and 2.82s with
//! `CARGO_TURBO_THREADS=0`. Withholding threads until the build narrows was tried
//! as a fix and helped neither case -- ripgrep measured 3.08s, 3.17s and 3.47s at
//! thresholds of one, two and three siblings, and tokio was best with no threshold
//! at all, at 2.57s against 3.24s at one. The graph shape that decides this is not
//! visible from inside a single invocation, and the projects that lose and the
//! projects that gain are not told apart by anything cheap: ripgrep loses and
//! tokio gains with much the same package count and workspace depth. So the
//! allocation stays unconditional, and a project that measures worse can turn it
//! off.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Past eight the returns flatten, so more only buys contention.
const MAX_THREADS: usize = 8;

pub fn run() -> i32 {
    let mut args = env::args_os().skip(1);
    let Some(rustc) = args.next() else {
        eprintln!("cargo-turbo: expected the real rustc as the first argument");
        return 2;
    };
    let args: Vec<OsString> = args.collect();

    let mut command = Command::new(&rustc);
    command.args(&args);

    if let Some(share) = thread_share() {
        command.arg(format!("-Zthreads={share}"));
    }

    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!(
                "cargo-turbo: could not run {}: {e}",
                rustc.to_string_lossy()
            );
            1
        }
    }
}

/// How many threads this invocation should get, or `None` to leave it alone.
///
/// The ticket is released when it drops, which happens as this returns, so the
/// count seen by a later invocation reflects only the work still running.
fn thread_share() -> Option<usize> {
    if env::var("CARGO_TURBO_THREADS").as_deref() == Ok("0") {
        return None;
    }
    let jobs = env::var("CARGO_TURBO_JOBS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(available_cores);

    // Without a ticket the concurrency is unknown, and assuming the build is
    // wide leaves rustc single-threaded, which is what cargo does unaided.
    let ticket = Ticket::claim()?;
    let share = (jobs / ticket.siblings().max(1)).clamp(1, MAX_THREADS);
    drop(ticket);
    (share > 1).then_some(share)
}

/// A file that exists while this invocation runs.
///
/// Counting files rather than holding a shared counter keeps this free of locks
/// and of any state that could survive a killed build. A miscount changes a
/// thread share and never a result, so the imprecision costs nothing.
struct Ticket {
    path: PathBuf,
    dir: PathBuf,
}

impl Ticket {
    fn claim() -> Option<Self> {
        // Scoped to the build, so two builds running side by side each see their
        // own width rather than each other's.
        let dir = env::temp_dir().join(format!("cargo-turbo-{}", build_scope()));
        fs::create_dir_all(&dir).ok()?;
        let path = dir.join(std::process::id().to_string());
        fs::write(&path, b"").ok()?;
        Some(Self { path, dir })
    }

    fn siblings(&self) -> usize {
        fs::read_dir(&self.dir).map_or(1, |entries| entries.count().max(1))
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        // Leaving the directory behind is deliberate: removing it would race
        // with a sibling claiming a ticket inside it.
    }
}

/// A name shared by every invocation of one cargo run, and only those.
///
/// The output directory is the right thing to key on: cargo passes it on every
/// command line and it is identical for every unit of one build.
///
/// `CARGO_PKG_NAME` looks tempting and is wrong. Cargo sets it per crate, so
/// scoping by it gave every invocation its own directory, and every invocation
/// then saw one sibling and claimed the maximum share. That is the blanket
/// allocation this exists to avoid: measured on rust-analyzer, handing eight
/// threads to everything took a cold check from 26.28s to 39.08s. The mistake
/// left 275 directories behind for a 309 unit build, which is how it was found.
fn build_scope() -> String {
    let mut args = env::args();
    while let Some(arg) = args.next() {
        let dir = if arg == "--out-dir" {
            args.next()
        } else {
            arg.strip_prefix("--out-dir=").map(str::to_owned)
        };
        if let Some(dir) = dir {
            return format!("{:016x}", crate::key::hash(profile_root(&dir).as_bytes()));
        }
    }
    "shared".into()
}

/// The output directory reduced to the profile it belongs to.
///
/// Ordinary units share `target/debug/deps`, but a build script is compiled into
/// `target/debug/build/<pkg>-<hash>`, so keying on the directory verbatim gave
/// every build script its own scope: 61 directories for two builds. Cutting the
/// path at the profile leaves one name per profile per target directory, which is
/// one name per build.
fn profile_root(out_dir: &str) -> &str {
    for marker in ["/debug/", "/release/"] {
        if let Some(at) = out_dir.find(marker) {
            return &out_dir[..at + marker.len()];
        }
    }
    out_dir
}

fn available_cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_invocation_gets_the_machine_up_to_the_cap() {
        assert_eq!((10_usize / 1).clamp(1, MAX_THREADS), MAX_THREADS);
    }

    #[test]
    fn a_wide_build_leaves_every_invocation_single_threaded() {
        assert_eq!((10_usize / 30).clamp(1, MAX_THREADS), 1);
        // And a share of one is not passed to rustc at all.
        assert!(!(1 > 1));
    }

    #[test]
    fn the_share_divides_the_machine() {
        assert_eq!((10_usize / 4).clamp(1, MAX_THREADS), 2);
        assert_eq!((10_usize / 2).clamp(1, MAX_THREADS), 5);
    }

    #[test]
    fn every_unit_of_one_build_shares_a_scope() {
        // The scope must depend on the output directory and nothing else, or
        // each invocation counts only itself and claims the whole machine.
        let a = ["rustc", "--crate-name", "foo", "--out-dir", "/t/debug/deps"];
        let b = ["rustc", "--crate-name", "bar", "--out-dir", "/t/debug/deps"];
        assert_eq!(scope_of(&a), scope_of(&b));

        let other = [
            "rustc",
            "--crate-name",
            "foo",
            "--out-dir",
            "/other/debug/deps",
        ];
        assert_ne!(scope_of(&a), scope_of(&other));
    }

    #[test]
    fn a_build_script_shares_the_scope_of_the_units_around_it() {
        // Cargo compiles build scripts into their own directory, and keying on
        // that verbatim gave each one the whole machine.
        assert_eq!(
            profile_root("/t/target/debug/build/serde-abc123"),
            profile_root("/t/target/debug/deps")
        );
        // Profiles stay apart, since a debug and a release build are separate work.
        assert_ne!(
            profile_root("/t/target/debug/deps"),
            profile_root("/t/target/release/deps")
        );
    }

    /// `build_scope` reads the real process arguments, so the logic is exercised
    /// through the same parse over a supplied list.
    fn scope_of(args: &[&str]) -> String {
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let dir = if *arg == "--out-dir" {
                it.next().map(|s| (*s).to_owned())
            } else {
                arg.strip_prefix("--out-dir=").map(str::to_owned)
            };
            if let Some(dir) = dir {
                return format!("{:016x}", crate::key::hash(profile_root(&dir).as_bytes()));
            }
        }
        "shared".into()
    }

    #[test]
    fn a_ticket_is_visible_while_held_and_gone_after() {
        let dir = env::temp_dir().join(format!("cargo-turbo-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let ticket = Ticket {
            path: dir.join("1"),
            dir: dir.clone(),
        };
        fs::write(&ticket.path, b"").unwrap();
        assert_eq!(ticket.siblings(), 1);
        drop(ticket);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
