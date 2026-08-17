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
use std::path::Path;
use std::process::Command;

use crate::key::{self, Plan};

/// Puts a recorded build back, returning whether anything was restored.
pub fn restore(plan: &Plan) -> bool {
    if env::var("CARGO_TURBO_OFF").as_deref() == Ok("1") {
        return false;
    }
    let snapshot = plan.snapshot();
    let tree = snapshot.join("target");
    if !snapshot.join("complete").exists() || !tree.is_dir() {
        return false;
    }

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
    eprintln!("cargo-turbo: restored {}", plan.key);
    true
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

    if plan.nightly && env::var("CARGO_TURBO_OFF").as_deref() != Ok("1") {
        // Content hashing rather than mtimes. Without it a fresh clone gives
        // every source a newer mtime than the restored outputs and the whole
        // build starts again. It has to be set for the build that *records* the
        // snapshot as well, because fingerprints written in one mode are
        // rejected wholesale by the other.
        if !args.iter().any(|a| a.contains("checksum-freshness")) {
            command.args(["-Z", "checksum-freshness"]);
        }
        install_wrapper(&mut command);
    }
    run(command)
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

    // `-c` asks APFS to clone; `--reflink=auto` asks Btrfs and XFS, and falls
    // back to a plain copy elsewhere. `-p` is what keeps the mtimes.
    let attempts: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["-Rpc"], &["-Rp"]]
    } else {
        &[&["-a", "--reflink=auto"], &["-a"]]
    };

    for flags in attempts {
        let status = Command::new("cp")
            .args(*flags)
            .arg(from)
            .arg(to)
            .status();
        if matches!(status, Ok(s) if s.success()) {
            return Ok(());
        }
        let _ = fs::remove_dir_all(to);
    }
    Err(format!("could not copy {} to {}", from.display(), to.display()))
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
        let Ok(entries) = fs::read_dir(&dir) else { continue };
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
