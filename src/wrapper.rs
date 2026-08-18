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
//! at 1.30x, against 1.42x for counting how many invocations are running: the
//! graph says how wide a build could be and the count says how wide it is, and a
//! unit whose siblings finished early gets the machine either way.
//!
//! Take the 1.42x with salt. It was measured while the count was stuck at one
//! (see below), so what it actually compared was the graph against blanket
//! allocation, not against counting. The argument for counting stands on its own
//! terms; the number does not, and the two designs have never been measured
//! against each other with the count working.
//!
//! Handing every invocation eight threads is worse than doing nothing: the
//! fan-out phase is already full, and blanket `-Zthreads=8` took the same build
//! from 27.51s to 39.08s.
//!
//! That blanket allocation is also what this did for a long time without meaning
//! to. The ticket was released as the share was decided rather than held for the
//! compile, so an invocation was only ever visible to another one inside the same
//! few microseconds of bookkeeping: over a cold rust-analyzer check, 282 of 286
//! invocations saw no sibling at all and took the cap. Holding the ticket across
//! the compile is what makes the count mean what the rest of this describes, and
//! the same check then spreads across the whole range, most invocations seeing
//! the nine or ten that are really there.
//!
//! Correcting it was worth 16.89s to 16.28s of wall clock and, more to the point,
//! 112.66s to 93.06s of CPU: the threads it stops handing out during fan-out were
//! being paid for and returning nothing. The 39.08s above did not reproduce at
//! that scale on a current toolchain -- blanket allocation now costs a little
//! rather than a lot -- but it costs, and it costs most on a machine doing
//! anything else at the same time.
//!
//! # Why there is no threshold
//!
//! A build with no tail has little to gain here and pays the wrapper's cost
//! anyway. On rustc 1.100.0-nightly ripgrep measures 3.16s under plain cargo
//! against 3.06s with this, so it is close to break-even; on an earlier toolchain
//! it was a small loss, at 2.85s against 3.09s.
//!
//! Withholding threads until the build narrows was tried as a fix and helped
//! neither case -- ripgrep measured 3.08s, 3.17s and 3.47s at thresholds of one,
//! two and three siblings, and tokio was best with no threshold at all, at 2.57s
//! against 3.24s at one. Those threshold figures have not been re-measured since.
//!
//! The graph shape that decides this is not visible from inside a single
//! invocation, and the projects that gain little and the projects that gain a lot
//! are not told apart by anything cheap: ripgrep and tokio have much the same
//! package count and workspace depth. So the allocation stays unconditional, and
//! a project that measures worse can turn it off.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Past eight the returns flatten, so more only buys contention.
const MAX_THREADS: usize = 8;

/// Names the temporary directory a build's tickets live in.
const TICKET_PREFIX: &str = "cargo-turbo-";

pub fn run() -> i32 {
    let mut args = env::args_os().skip(1);
    let Some(rustc) = args.next() else {
        eprintln!("cargo-turbo: expected the real rustc as the first argument");
        return 2;
    };
    let args: Vec<OsString> = args.collect();

    let mut command = Command::new(&rustc);
    command.args(&args);

    // Held across the whole compile, so the invocations that start later count
    // this one as running. Releasing it before rustc ran left the count blind to
    // every process actually compiling: measured over a cold rust-analyzer
    // check, 282 of 286 invocations saw no sibling at all and took the cap.
    let ticket = Ticket::claim();
    if let Some(share) = thread_share(ticket.as_ref()) {
        command.arg(format!("-Zthreads={share}"));
    }

    let status = command.status();
    // Dropped only now, so the machine is handed back when the work is over
    // rather than when the decision was made.
    drop(ticket);

    match status {
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
/// The ticket is the caller's, because it has to outlive this decision: it is
/// what makes this invocation visible to the ones that start while it compiles.
fn thread_share(ticket: Option<&Ticket>) -> Option<usize> {
    if env::var("CARGO_TURBO_THREADS").as_deref() == Ok("0") {
        return None;
    }
    let jobs = env::var("CARGO_TURBO_JOBS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(available_cores);

    // Without a ticket the concurrency is unknown, and assuming the build is
    // wide leaves rustc single-threaded, which is what cargo does unaided.
    share_for(jobs, ticket?.siblings())
}

/// The policy itself, kept free of the environment so it can be tested.
///
/// `None` means the flag is not passed at all, which is the right answer for a
/// share of one: `-Zthreads=1` is what rustc does unaided, so sending it only
/// adds an argument and a reason for the fingerprint to differ.
///
/// # Why the machine is divided exactly
///
/// Handing each core out two or three times over was tried and rejected. It
/// buys a little wall clock and pays for it in CPU, which is the wrong trade for
/// something that runs on a machine doing other work:
///
/// | cold, factor 1 to 3 | wall | CPU |
/// |---|---|---|
/// | rust-analyzer | 16.3s to 15.8s | 93s to 108s |
/// | tokio | 2.74s to 2.53s | 10.8s to 13.1s |
/// | ripgrep | 3.06s to 3.12s | 15.6s to 18.2s |
///
/// Three to seven per cent of wall for sixteen to nineteen per cent of CPU, and
/// ripgrep does not even get the wall back. Worth re-testing only against a
/// working sibling count: the first attempt at this was measured while the count
/// was stuck at one, where a factor changes almost nothing and the result meant
/// nothing.
fn share_for(jobs: usize, siblings: usize) -> Option<usize> {
    let share = (jobs / siblings.max(1)).clamp(1, MAX_THREADS);
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
        let dir = env::temp_dir().join(format!("{TICKET_PREFIX}{}", build_scope()));
        fs::create_dir_all(&dir).ok()?;
        let path = dir.join(std::process::id().to_string());
        fs::write(&path, b"").ok()?;
        Some(Self { path, dir })
    }

    fn siblings(&self) -> usize {
        fs::read_dir(&self.dir).map_or(1, |entries| entries.count().max(1))
    }
}

/// Removes the ticket directories that finished builds left behind.
///
/// A directory is deliberately not removed when its last ticket drops, because
/// that would race a sibling claiming a ticket inside it. Nothing else reclaims
/// them, so they accumulate for the life of the machine: one per workspace,
/// profile and target directory. Fifty-four had gathered on the machine this
/// was written on.
///
/// Only empty directories are removed, and `remove_dir` refusing a non-empty
/// one *is* the test for "no invocation is using this". A build that loses the
/// race recreates the directory on its next claim, and a claim that fails
/// merely leaves that one rustc single-threaded, so the worst case is a lost
/// thread share rather than a broken build.
pub fn clean_tickets() -> usize {
    clean_tickets_in(&env::temp_dir())
}

/// The directory is a parameter so a test can be confined to one it created;
/// sweeping the real temporary directory would disturb builds running beside it.
fn clean_tickets_in(dir: &std::path::Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(TICKET_PREFIX))
        })
        .filter(|e| fs::remove_dir(e.path()).is_ok())
        .count()
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
        assert_eq!(share_for(10, 1), Some(MAX_THREADS));
        // Even on a machine wider than the cap, which is what the cap is for.
        assert_eq!(share_for(128, 1), Some(MAX_THREADS));
    }

    #[test]
    fn a_wide_build_leaves_every_invocation_single_threaded() {
        // A share of one is not passed to rustc at all, so this is `None`
        // rather than `Some(1)`.
        assert_eq!(share_for(10, 30), None);
        assert_eq!(share_for(10, 10), None);
    }

    #[test]
    fn the_share_divides_the_machine() {
        assert_eq!(share_for(10, 4), Some(2));
        assert_eq!(share_for(10, 2), Some(5));
    }

    #[test]
    fn a_saturated_build_gets_no_flag_at_all() {
        // A core each or less: the machine is already full, and this is the
        // case that made blanket `-Zthreads=8` a loss.
        assert_eq!(share_for(10, 10), None);
        assert_eq!(share_for(10, 11), None);
    }

    #[test]
    fn a_miscounted_ticket_never_divides_by_zero() {
        // `siblings` counts directory entries and this invocation's own ticket
        // is one of them, so zero should be impossible -- but a racing cleanup
        // could still read an empty directory, and dividing by it would panic
        // inside a build rather than merely misjudge the share.
        assert_eq!(share_for(10, 0), Some(MAX_THREADS));
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
    fn cleaning_reclaims_finished_builds_but_spares_running_ones() {
        let tmp = env::temp_dir().join(format!("cargo-turbo-sweep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let idle = tmp.join(format!("{TICKET_PREFIX}idle"));
        let busy = tmp.join(format!("{TICKET_PREFIX}busy"));
        let other = tmp.join("unrelated");
        for d in [&idle, &busy, &other] {
            fs::create_dir_all(d).unwrap();
        }
        // A build still running holds a ticket inside its directory.
        fs::write(busy.join("1234"), b"").unwrap();

        clean_tickets_in(&tmp);

        assert!(!idle.exists(), "a finished build's directory should be gone");
        assert!(busy.exists(), "a running build must not be disturbed");
        assert!(other.exists(), "unrelated temp directories must be left alone");

        let _ = fs::remove_dir_all(&tmp);
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
