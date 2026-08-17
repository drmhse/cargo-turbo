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

use crate::key::{self, Plan};

/// Puts a recorded build back, returning whether anything was restored.
pub fn restore(plan: &Plan) -> bool {
    if env::var("CARGO_TURBO_OFF").as_deref() == Ok("1") {
        return false;
    }
    // An exact match is best, but a snapshot of the same workspace built against
    // a different dependency set is still a far better starting point than
    // nothing: cargo's freshness pass rebuilds what actually differs. Without
    // this, a `cargo update` or a branch with another lock file fell all the way
    // back to a cold build.
    let (snapshot, exact) = match usable(&plan.snapshot()) {
        true => (plan.snapshot(), true),
        false if env::var("CARGO_TURBO_NEAR").as_deref() == Ok("0") => return false,
        false => match nearest_in_lineage(plan) {
            Some(path) => (path, false),
            None => return false,
        },
    };
    let tree = snapshot.join("target");

    // Never overwrite work in progress. A target directory that already exists
    // may hold a build newer than this snapshot, and cargo is better placed to
    // decide that than we are.
    if plan.target_dir.exists() {
        return false;
    }

    if clone_tree(&tree, &plan.target_dir).is_err() {
        // A failed restore leaves nothing behind, so the build simply starts cold.
        let _ = fs::remove_dir_all(&plan.target_dir);
        return false;
    }
    // Carried with the restore, because the snapshot was recorded in checksum
    // mode and the fingerprints inside it only read correctly that way.
    let _ = fs::write(plan.target_dir.join(MARKER), b"checksum-freshness\n");
    if exact {
        eprintln!("cargo-turbo: restored {}", plan.key);
    } else {
        eprintln!("cargo-turbo: restored a near match, cargo will rebuild the difference");
    }
    // Only an exact match means the recorded build describes this one, so an
    // inexact restore still gets saved under its own key afterwards.
    exact
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
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut stack = vec![plan.store.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
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
                != Some(plan.lineage.as_str())
            {
                continue;
            }
            let when = fs::metadata(path.join("complete"))
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(b, _)| when > *b) {
                best = Some((when, path));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// Records a target directory, if it is worth recording.
pub fn save(plan: &Plan) {
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
    // without the lineage a near-match restore needs to check.
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
}

/// Runs cargo for this plan, with the flags that make a restore usable.
pub fn forward(plan: &Plan, args: &[String]) -> i32 {
    let mut command = cargo();
    command.args(args);

    // The wrapper and content hashing both need unstable flags, so on stable the
    // snapshot still restores and the rest is simply absent. Measured on
    // rust-analyzer: a wiped target directory returns in 0.57s on stable, and a
    // checkout with new timestamps rebuilds 51 of 309 units rather than 4.
    if plan.nightly && env::var("CARGO_TURBO_OFF").as_deref() != Ok("1") {
        // Threads are free to add anywhere: cargo never sees the argument, so
        // fingerprints are untouched and an ordinary `cargo` run afterwards finds
        // the directory exactly as it left it.
        install_wrapper(&mut command);

        if owns_target_dir(&plan.target_dir) {
            // Content hashing rather than timestamps. Without it a fresh clone
            // gives every source a newer timestamp than the restored outputs and
            // the whole build starts again.
            if !args.iter().any(|a| a.contains("checksum-freshness")) {
                command.args(["-Z", "checksum-freshness"]);
            }
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
fn clone_tree(from: &Path, to: &Path) -> Result<(), String> {
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
            if path.join("complete").exists() {
                count += 1;
            } else if path.is_dir() {
                stack.push(path);
            }
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
    println!("cargo-turbo: {count} snapshots, {size} logical (shared with the target directories they came from)");
    println!("             in {}", store.display());
    0
}

pub fn clean() -> i32 {
    let store = key::store_dir();
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
