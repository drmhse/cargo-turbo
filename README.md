# cargo-turbo

Makes cold Rust builds fast by reusing work already done and using the cores the
dependency graph leaves idle. It patches neither cargo nor rustc.

```
cargo install cargo-turbo
cargo turbo check --workspace
```

Measured on a ten-core Apple M4, checking rust-analyzer (309 units) from an empty
target directory:

| | seconds |
|---|---|
| `cargo check --workspace` | 27.51 |
| `cargo turbo check --workspace`, first run | 21.91 |
| `cargo turbo check --workspace`, target directory wiped | **0.90** |
| the same, into a fresh clone with new file timestamps | 2.11 |

## What it does

**Gives each rustc invocation a share of the machine.** A cold build is
latency-bound rather than throughput-bound: that rust-analyzer check uses 2.34 of
ten cores, because dependencies fan out and saturate the machine for the first
17.5 seconds and then concurrency falls to 1.46 while the workspace crates
compile in a chain. rustc's frontend can use several threads, and during that
tail every core but one is idle. Each invocation registers itself and counts how
many others are running, so the share reflects how wide the build actually is.

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
| `CARGO_TURBO_OFF=1` | forward to cargo unchanged |

## Requirements

A nightly toolchain, for two unstable flags: `-Zthreads`, which is how rustc is
asked to use more than one core, and `-Zchecksum-freshness`, which makes cargo
compare file contents instead of timestamps. Without the second, restoring into a
fresh clone would rebuild everything, since a clone gives every source a newer
timestamp than the outputs built from it. On a stable toolchain the tool forwards
to cargo unchanged rather than doing something unsound.

A filesystem with copy-on-write clones (APFS, Btrfs, XFS) keeps snapshots free.
Elsewhere they fall back to real copies and cost their size.

## What it does not do

The first build of code never built before costs what it always did. The wins are
in CI, fresh clones, wiped target directories, and switching between branches.

Snapshots are local to the machine, because a build script can read a system
library or an environment variable that cargo never fingerprints, and its result
is only reliably reusable where those are the same.
