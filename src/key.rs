//! Deciding whether a recorded build describes the one about to run.
//!
//! The key has to cover everything that determined the target directory's
//! contents, because restoring a directory produced by different inputs would
//! hand cargo a set of artifacts it will happily believe. Cargo re-checks
//! freshness afterwards and rebuilds what it must, so a key that is too coarse
//! costs a rebuild rather than a wrong answer. A key that is too *fine* costs a
//! miss, which is why the resolved dependency set is hashed rather than every
//! file in the workspace.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Everything needed to run and record one accelerated build.
pub struct Plan {
    /// Identifies the recorded build, and names its directory.
    pub key: String,
    /// Where snapshots are stored.
    pub store: PathBuf,
    /// The target directory cargo will use.
    pub target_dir: PathBuf,
    /// Whether the toolchain accepts the unstable flags this relies on.
    pub nightly: bool,
}

impl Plan {
    pub fn resolve(args: &[String]) -> Result<Self, String> {
        let manifest_dir = workspace_root()?;
        let lock = manifest_dir.join("Cargo.lock");

        let mut input = String::new();
        // The resolved dependency set. This is the whole point of the key: it
        // changes when a dependency version or source does, and not when an
        // unrelated source file is edited.
        let lock_contents =
            fs::read(&lock).map_err(|e| format!("cannot read {}: {e}", lock.display()))?;
        let _ = writeln!(input, "lock:{:016x}", hash(&lock_contents));

        // The compiler, because metadata from one rustc is not loadable by
        // another, and the cargo, because fingerprint formats change.
        let rustc = tool_version("rustc")?;
        let cargo = tool_version("cargo")?;
        let _ = writeln!(input, "rustc:{rustc}\ncargo:{cargo}");

        // Anything on the command line that changes what gets built. Paths are
        // excluded deliberately: the same build in two checkouts should share.
        for arg in args.iter().filter(|a| !is_irrelevant(a)) {
            let _ = writeln!(input, "arg:{arg}");
        }

        // Flags cargo folds into its own fingerprints.
        for var in [
            "RUSTFLAGS",
            "RUSTDOCFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_PROFILE",
        ] {
            if let Some(v) = env::var_os(var) {
                let _ = writeln!(input, "{var}:{}", v.to_string_lossy());
            }
        }

        Ok(Self {
            key: format!("{:016x}", hash(input.as_bytes())),
            store: store_dir(),
            target_dir: manifest_dir.join("target"),
            nightly: rustc.contains("nightly") || rustc.contains("dev"),
        })
    }

    /// Where this build's snapshot lives.
    pub fn snapshot(&self) -> PathBuf {
        // Two levels of fan-out, so the store stays quick to enumerate.
        self.store.join(&self.key[..2]).join(&self.key)
    }
}

/// Whether an argument should be left out of the key.
///
/// Two kinds are excluded. Locations, because two checkouts of the same commit
/// should share a snapshot rather than each keeping their own. And anything that
/// only changes what cargo prints, because a quiet build and a verbose one
/// produce identical artifacts and splitting them would double the store for
/// nothing.
fn is_irrelevant(arg: &str) -> bool {
    const OUTPUT_ONLY: &[&str] = &[
        "-q",
        "--quiet",
        "-v",
        "--verbose",
        "-vv",
        "--color",
        "--message-format",
        "--timings",
    ];
    arg.contains('/')
        || arg.starts_with("--target-dir")
        || arg.starts_with("--manifest-path")
        || OUTPUT_ONLY
            .iter()
            .any(|f| arg == *f || arg.starts_with(&format!("{f}=")))
}

/// The directory holding `Cargo.lock`, found by walking up from the current one.
fn workspace_root() -> Result<PathBuf, String> {
    let mut dir = env::current_dir().map_err(|e| e.to_string())?;
    loop {
        if dir.join("Cargo.lock").is_file() {
            return Ok(dir);
        }
        // A workspace member without its own lock file still resolves, because
        // the lock lives at the workspace root above it.
        if !dir.pop() {
            return Err("no Cargo.lock in this directory or any parent".into());
        }
    }
}

fn tool_version(tool: &str) -> Result<String, String> {
    let out = Command::new(tool)
        .arg("-vV")
        .output()
        .map_err(|e| format!("cannot run {tool}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{tool} -vV failed"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

pub fn store_dir() -> PathBuf {
    if let Some(dir) = env::var_os("CARGO_TURBO_DIR") {
        return PathBuf::from(dir);
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    home.join(".cache").join("cargo-turbo")
}

/// FNV-1a, 64 bit.
///
/// Chosen for having no dependency and no start-up cost. The key identifies a
/// build rather than authenticating it, and a collision costs a rebuild that
/// cargo would then correct, so a cryptographic digest buys nothing here.
pub fn hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_do_not_enter_the_key() {
        // Otherwise two checkouts of one commit could never share a snapshot.
        assert!(is_irrelevant("--target-dir=/tmp/x"));
        assert!(is_irrelevant("/abs/path"));
        assert!(is_irrelevant("--manifest-path=Cargo.toml"));
    }

    #[test]
    fn verbosity_does_not_enter_the_key() {
        // A quiet build and a verbose one produce the same artifacts.
        for flag in [
            "-q",
            "--quiet",
            "-v",
            "--verbose",
            "--color=never",
            "--message-format=json",
        ] {
            assert!(is_irrelevant(flag), "{flag} should not affect the key");
        }
    }

    #[test]
    fn anything_that_changes_the_build_does_enter_the_key() {
        for flag in [
            "check",
            "build",
            "--workspace",
            "--release",
            "--all-features",
            "-p",
            "test",
        ] {
            assert!(!is_irrelevant(flag), "{flag} must affect the key");
        }
    }

    #[test]
    fn hashing_is_stable_and_distinguishes_inputs() {
        assert_eq!(hash(b"cargo turbo"), hash(b"cargo turbo"));
        assert_ne!(hash(b"check --workspace"), hash(b"check"));
        // A single flipped bit has to move it, or near-identical lock files
        // would collide.
        assert_ne!(hash(b"rustc 1.99.0"), hash(b"rustc 1.99.1"));
    }

    #[test]
    fn snapshot_paths_fan_out_by_key_prefix() {
        let plan = Plan {
            key: "abcdef0123456789".into(),
            store: PathBuf::from("/store"),
            target_dir: PathBuf::from("/t"),
            nightly: true,
        };
        assert_eq!(plan.snapshot(), PathBuf::from("/store/ab/abcdef0123456789"));
    }
}
