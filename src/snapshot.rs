//! Recording and replaying a target directory.
//!
//! # Why a whole directory rather than individual artifacts
//!
//! Cargo decides a unit's freshness before it would ever consult an external
//! cache, so a tool outside cargo cannot make one unit fresh. It can, however,
//! hand cargo a directory that already contains the answer, and let cargo's own
//! freshness pass conclude there is nothing to do. Measured on rust-analyzer,
//! that reaches 1.17s against 27.51s cold.
//!
//! # Why copies are cheap
//!
//! A target directory is hundreds of megabytes, so copying one per build would
//! cost more than it saves. Both APFS and Btrfs support copy-on-write clones,
//! where a copy shares blocks with its original until one of them is written.
//! `cp -c` on macOS and `cp --reflink` on Linux expose it, so a 713 MB snapshot
//! costs almost no time and almost no space.
//!
//! # Why mtimes are preserved
//!
//! Cargo compares source mtimes against output mtimes. A copy that rewrote them
//! would make every output look older than the sources that produced it, and the
//! build would start again from nothing: measured at 5.39s against 0.09s for a
//! copy that kept them.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the shared units live, which the snapshot walk must not descend into.
pub(crate) const UNITS_DIR: &str = "units";

/// Snapshots kept per lineage. Enough to cover switching between a handful of
/// branches, which is what an older snapshot is still worth an exact hit for.
const DEFAULT_KEEP: usize = 5;

use crate::key::{self, Plan};

/// How cargo will be asked to judge freshness for this build.
///
/// The choice has to be the same for a target directory's whole life. Recording a
/// directory under timestamps and then checking it under content hashing
/// invalidates every fingerprint at once: measured on tokio, a wiped-target
/// restore that should take 0.08s took 2.44s, with almost everything rebuilt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Freshness {
    /// Content hashing. Needed by a restored snapshot, whose sources may have
    /// newer timestamps than the outputs built from them.
    Checksum,
    /// Timestamps, which is cargo's own default. Needed by a directory filled from
    /// the shared unit store, whose entries content hashing rejects.
    Mtime,
}

impl Freshness {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Checksum => "checksum",
            Self::Mtime => "mtime",
        }
    }

    fn from_label(label: &str) -> Self {
        match label.trim() {
            "mtime" => Self::Mtime,
            // Snapshots recorded before the mode was written down were all made
            // under content hashing.
            _ => Self::Checksum,
        }
    }
}

/// What a restore managed to find.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    /// The recorded build describes this one, so nothing needs saving after.
    Exact,
    /// A snapshot of the same workspace built against a different dependency set.
    Near,
    /// Nothing usable, so the target directory is left for the unit store to fill.
    None,
}

/// Puts a recorded build back, reporting what it found and how it was recorded.
pub fn restore(plan: &Plan) -> (Hit, Freshness) {
    if env::var("CARGO_TURBO_OFF").as_deref() == Ok("1") {
        return (Hit::None, Freshness::Checksum);
    }
    // An exact match is best, but a snapshot of the same workspace built against
    // a different dependency set is still a far better starting point than
    // nothing: cargo's freshness pass rebuilds what actually differs. Without
    // this, a `cargo update` or a branch with another lock file fell all the way
    // back to a cold build.
    let (snapshot, exact) = match usable(&plan.snapshot()) {
        true => (plan.snapshot(), true),
        false if env::var("CARGO_TURBO_NEAR").as_deref() == Ok("0") => {
            return (Hit::None, Freshness::Checksum)
        }
        false => match nearest_in_lineage(plan) {
            Some(path) => (path, false),
            None => return (Hit::None, Freshness::Checksum),
        },
    };
    let tree = snapshot.join("target");

    // Never overwrite work in progress. A target directory that already exists
    // may hold a build newer than this snapshot, and cargo is better placed to
    // decide that than we are.
    if plan.target_dir.exists() {
        return (Hit::None, Freshness::Checksum);
    }

    if clone_tree(&tree, &plan.target_dir).is_err() {
        // A failed restore leaves nothing behind, so the build simply starts cold.
        let _ = fs::remove_dir_all(&plan.target_dir);
        return (Hit::None, Freshness::Checksum);
    }
    // The mode travels with the snapshot, because the fingerprints inside it only
    // read correctly under the one they were written by.
    let freshness = fs::read_to_string(snapshot.join("mode"))
        .map(|m| Freshness::from_label(&m))
        .unwrap_or(Freshness::Checksum);
    if freshness == Freshness::Checksum {
        let _ = fs::write(plan.target_dir.join(MARKER), b"checksum-freshness\n");
    }
    if exact {
        eprintln!("cargo-turbo: restored {}", plan.key);
    } else {
        eprintln!("cargo-turbo: restored a near match, cargo will rebuild the difference");
    }
    // Only an exact match means the recorded build describes this one, so an
    // inexact restore still gets saved under its own key afterwards.
    (if exact { Hit::Exact } else { Hit::Near }, freshness)
}

/// Whether a snapshot directory holds a finished recording.
fn usable(snapshot: &Path) -> bool {
    snapshot.join("complete").exists() && snapshot.join("target").is_dir()
}

/// The most recently recorded snapshot of the same workspace and profile.
///
/// Recency is the right choice among near matches: the newest build of a
/// workspace shares the most with the next one, whatever moved in between.
fn nearest_in_lineage(plan: &Plan) -> Option<PathBuf> {
    lineage_members(&plan.store, &plan.lineage)
        .into_iter()
        .next()
        .map(|(_, path)| path)
}

/// Every complete snapshot of one workspace, profile and command, newest first.
///
/// Ordered by the marker's timestamp, which is written last and so dates the
/// moment the snapshot became restorable.
fn lineage_members(store: &Path, lineage: &str) -> Vec<(std::time::SystemTime, PathBuf)> {
    let mut found = Vec::new();
    let mut stack = vec![store.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // The shared units live under here in their hundreds and none of
            // them is a snapshot, so descending into them walks the largest part
            // of the store to no purpose. With 665 units in the store the walk
            // took 0.081s, and it grows with every project that shares it.
            if entry.file_name() == UNITS_DIR {
                continue;
            }
            if !usable(&path) {
                stack.push(path);
                continue;
            }
            // Only snapshots of this workspace and profile, so a restore never
            // hands cargo artifacts from an unrelated project.
            if fs::read_to_string(path.join("lineage"))
                .ok()
                .as_deref()
                .map(str::trim)
                != Some(lineage)
            {
                continue;
            }
            let when = fs::metadata(path.join("complete"))
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            found.push((when, path));
        }
    }
    found.sort_by_key(|(when, _)| std::cmp::Reverse(*when));
    found
}

/// Removes all but the newest `keep` snapshots of one lineage.
///
/// Nothing else ever removes a snapshot. A lineage gains one for every distinct
/// `Cargo.lock` a workspace is built with, so a branch a day is a snapshot a
/// day, kept for the life of the machine. That is close to free while the target
/// directory they were cloned from still holds the same blocks, and stops being
/// free the moment it is rebuilt: the old blocks then belong to the snapshot
/// alone and start costing what they claim to.
///
/// Only whole snapshots are removed, and never the newest, so a near match is
/// always still available. A build restoring from one that is being removed sees
/// the copy fail and compiles instead, which is the same outcome as a miss.
fn prune_lineage(store: &Path, lineage: &str, keep: usize) -> usize {
    let _t = Timer::new("prune");
    lineage_members(store, lineage)
        .into_iter()
        .skip(keep.max(1))
        .filter(|(_, path)| fs::remove_dir_all(path).is_ok())
        .count()
}

/// How many snapshots of one lineage are kept, newest first.
fn keep_limit() -> usize {
    env::var("CARGO_TURBO_KEEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_KEEP)
        .max(1)
}

/// Records a target directory, if it is worth recording.
pub fn save(plan: &Plan, freshness: Freshness) {
    let _t = Timer::new("save");
    if env::var("CARGO_TURBO_OFF").as_deref() == Ok("1") || !plan.target_dir.is_dir() {
        return;
    }
    let snapshot = plan.snapshot();
    if snapshot.join("complete").exists() {
        return;
    }

    // Staged under a different name and renamed, so a concurrent build sees
    // either nothing or a finished snapshot. The marker is written last for the
    // same reason: its presence is what makes a snapshot restorable.
    let staging = snapshot.with_extension(format!("staging-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    if fs::create_dir_all(&staging).is_err() {
        return;
    }
    if clone_tree(&plan.target_dir, &staging.join("target")).is_err() {
        let _ = fs::remove_dir_all(&staging);
        return;
    }
    // Written before the marker, so a snapshot is never advertised as complete
    // without the lineage a near-match restore needs to check, or the mode it has
    // to be read under.
    if fs::write(staging.join("mode"), freshness.label().as_bytes()).is_err() {
        let _ = fs::remove_dir_all(&staging);
        return;
    }
    if fs::write(staging.join("lineage"), plan.lineage.as_bytes()).is_err() {
        let _ = fs::remove_dir_all(&staging);
        return;
    }
    if fs::write(staging.join("complete"), plan.key.as_bytes()).is_err() {
        let _ = fs::remove_dir_all(&staging);
        return;
    }
    if let Some(parent) = snapshot.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Losing this race is success: the other build recorded the same inputs.
    if fs::rename(&staging, &snapshot).is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    // Only after the new snapshot is in place, so the lineage is never empty and
    // the one just written is the newest and so never a candidate.
    prune_lineage(&plan.store, &plan.lineage, keep_limit());
}

/// Runs cargo for this plan, with the flags that make a restore usable.
pub fn forward(plan: &Plan, args: &[String], freshness: Freshness) -> i32 {
    let mut command = cargo();
    command.args(args);

    // Only the thread allocation and content hashing need unstable flags, and
    // neither the snapshot nor the shared dependencies do, so stable gets the same
    // reuse and simply gives up the extra cores: tokio measured 0.10s on stable
    // for a wiped target directory and 2.04s for a checkout never built before,
    // matching nightly on both.
    if plan.nightly && env::var("CARGO_TURBO_OFF").as_deref() != Ok("1") {
        // Threads are free to add anywhere: cargo never sees the argument, so
        // fingerprints are untouched and an ordinary `cargo` run afterwards finds
        // the directory exactly as it left it.
        install_wrapper(&mut command);

        // Content hashing rather than timestamps, when this build is one of the
        // ones that wants it. Both modes exist because they suit opposite cases,
        // and `Freshness` is where that choice is explained.
        if freshness == Freshness::Checksum
            && owns_target_dir(&plan.target_dir)
            && !args.iter().any(|a| a.contains("checksum-freshness"))
        {
            command.args(["-Z", "checksum-freshness"]);
        }
    }
    run(command)
}

/// The file marking a target directory as one this tool set up.
const MARKER: &str = ".cargo-turbo-checksums";

/// Whether the fingerprints in this target directory were written in checksum
/// mode, so asking for it again is free.
///
/// Cargo rejects fingerprints written in the other mode wholesale, so switching
/// rebuilds everything. A directory that plain `cargo` built is therefore left in
/// timestamp mode: adding the flag to it would rebuild the world, and dropping it
/// again on the next plain `cargo` run would rebuild the world a second time.
/// Measured while alternating the two commands over rust-analyzer, that cost
/// 16.91s and then 22.71s where an ordinary edit costs 4.20s.
fn owns_target_dir(target_dir: &Path) -> bool {
    if target_dir.join(MARKER).exists() {
        return true;
    }
    // An absent or empty directory holds no fingerprints to be inconsistent
    // with, so this is the one moment the mode can be chosen.
    let empty = !target_dir.exists()
        || fs::read_dir(target_dir)
            .map(|d| d.count() == 0)
            .unwrap_or(false);
    if empty {
        let _ = fs::create_dir_all(target_dir);
        return fs::write(target_dir.join(MARKER), b"checksum-freshness\n").is_ok();
    }
    false
}

/// Runs cargo with nothing added, for when a plan could not be resolved.
pub fn forward_plain(args: &[String]) -> i32 {
    let mut command = cargo();
    command.args(args);
    run(command)
}

fn install_wrapper(command: &mut Command) {
    // Only if the user has not asked for their own wrapper, which would
    // otherwise be silently replaced.
    if env::var_os("RUSTC_WRAPPER").is_some() {
        return;
    }
    if let Ok(self_path) = env::current_exe() {
        command.env("RUSTC_WRAPPER", self_path);
        command.env(crate::WRAPPER_MARKER, "1");
    }
}

fn cargo() -> Command {
    Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

fn run(mut command: Command) -> i32 {
    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("cargo-turbo: could not run cargo: {e}");
            1
        }
    }
}

/// Copies a tree, sharing blocks where the filesystem allows and always keeping
/// mtimes.
pub(crate) fn clone_tree(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    #[cfg(target_os = "macos")]
    if clonefile_tree(from, to) {
        return Ok(());
    }

    // `-c` asks APFS to clone per file; `--reflink=auto` asks Btrfs and XFS, and
    // falls back to a plain copy elsewhere. `-p` is what keeps the mtimes.
    let attempts: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["-Rpc"], &["-Rp"]]
    } else {
        &[&["-a", "--reflink=auto"], &["-a"]]
    };

    for flags in attempts {
        let status = Command::new("cp").args(*flags).arg(from).arg(to).status();
        if matches!(status, Ok(s) if s.success()) {
            return Ok(());
        }
        let _ = fs::remove_dir_all(to);
    }
    Err(format!(
        "could not copy {} to {}",
        from.display(),
        to.display()
    ))
}

/// Clones a whole tree in one call, on APFS.
///
/// `clonefile` takes a directory and reproduces everything beneath it, sharing
/// blocks and keeping timestamps. Doing that in the kernel rather than walking
/// the tree is the difference between 41 ms and 565 ms for the 2,639 files of a
/// rust-analyzer target directory, which is most of what a warm restore costs.
///
/// The destination must not exist, and `false` sends the caller to `cp`.
#[cfg(target_os = "macos")]
fn clonefile_tree(from: &Path, to: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn clonefile(src: *const std::ffi::c_char, dst: *const std::ffi::c_char, flags: u32)
            -> i32;
    }

    let (Ok(src), Ok(dst)) = (
        CString::new(from.as_os_str().as_bytes()),
        CString::new(to.as_os_str().as_bytes()),
    ) else {
        return false;
    };
    // Safe: both pointers are valid nul-terminated paths for the duration of the
    // call, and the flag set is empty.
    let result = unsafe { clonefile(src.as_ptr(), dst.as_ptr(), 0) };
    if result != 0 {
        // A partial clone would be worse than none, so anything left behind goes.
        let _ = fs::remove_dir_all(to);
        return false;
    }
    true
}

pub fn status() -> i32 {
    let store = key::store_dir();
    if !store.is_dir() {
        println!("cargo-turbo: nothing stored yet ({})", store.display());
        return 0;
    }
    let mut count = 0usize;
    let mut stack = vec![store.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // The shared units are counted separately, and there are hundreds of
            // them, so the snapshot walk stops at their door.
            if entry.file_name() == UNITS_DIR {
                continue;
            }
            if path.join("complete").exists() {
                count += 1;
            } else if path.is_dir() {
                stack.push(path);
            }
        }
    }

    // Prebuilt dependencies, which are what a project the store has never seen
    // draws on, grouped by toolchain and profile.
    let mut units = 0usize;
    if let Ok(scopes) = fs::read_dir(store.join(UNITS_DIR)) {
        for scope in scopes.flatten() {
            units += fs::read_dir(scope.path()).map_or(0, |e| e.count());
        }
    }
    // `du` reports what the files claim to occupy, which for clones is far more
    // than they cost: a 2.6 GB store measured 2 MB of actual consumption,
    // because every block is shared with the target directory it came from until
    // one of them is written. Labelled as logical so the number is not read as
    // disk pressure.
    let du = Command::new("du").arg("-sh").arg(&store).output();
    let size = du
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo-turbo: {count} snapshots and {units} shared dependency units");
    println!("             {size} logical, shared with the target directories they came from");
    println!("             in {}", store.display());
    0
}

pub fn clean() -> i32 {
    let store = key::store_dir();
    // Finished builds leave their ticket directories behind, and nothing else
    // reclaims them, so this is the only thing that ever does.
    let tickets = crate::wrapper::clean_tickets();
    if tickets > 0 {
        println!("cargo-turbo: removed {tickets} leftover ticket directories");
    }
    match fs::remove_dir_all(&store) {
        Ok(()) => {
            println!("cargo-turbo: removed {}", store.display());
            0
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => {
            eprintln!("cargo-turbo: could not remove {}: {e}", store.display());
            1
        }
    }
}

/// Reports how long one phase of the wrapper's own work took.
///
/// The phases are cheap and easy to assume are cheap, which is how a walk of the
/// whole store ended up running on every save and went unnoticed at 0.081s. This
/// is what caught it, so it stays.
pub(crate) struct Timer(&'static str, std::time::Instant);

impl Timer {
    pub(crate) fn new(phase: &'static str) -> Self {
        Timer(phase, std::time::Instant::now())
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if env::var("CARGO_TURBO_TIME").is_ok() {
            eprintln!(
                "cargo-turbo: {} took {:.3}s",
                self.0,
                self.1.elapsed().as_secs_f64()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finished snapshot on disk, as `save` would leave it.
    fn record(store: &Path, key: &str, lineage: &str) -> PathBuf {
        let dir = store.join(&key[..2]).join(key);
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::write(dir.join("lineage"), lineage).unwrap();
        fs::write(dir.join("complete"), key).unwrap();
        dir
    }

    fn plan_for(store: &Path, key: &str, lineage: &str) -> Plan {
        Plan {
            key: key.into(),
            lineage: lineage.into(),
            profile_dirs: vec!["debug".into()],
            toolchain: "test".into(),
            lock_contents: String::new(),
            store: store.to_path_buf(),
            target_dir: store.join("unused-target"),
            nightly: true,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("turbo-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_near_match_is_only_taken_from_the_same_lineage() {
        // Restoring another project's target directory would hand cargo artifacts
        // for crates this build has never heard of.
        let store = scratch("lineage");
        let mine = record(&store, "aa11", "mine");
        record(&store, "bb22", "theirs");

        assert_eq!(
            nearest_in_lineage(&plan_for(&store, "cc33", "mine")),
            Some(mine)
        );
        assert_eq!(
            nearest_in_lineage(&plan_for(&store, "cc33", "nobody")),
            None
        );

        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn an_unfinished_snapshot_is_never_a_near_match() {
        // Without the marker a snapshot may be a half-written directory, and
        // handing that to cargo is worse than starting cold. The marker is written
        // last by `save` for exactly this reason.
        let store = scratch("partial");
        let dir = store.join("aa").join("aa11");
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::write(dir.join("lineage"), "mine").unwrap();

        let plan = plan_for(&store, "cc33", "mine");
        assert_eq!(nearest_in_lineage(&plan), None);

        fs::write(dir.join("complete"), "aa11").unwrap();
        assert_eq!(nearest_in_lineage(&plan), Some(dir));

        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn pruning_keeps_the_newest_and_spares_other_lineages() {
        let store = scratch("prune");
        let at = |n| std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(n);
        // Four snapshots of one lineage, and one of another that must survive
        // whatever happens to the first.
        let mut mine = Vec::new();
        for (i, key) in ["aa11", "bb22", "cc33", "dd44"].iter().enumerate() {
            let dir = record(&store, key, "mine");
            set_modified(&dir.join("complete"), at(1_000 + i as u64 * 100));
            mine.push(dir);
        }
        let theirs = record(&store, "ee55", "yours");
        set_modified(&theirs.join("complete"), at(500));

        assert_eq!(prune_lineage(&store, "mine", 2), 2);

        assert!(!mine[0].exists(), "the oldest should go");
        assert!(!mine[1].exists(), "the next oldest should go");
        assert!(mine[2].exists(), "the second newest is kept");
        assert!(mine[3].exists(), "the newest is always kept");
        assert!(theirs.exists(), "another lineage must be untouched");

        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn pruning_never_empties_a_lineage() {
        let store = scratch("prune-floor");
        let only = record(&store, "aa11", "mine");
        // A keep of zero would otherwise remove the very snapshot a near match
        // depends on, leaving the next build to start cold.
        assert_eq!(prune_lineage(&store, "mine", 0), 0);
        assert!(only.exists());
        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn the_newest_snapshot_of_a_lineage_wins() {
        // Among near matches the most recent build shares the most with the next
        // one, whatever moved in between.
        let store = scratch("newest");
        let older = record(&store, "aa11", "mine");
        let newer = record(&store, "bb22", "mine");
        // Recorded order is not guaranteed to be directory order, so the times are
        // set explicitly.
        let long_ago = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let recently = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000);
        set_modified(&older.join("complete"), long_ago);
        set_modified(&newer.join("complete"), recently);

        assert_eq!(
            nearest_in_lineage(&plan_for(&store, "cc33", "mine")),
            Some(newer)
        );

        let _ = fs::remove_dir_all(&store);
    }

    #[test]
    fn the_mode_a_snapshot_was_recorded_under_survives_a_round_trip() {
        // Restoring a timestamp-recorded snapshot and then checking it under
        // content hashing invalidates every fingerprint: 0.08s became 2.44s.
        assert_eq!(Freshness::from_label("mtime"), Freshness::Mtime);
        assert_eq!(Freshness::from_label("checksum"), Freshness::Checksum);
        assert_eq!(
            Freshness::from_label(Freshness::Mtime.label()),
            Freshness::Mtime
        );
        // Snapshots recorded before the mode was written down were all made under
        // content hashing, so that is what a missing or unreadable label means.
        assert_eq!(Freshness::from_label(""), Freshness::Checksum);
        assert_eq!(Freshness::from_label("something else"), Freshness::Checksum);
    }

    #[test]
    fn an_exact_key_is_preferred_over_any_near_match() {
        // Cargo has to do no work at all for an exact hit, so a near match is only
        // ever a fallback.
        let store = scratch("exact");
        record(&store, "aa11", "mine");
        let plan = plan_for(&store, "aa11", "mine");
        assert!(usable(&plan.snapshot()));

        let _ = fs::remove_dir_all(&store);
    }

    /// Stamping an mtime without depending on a crate to do it.
    fn set_modified(path: &Path, when: std::time::SystemTime) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }
}
