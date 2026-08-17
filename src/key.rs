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
    /// The directories under `target` cargo will write into, relative to it.
    ///
    /// Usually one, named after the profile. A build with `--target` has two: the
    /// units for that target go under `<triple>/<profile>`, and the ones for the
    /// host running the compiler -- proc macros and build scripts -- stay in
    /// `<profile>`. Both are worth sharing, and they must be kept apart, or an
    /// entry would be offered to the directory it did not come from.
    pub profile_dirs: Vec<String>,
    /// Identifies the compiler and the flags, for scoping the shared unit store.
    pub toolchain: String,
    /// `Cargo.lock` as read, so which packages are local can be answered without
    /// reading it a second time.
    pub lock_contents: String,
    /// Identifies this workspace and profile without the resolved dependencies.
    ///
    /// An exact key changes whenever `Cargo.lock` does, so a dependency bump or a
    /// branch with a different lock file missed the cache entirely and started
    /// cold: measured on tokio, 0.10s became 2.75s. The lineage groups every
    /// snapshot of one workspace and profile, so the nearest one can be restored
    /// as a baseline and cargo asked to reconcile the difference.
    pub lineage: String,
}

impl Plan {
    pub fn resolve(args: &[String]) -> Result<Self, String> {
        let manifest_dir = workspace_root()?;
        let lock = manifest_dir.join("Cargo.lock");

        // The compiler, because metadata from one rustc is not loadable by
        // another, and the cargo, because fingerprint formats change.
        let rustc = tool_version("rustc")?;
        let cargo = tool_version("cargo")?;

        // Everything except the resolved dependencies. Two builds sharing this
        // are close enough that one is a useful starting point for the other.
        let mut shared = String::new();
        let _ = writeln!(shared, "rustc:{rustc}\ncargo:{cargo}");
        let _ = writeln!(shared, "root:{}", manifest_dir.display());

        // Anything on the command line that changes what gets built. Paths are
        // excluded deliberately: the same build in two checkouts should share.
        for arg in args.iter().filter(|a| !is_irrelevant(a)) {
            let _ = writeln!(shared, "arg:{arg}");
        }

        // Flags cargo folds into its own fingerprints.
        for var in [
            "RUSTFLAGS",
            "RUSTDOCFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_PROFILE",
        ] {
            if let Some(v) = env::var_os(var) {
                let _ = writeln!(shared, "{var}:{}", v.to_string_lossy());
            }
        }

        // The resolved dependency set, which is what separates one snapshot of a
        // workspace from another.
        let lock_contents = fs::read_to_string(&lock)
            .map_err(|e| format!("cannot read {}: {e}", lock.display()))?;
        let lineage = format!("{:016x}", hash(shared.as_bytes()));
        let key = format!(
            "{:016x}",
            hash(format!("{lineage}\nlock:{:016x}", hash(lock_contents.as_bytes())).as_bytes())
        );

        Ok(Self {
            key,
            lineage,
            profile_dirs: profile_dirs(args),
            // Everything the shared unit store must not mix, which is the compiler
            // and any flag that reaches it.
            toolchain: format!(
                "{rustc}|{cargo}|{}",
                ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"]
                    .iter()
                    .filter_map(|v| env::var(v).ok())
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            lock_contents,
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

/// Which directories under `target` cargo will write into, relative to it.
///
/// Needed before cargo runs, because prebuilt units are copied in beforehand and
/// they have to land where cargo will look for them.
fn profile_dirs(args: &[String]) -> Vec<String> {
    let profile = profile_name(args);
    match explicit_target(args) {
        // Cross-compiling splits the output in two. The host directory holds the
        // proc macros and build scripts, which are compiled for the machine doing
        // the compiling whatever the build is aimed at.
        Some(triple) => vec![format!("{triple}/{profile}"), profile],
        None => vec![profile],
    }
}

/// The profile directory's name.
///
/// Cargo names it after the profile, except that the two built-in profiles have
/// historical names and the two that inherit from them share theirs.
fn profile_name(args: &[String]) -> String {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--release" || arg == "-r" {
            return "release".into();
        }
        let named = if arg == "--profile" {
            args.next().cloned()
        } else {
            arg.strip_prefix("--profile=").map(str::to_owned)
        };
        if let Some(profile) = named {
            return match profile.as_str() {
                "dev" | "test" => "debug".into(),
                "bench" => "release".into(),
                other => other.into(),
            };
        }
    }
    "debug".into()
}

/// The target triple asked for on the command line, if any.
fn explicit_target(args: &[String]) -> Option<String> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--target" {
            return args.next().cloned();
        }
        if let Some(triple) = arg.strip_prefix("--target=") {
            return Some(triple.to_owned());
        }
    }
    // Cargo also reads this, and a project that sets it cross-compiles every build.
    env::var("CARGO_BUILD_TARGET").ok()
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

/// The `-vV` banner of a tool.
///
/// This spawns a process on every run, and caching the answer against the
/// binary's size and modification time was tried and removed: on a warm tokio
/// restore it moved the median from 0.65s to 0.65s, so the two spawns are a few
/// milliseconds rather than the tenth of a second guessed at. The cache only
/// added a way to be wrong about which compiler was in use.
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

    /// Spelling out an argument list without the noise.
    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_profile_directory_matches_where_cargo_writes() {
        assert_eq!(profile_name(&args(&["check"])), "debug");
        assert_eq!(profile_name(&args(&["build", "--release"])), "release");
        assert_eq!(profile_name(&args(&["build", "-r"])), "release");
        // A custom profile gets a directory of its own.
        assert_eq!(profile_name(&args(&["build", "--profile=fast"])), "fast");
        assert_eq!(profile_name(&args(&["build", "--profile", "fast"])), "fast");
        // The built-in ones do not, which is what makes this worth a test: seeding
        // `target/test` would put prebuilt units where cargo never looks.
        assert_eq!(profile_name(&args(&["test", "--profile", "test"])), "debug");
        assert_eq!(
            profile_name(&args(&["bench", "--profile", "bench"])),
            "release"
        );
    }

    #[test]
    fn cross_compiling_has_two_output_directories() {
        // Getting this wrong is silent: nothing is offered and nothing is stored,
        // because neither directory is where the units actually are. It matters for
        // every release build that names a target, which is most of them.
        assert_eq!(
            profile_dirs(&args(&[
                "build",
                "--release",
                "--target",
                "x86_64-unknown-linux-musl"
            ])),
            ["x86_64-unknown-linux-musl/release", "release"]
        );
        assert_eq!(
            profile_dirs(&args(&[
                "zigbuild",
                "--release",
                "--target=aarch64-unknown-linux-musl"
            ])),
            ["aarch64-unknown-linux-musl/release", "release"]
        );
        // The host directory is the smaller half: it holds only the proc macros and
        // build scripts, which are built for the machine doing the compiling
        // whatever the build is aimed at.
        assert_eq!(profile_dirs(&args(&["check"])), ["debug"]);
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
            lineage: "0000000000000000".into(),
            profile_dirs: vec!["debug".into()],
            toolchain: "test".into(),
            lock_contents: String::new(),
            store: PathBuf::from("/store"),
            target_dir: PathBuf::from("/t"),
            nightly: true,
        };
        assert_eq!(plan.snapshot(), PathBuf::from("/store/ab/abcdef0123456789"));
    }
}
