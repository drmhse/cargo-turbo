# cargo-turbo

Makes cold Rust builds fast by reusing work already done and using the cores the
dependency graph leaves idle. It patches neither cargo nor rustc.

```
cargo install cargo-turbo
cargo turbo check --workspace
```

Measured on a ten-core Apple M4, checking rust-analyzer (309 units), nightly
toolchain:

Medians of three runs each, nightly toolchain:

| project | cold, `cargo` | cold, `cargo turbo` | warm, target wiped |
|---|---|---|---|
| rust-analyzer, 309 units | 22.85s | 16.77s (1.36x) | **0.36s (64x)** |
| tokio | 3.66s | 2.81s (1.30x) | 0.11s (33x) |
| ripgrep | 3.13s | 3.19s (0.98x) | 0.09s (33x) |

The warm column is the reliable win, and it is where the tool earns its place. The
cold column depends on the shape of the dependency graph: rust-analyzer and tokio
have a long chain of crates that compile one at a time, and ripgrep does not, so
there is no idle machine to hand to rustc.

A changed `Cargo.lock` used to fall all the way back to a cold build, because the
key it is derived from no longer matched anything. The nearest snapshot of the
same workspace is now restored instead, and cargo rebuilds the difference. Tokio,
target directory wiped, medians of three:

| what changed in the lock file | nearest snapshot restored | no snapshot |
|---|---|---|
| a leaf dependency added | **0.86s (2.9x)** | 2.50s |
| a dependency several levels down bumped | **1.84s (1.4x)** | 2.65s |

How much this buys depends on how much of the graph the change reaches: a new leaf
leaves every existing unit valid, while bumping something deep invalidates
everything above it.

## What it does

**Gives each rustc invocation a share of the machine.** A cold build is
latency-bound rather than throughput-bound: that rust-analyzer check uses 2.34 of
ten cores, because dependencies fan out and saturate the machine for the first
17.5 seconds and then concurrency falls to 1.46 while the workspace crates
compile in a chain. rustc's frontend can use several threads, and during that
tail every core but one is idle. Each invocation registers itself and counts how
many others are running, so the share reflects how wide the build actually is.

**Restores the nearest snapshot when there is no exact one.** Snapshots of one
workspace, profile and command form a lineage, and the most recent member of the
lineage is used when the exact key is absent. It is only ever a starting point:
cargo's freshness pass decides what of it survives, so this can cost a rebuild and
cannot produce a wrong answer.

**Records the target directory and puts it back.** Of those 309 units, most are
third-party and immutable: a given version of a crate, built with the same
features, profile and compiler, produces the same bytes every time. The snapshot
is keyed on the resolved dependency set, the compiler and cargo versions, the
build flags, and the environment variables cargo folds into its own fingerprints.

Snapshots cost almost no disk. Both APFS and Btrfs can copy a file by sharing its
blocks until one copy is written, so a store reporting 2.6 GB measured 2 MB of
actual consumption.

## Why it is safe

Cargo still decides what is stale. The snapshot only pre-populates a directory,
and cargo's own freshness pass then runs unchanged, so an edited file is always
rebuilt and an error is always reported. Verified on every release:

- restoring then editing a source rebuilds that crate and its dependents
- restoring then introducing a type error reports the error
- restoring into a checkout where every file timestamp is new stays fast and correct
- eight dependency-version changes in sequence, each built both by plain cargo in a
  pristine directory and by `cargo turbo` on top of a near match, produce identical
  program output

A key that is too coarse costs a rebuild, never a wrong answer, because cargo has
the final say.

## Commands

```
cargo turbo <cargo-command> [args…]   run a cargo command, accelerated
cargo turbo status                    what is stored
cargo turbo clean                     remove every snapshot
```

## Environment

| variable | effect |
|---|---|
| `CARGO_TURBO_DIR` | where snapshots live, default `~/.cache/cargo-turbo` |
| `CARGO_TURBO_JOBS` | cores to divide between invocations, default all |
| `CARGO_TURBO_THREADS=0` | leave rustc single-threaded |
| `CARGO_TURBO_NEAR=0` | require an exact key, never restore a near match |
| `CARGO_TURBO_OFF=1` | forward to cargo unchanged |

## Requirements

It works on stable, and better on nightly.

Two unstable flags are used when they are available. `-Zthreads` is how rustc is
asked to use more than one core, and `-Zchecksum-freshness` makes cargo compare
file contents instead of timestamps.

On **stable** the snapshot still restores, because a mtime-preserving clone needs
no unstable flag. What stable gives up is the thread allocation, so a first build
matches plain cargo, and timestamp independence, so a checkout with new
timestamps rebuilds the crates whose sources appear newer. Dependencies keep their
timestamps in the registry and stay fresh, which is most of the work.

Measured on rust-analyzer, stable 1.97.1:

| scenario | stable | nightly |
|---|---|---|
| first build | 23.80s | 16.38s |
| target directory wiped, sources untouched | **0.57s** | 0.77s |
| every source timestamp changed | 6.73s, 51 of 309 units rebuilt | 2.11s, 4 units rebuilt |

The four that rebuild on nightly are build scripts and their dependents, because
cargo still compares timestamps for the paths a build script declares even under
checksum freshness.

A filesystem with copy-on-write clones (APFS, Btrfs, XFS) keeps snapshots free.
Elsewhere they fall back to real copies and cost their size.

## What it does not do

The first build of code never built before costs what it always did. The wins are
in CI, fresh clones, wiped target directories, and switching between branches.

Snapshots are local to the machine, because a build script can read a system
library or an environment variable that cargo never fingerprints, and its result
is only reliably reusable where those are the same.
