//! Sharing built dependencies between unrelated projects.
//!
//! A snapshot only helps a project that has been built before. The first build of
//! a project the store has never seen starts from nothing, even though most of it
//! is third-party code some other project on the machine has already compiled:
//! measured on rust-analyzer, 250 of 288 units are third-party and account for
//! 77% of the CPU.
//!
//! Those units are shareable, and cargo has already done the hard part of saying
//! so. Every artifact it writes is named `<crate>-<hash>`, where the hash covers
//! the package, its features, the profile, the compiler and the resolved
//! dependencies. Two unrelated projects that depend on the same version of a
//! crate with the same features produce the *same* names:
//!
//! ```text
//! project A   target/debug/build/serde_core/78063a3bdd8d7da4/
//! project B   target/debug/build/serde_core/78063a3bdd8d7da4/
//! ```
//!
//! and a project given the other's directories treats them as its own. Measured
//! on a project depending on `serde` with derive, target directory empty:
//!
//! | | cold | dependencies supplied by another project |
//! |---|---|---|
//! | nightly | 2.27s | **0.08s** |
//! | stable | 2.71s | **0.06s** |
//!
//! with nothing recompiled in either case.
//!
//! # Why this is sound
//!
//! The hash is cargo's own answer to "would this unit have to be rebuilt", so an
//! entry is only ever reused where cargo itself would consider it fresh. Measured
//! directly: the same `serde` version resolves to a different hash under a
//! different feature set and under a different profile.
//!
//! | build | hash |
//! |---|---|
//! | `features = ["derive"]` | `78063a3bdd8d7da4` |
//! | `default-features = false` | `54d6c93b70866410` |
//! | `features = ["derive", "rc"]` | `7d5bfdc3111bd661` |
//! | `features = ["derive"]`, release | `64b8f629e058aba9` |
//!
//! Cargo also re-checks freshness after a seed, exactly as it does after a
//! snapshot restore, so a wrong entry costs a rebuild rather than a wrong answer.
//!
//! # Only third-party units are stored
//!
//! A workspace's own crates are excluded. Their hashes are unique to a checkout,
//! so sharing them would achieve nothing, and keeping them out means an entry can
//! never stand in for a crate the developer is editing. Membership is read from
//! `Cargo.lock`, where a package with no `source` is a local one.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::key::{hash, Plan};
use crate::snapshot::Freshness;

/// One unit's files, as they sit relative to the profile directory.
///
/// Cargo splits a unit across up to three places, and a partial copy is worse
/// than none: leaving out the build-script output was measured to make every
/// dependent dirty again, turning a 0.06s build back into 2.20s.
struct Fragment {
    /// `<crate>-<hash>`, which is what makes the unit shareable.
    key: String,
    /// Paths relative to the profile directory, each a file or a directory.
    parts: Vec<PathBuf>,
}

/// Fills an empty target directory with dependencies other projects have built.
///
/// Returns how many units were supplied.
pub fn seed(plan: &Plan, freshness: Freshness) -> usize {
    let store = unit_store(plan, freshness);
    if !store.is_dir() {
        return 0;
    }
    let profile = plan.target_dir.join(&plan.profile_dir);

    // A directory cargo has already built in needs nothing: every entry offered
    // would be skipped in favour of what is there, and the walk is wasted. It also
    // covers the everyday case of rebuilding after an edit, which was paying for a
    // seed it could not use and being told units had been supplied when none were.
    if populated(&profile) {
        return 0;
    }

    // Only the packages this build actually resolves to, so an unrelated
    // project's crates are never copied in. The hash is unknown until cargo runs,
    // so every stored variant of a wanted package is offered and cargo picks the
    // names it asked for.
    let wanted = foreign_packages(plan);
    let mut seeded = 0;
    let Ok(entries) = fs::read_dir(&store) else {
        return 0;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(crate_name) = name.rsplit_once('-').map(|(c, _)| c) else {
            continue;
        };
        if !wanted.contains(crate_name) {
            continue;
        }
        if merge_into(&entry.path(), &profile).is_ok() {
            seeded += 1;
        }
    }
    if seeded > 0 {
        eprintln!("cargo-turbo: supplied {seeded} prebuilt units");
    }
    seeded
}

/// Adds this build's third-party units to the store.
pub fn record(plan: &Plan, freshness: Freshness, compiled: bool) {
    let profile = plan.target_dir.join(&plan.profile_dir);
    if !profile.is_dir() {
        return;
    }
    let store = unit_store(plan, freshness);
    // A build that compiled nothing has nothing new to offer, so the walk over
    // every unit in the directory is skipped -- which is every rebuild after an
    // edit and every exact snapshot hit. The exception is a store that has been
    // emptied: without it, a project whose builds always hit would never refill it.
    if !compiled && store.is_dir() {
        return;
    }
    // Only packages this build resolved from outside the workspace. Asking
    // instead which units are *not* local admitted anything that happened to be
    // sitting in the directory, and a near-match restore leaves another
    // project's crates sitting in it: the store filled up with entries named
    // after workspace crates of unrelated projects.
    let shareable = foreign_packages(plan);

    for fragment in fragments(&profile) {
        let Some(crate_name) = fragment.key.rsplit_once('-').map(|(c, _)| c) else {
            continue;
        };
        if !shareable.contains(crate_name) {
            continue;
        }
        let entry = store.join(&fragment.key);
        // First writer wins. An entry is addressed by a hash over everything that
        // produced it, so a second copy would be the same bytes.
        if entry.exists() {
            continue;
        }
        let staging = store.join(format!(".staging-{}-{}", std::process::id(), fragment.key));
        let _ = fs::remove_dir_all(&staging);
        if fs::create_dir_all(&staging).is_err() {
            continue;
        }
        let mut ok = true;
        for part in &fragment.parts {
            let to = staging.join(part);
            if let Some(parent) = to.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if crate::snapshot::clone_tree(&profile.join(part), &to).is_err() {
                ok = false;
                break;
            }
        }
        // Renamed into place, so a concurrent build sees a whole entry or none.
        if !ok || fs::rename(&staging, &entry).is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
    }
}

/// Whether cargo has already built in a profile directory.
fn populated(profile: &Path) -> bool {
    // `build` is where both layouts put their units, so its presence is the signal.
    // An empty one is left behind by an interrupted build and is worth filling.
    fs::read_dir(profile.join("build")).is_ok_and(|mut entries| entries.next().is_some())
}

/// Every unit found in a profile directory, in whichever layout cargo used.
fn fragments(profile: &Path) -> Vec<Fragment> {
    let mut found = Vec::new();

    // Both layouts use `build`, so which one this is has to be settled first. The
    // discriminator is `.fingerprint`, which only the older layout has: guessing
    // from directory names instead silently skipped every package whose name
    // contains a dash, such as `proc-macro2`, and left those units unshared.
    if !profile.join(".fingerprint").is_dir() {
        // One directory per package, one per hash inside it, holding both the
        // artifacts and the fingerprint.
        let Ok(packages) = fs::read_dir(profile.join("build")) else {
            return found;
        };
        for package in packages.flatten() {
            let name = package.file_name().to_string_lossy().into_owned();
            if !package.path().is_dir() {
                continue;
            }
            let Ok(hashes) = fs::read_dir(package.path()) else {
                continue;
            };
            for entry in hashes.flatten() {
                if !entry.path().is_dir() {
                    continue;
                }
                let hash = entry.file_name().to_string_lossy().into_owned();
                found.push(Fragment {
                    key: format!("{name}-{hash}"),
                    parts: vec![Path::new("build").join(&name).join(&hash)],
                });
            }
        }
        return found;
    }

    // The older layout: a unit's pieces are spread across three directories and
    // all of them are needed.
    let Ok(prints) = fs::read_dir(profile.join(".fingerprint")) else {
        return found;
    };
    for print in prints.flatten() {
        if !print.path().is_dir() {
            continue;
        }
        let key = print.file_name().to_string_lossy().into_owned();
        if !key.contains('-') {
            continue;
        }
        let mut parts = vec![Path::new(".fingerprint").join(&key)];
        if profile.join("build").join(&key).is_dir() {
            parts.push(Path::new("build").join(&key));
        }
        // `libserde_core-<hash>.rmeta` and `serde_core-<hash>.d` both belong to
        // the same unit, so the artifacts are matched on the hash.
        if let Some((_, hash)) = key.rsplit_once('-') {
            if let Ok(deps) = fs::read_dir(profile.join("deps")) {
                for dep in deps.flatten() {
                    let file = dep.file_name().to_string_lossy().into_owned();
                    if file.split('.').next().is_some_and(|s| s.ends_with(hash)) {
                        parts.push(Path::new("deps").join(&file));
                    }
                }
            }
        }
        found.push(Fragment { key, parts });
    }
    found
}

/// Copies an entry's tree over a profile directory, leaving what is already there.
///
/// The recursion matters. Merging only the top level meant that once
/// `build/serde` existed, no further `serde` entry could be added, so a package
/// with several feature variants contributed exactly one of them and every other
/// unit that wanted the rest was rebuilt.
fn merge_into(entry: &Path, profile: &Path) -> Result<(), String> {
    let Ok(items) = fs::read_dir(entry) else {
        return Err("unreadable entry".into());
    };
    for item in items.flatten() {
        let from = item.path();
        let to = profile.join(item.file_name());
        if to.exists() {
            // A directory the build already has may still be missing this entry's
            // contents, so the descent continues; a file it already has is its own
            // and cargo's copy is always the better one.
            if from.is_dir() && to.is_dir() {
                merge_into(&from, &to)?;
            }
            continue;
        }
        crate::snapshot::clone_tree(&from, &to)?;
    }
    Ok(())
}

/// Where entries live for this toolchain, profile and freshness mode.
///
/// The compiler and profile are belt and braces, since cargo's hash already
/// covers them: they keep a store from ever offering an entry built by a different
/// toolchain, and keep a debug build from being offered release entries.
///
/// The freshness mode is not optional. An entry recorded by a build that judged
/// freshness by content hashing is rejected by one judging it by timestamps, and
/// the other way round, so mixing them fills the store with entries that are
/// offered and then ignored: measured on rust-analyzer, 269 units supplied and 235
/// rebuilt regardless.
fn unit_store(plan: &Plan, freshness: Freshness) -> PathBuf {
    let scope = hash(
        format!(
            "{}|{}|{}",
            plan.toolchain,
            plan.profile_dir,
            freshness.label()
        )
        .as_bytes(),
    );
    plan.store.join("units").join(format!("{scope:016x}"))
}

/// Package names in `Cargo.lock`, split by whether they come from outside.
///
/// A package with no `source` is built from a path in this workspace, and a
/// registry or git package has one. Reading the lock file avoids a `cargo
/// metadata` spawn, and the lock file has already been read to build the key.
fn partition_packages(lock: &str) -> (HashSet<String>, HashSet<String>) {
    let mut local = HashSet::new();
    let mut foreign = HashSet::new();
    for block in lock.split("[[package]]").skip(1) {
        let mut name = None;
        let mut has_source = false;
        for line in block.lines() {
            let line = line.trim();
            if line.starts_with("[[") || line.starts_with('[') && line.ends_with(']') {
                break;
            }
            if let Some(rest) = line.strip_prefix("name = ") {
                name = Some(rest.trim_matches('"').to_owned());
            } else if line.starts_with("source = ") {
                has_source = true;
            }
        }
        if let Some(name) = name {
            // Cargo names artifacts after the crate, which replaces dashes with
            // underscores, so both spellings are recorded and either matches.
            let target = if has_source { &mut foreign } else { &mut local };
            target.insert(name.replace('-', "_"));
            target.insert(name);
        }
    }
    (local, foreign)
}

/// Packages this build resolves from a registry or a git remote.
///
/// This is the set that may be shared, in both directions: a unit is stored only
/// if its package is in here, and an entry is offered only if its package is too.
fn foreign_packages(plan: &Plan) -> HashSet<String> {
    partition_packages(&plan.lock_contents).1
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"
version = 4

[[package]]
name = "my-app"
version = "0.1.0"
dependencies = ["serde"]

[[package]]
name = "serde"
version = "1.0.229"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "abc"
"#;

    #[test]
    fn only_a_resolved_third_party_package_may_be_stored() {
        // The store is filled from whatever is in the target directory, and a
        // near-match restore leaves another project's crates in there. Storing
        // "everything that is not local" therefore admitted crates this build
        // never depended on.
        let (_, foreign) = partition_packages(LOCK);
        assert!(foreign.contains("serde"), "a resolved dependency");
        assert!(!foreign.contains("my_app"), "this workspace's own crate");
        assert!(
            !foreign.contains("some_other_projects_crate"),
            "a crate left behind by a near-match restore"
        );
    }

    #[test]
    fn a_workspace_crate_is_never_shared() {
        // Its hash is unique to this checkout, so sharing it buys nothing, and
        // excluding it means an entry can never stand in for edited code.
        let (local, foreign) = partition_packages(LOCK);
        assert!(local.contains("my_app"));
        assert!(!foreign.contains("my_app"));
        assert!(foreign.contains("serde"));
        assert!(!local.contains("serde"));
    }

    #[test]
    fn a_dashed_package_matches_the_name_cargo_writes() {
        // Cargo names artifacts `my_app-<hash>`, so a lock entry spelled with a
        // dash has to match that or the exclusion silently fails.
        let (local, _) = partition_packages(LOCK);
        assert!(local.contains("my-app"), "the lock file spelling");
        assert!(local.contains("my_app"), "the artifact spelling");
    }

    #[test]
    fn the_nightly_layout_yields_one_entry_per_hash() {
        let profile = scratch("units-new");
        for (pkg, hash) in [("serde", "aaaa"), ("serde", "bbbb"), ("quote", "cccc")] {
            fs::create_dir_all(profile.join("build").join(pkg).join(hash)).unwrap();
        }

        let mut keys: Vec<String> = fragments(&profile).into_iter().map(|f| f.key).collect();
        keys.sort();
        assert_eq!(keys, ["quote-cccc", "serde-aaaa", "serde-bbbb"]);
        let _ = fs::remove_dir_all(&profile);
    }

    #[test]
    fn a_package_name_containing_a_dash_is_still_shared() {
        // Guessing the layout from directory names skipped every one of these,
        // which quietly left `proc-macro2` and `unicode-ident` out of the store.
        let profile = scratch("units-dash");
        fs::create_dir_all(profile.join("build").join("proc-macro2").join("aaaa")).unwrap();

        let found = fragments(&profile);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "proc-macro2-aaaa");
        // And the crate name has to come back out of the key intact.
        assert_eq!(
            found[0].key.rsplit_once('-').map(|(c, _)| c),
            Some("proc-macro2")
        );
        let _ = fs::remove_dir_all(&profile);
    }

    #[test]
    fn the_layout_is_settled_by_the_fingerprint_directory() {
        // The two layouts both use `build`, so the older one has to win whenever
        // its fingerprint directory is present.
        let profile = scratch("units-both");
        fs::create_dir_all(profile.join(".fingerprint").join("serde-1b84")).unwrap();
        fs::create_dir_all(profile.join("build").join("serde").join("aaaa")).unwrap();

        let keys: Vec<String> = fragments(&profile).into_iter().map(|f| f.key).collect();
        assert_eq!(keys, ["serde-1b84"]);
        let _ = fs::remove_dir_all(&profile);
    }

    #[test]
    fn the_stable_layout_collects_all_three_places() {
        // A unit whose build-script output is left behind makes every dependent
        // dirty again, which measured as 2.20s against 0.06s.
        let profile = scratch("units-old");
        fs::create_dir_all(profile.join(".fingerprint").join("serde_core-1b84")).unwrap();
        fs::create_dir_all(profile.join("build").join("serde_core-1b84")).unwrap();
        fs::create_dir_all(profile.join("deps")).unwrap();
        for file in [
            "libserde_core-1b84.rmeta",
            "serde_core-1b84.d",
            "libother-9999.rmeta",
        ] {
            fs::write(profile.join("deps").join(file), b"x").unwrap();
        }

        let found = fragments(&profile);
        assert_eq!(found.len(), 1);
        let mut parts: Vec<String> = found[0]
            .parts
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        parts.sort();
        assert_eq!(
            parts,
            [
                ".fingerprint/serde_core-1b84",
                "build/serde_core-1b84",
                "deps/libserde_core-1b84.rmeta",
                "deps/serde_core-1b84.d",
            ],
            "another unit's artifacts must not be dragged in"
        );
        let _ = fs::remove_dir_all(&profile);
    }

    #[test]
    fn a_directory_cargo_has_built_in_is_left_alone() {
        // Every offered entry would lose to what is already there, so the walk is
        // pure cost -- and it was being paid on every rebuild after an edit.
        let profile = scratch("units-populated");
        assert!(!populated(&profile), "nothing there at all");

        fs::create_dir_all(profile.join("build")).unwrap();
        assert!(
            !populated(&profile),
            "an empty one is left by an interrupted build and is worth filling"
        );

        fs::create_dir_all(profile.join("build").join("serde").join("aaaa")).unwrap();
        assert!(populated(&profile));
        let _ = fs::remove_dir_all(&profile);
    }

    #[test]
    fn a_seed_reaches_below_a_directory_that_already_exists() {
        // The store holds one entry per unit, so several entries share a package
        // directory. Stopping at the top level meant only the first of them landed.
        let root = scratch("units-deep");
        let profile = root.join("profile");
        fs::create_dir_all(profile.join("build").join("serde").join("aaaa")).unwrap();

        let entry = root.join("entry");
        fs::create_dir_all(entry.join("build").join("serde").join("bbbb")).unwrap();
        fs::write(
            entry.join("build").join("serde").join("bbbb").join("out"),
            b"x",
        )
        .unwrap();

        merge_into(&entry, &profile).unwrap();

        assert!(profile.join("build").join("serde").join("aaaa").exists());
        assert!(
            profile
                .join("build")
                .join("serde")
                .join("bbbb")
                .join("out")
                .exists(),
            "a second variant of an already-present package must still arrive"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_seed_never_overwrites_what_the_build_already_has() {
        // Cargo's own copy is always the better one, and clobbering it could
        // replace a just-compiled artifact with an older stored one.
        let root = scratch("units-merge");
        let entry = root.join("entry");
        let profile = root.join("profile");
        fs::create_dir_all(entry.join("deps")).unwrap();
        fs::write(entry.join("deps").join("libx-1.rmeta"), b"stored").unwrap();
        fs::write(entry.join("deps").join("libnew-2.rmeta"), b"stored").unwrap();
        fs::create_dir_all(profile.join("deps")).unwrap();
        fs::write(profile.join("deps").join("libx-1.rmeta"), b"mine").unwrap();

        merge_into(&entry, &profile).unwrap();

        let kept = fs::read_to_string(profile.join("deps").join("libx-1.rmeta")).unwrap();
        assert_eq!(kept, "mine", "an existing artifact must survive a seed");
        assert!(profile.join("deps").join("libnew-2.rmeta").exists());
        let _ = fs::remove_dir_all(&root);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("turbo-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }
}
