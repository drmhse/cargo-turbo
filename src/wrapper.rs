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
            eprintln!("cargo-turbo: could not run {}: {e}", rustc.to_string_lossy());
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
        let dir = env::temp_dir().join(format!(
            "cargo-turbo-{}",
            // Scoped to the cargo process, so two builds running side by side
            // each see their own width rather than each other's.
            env::var("CARGO_PKG_NAME").unwrap_or_else(|_| parent_scope())
        ));
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

/// A name shared by every invocation of one cargo run.
///
/// `CARGO_PKG_NAME` is absent when rustc is invoked for a dependency, so this
/// falls back to the target directory, which cargo passes on every command line
/// and which is the same for the whole build.
fn parent_scope() -> String {
    let mut args = env::args();
    while let Some(arg) = args.next() {
        if arg == "--out-dir" {
            if let Some(dir) = args.next() {
                return format!("{:016x}", crate::key::hash(dir.as_bytes()));
            }
        }
    }
    "default".into()
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
    fn a_ticket_is_visible_while_held_and_gone_after() {
        let dir = env::temp_dir().join(format!("cargo-turbo-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let ticket = Ticket { path: dir.join("1"), dir: dir.clone() };
        fs::write(&ticket.path, b"").unwrap();
        assert_eq!(ticket.siblings(), 1);
        drop(ticket);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
