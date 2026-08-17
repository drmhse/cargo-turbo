# cargo-turbo

Makes cold Rust builds fast by reusing work already done and using the cores the
dependency graph leaves idle. It patches neither cargo nor rustc.

```
cargo install cargo-turbo
cargo turbo check --workspace
```

Measured on a ten-core Apple M4, nightly toolchain, `check --workspace`, medians
of three runs each. Reproduce with `scripts/measure.sh <git-url> check --workspace`.

| | rust-analyzer | tokio | ripgrep |
|---|---|---|---|
| `cargo`, nothing to reuse | 20.98s | 3.44s | 2.72s |
| `cargo turbo`, nothing to reuse | 16.00s (1.31x) | 2.57s (1.34x) | 3.33s (0.82x) |
| `cargo turbo`, same checkout again | **0.25s (84x)** | **0.10s (34x)** | **0.09s (30x)** |
| `cargo turbo`, a checkout it has never seen | **8.88s (2.6x)** | **1.31s (3.6x)** | **0.63s (4.3x)** |

Three different things are being reused, and the last row is the one most builds
actually meet:

- **The same checkout again** restores the target directory this build produced
  last time. Nothing is compiled.
- **A checkout it has never seen** is a fresh clone, a second worktree, or a CI
  runner: no snapshot applies, but the dependencies were built here by some other
  project. Only the workspace's own crates are compiled, 38 of rust-analyzer's 288
  units and 7 of tokio's 43.
- **Rebuilding in place** after an edit is left alone: the target directory already
  holds what cargo needs, so nothing is restored and nothing is offered. Measured on
  rust-analyzer, a no-op `check --workspace` costs 0.14s against cargo's 0.10s, and
  a one-crate edit 3.66s against 3.55s.
- **Nothing to reuse** is a machine where cargo-turbo has never run. All that is
  left is handing rustc the cores the dependency graph leaves idle, which depends
  on the shape of that graph: rust-analyzer and tokio have a long chain of crates
  that compile one at a time, and ripgrep does not, so there is no idle machine to
  give away and the wrapper's overhead shows instead.

Stable gets the same treatment. Nothing above needs an unstable flag:

| tokio, stable | `cargo` | `cargo turbo` |
|---|---|---|
| same checkout again | 3.51s | **0.10s (35x)** |
| a checkout it has never seen | 8.34s | **2.04s (4.1x)** |

A changed `Cargo.lock` used to fall all the way back to a cold build, because the
key it is derived from no longer matched anything. The nearest snapshot of the
same workspace is restored instead, and cargo rebuilds the difference:

| what changed in the lock file | nearest snapshot restored | no snapshot |
|---|---|---|
| a leaf dependency added | **0.86s (2.9x)** | 2.50s |
| a dependency several levels down bumped | **1.84s (1.4x)** | 2.65s |

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

**Shares built dependencies between projects.** Of rust-analyzer's 288 units, 250
are third-party and account for 77% of the CPU, and every artifact cargo writes is
named `<crate>-<hash>` where the hash covers the package, its features, the
profile, the compiler and the resolved dependencies. Two unrelated projects
depending on the same version of a crate with the same features therefore produce
the *same* names, and one project given the other's directories treats them as its
own. The hash is cargo's own answer to "would this have to be rebuilt", so a
different feature set or profile gets a different name and is never reused.

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
  program output (`scripts/nearmatch_diff.sh`)
- five projects with overlapping dependencies and deliberately varied feature sets,
  each built both by plain cargo in a pristine directory and by `cargo turbo`
  against a store the others filled, produce identical program output
  (`scripts/unitstore_diff.sh`), and again when cross-compiled, where cargo writes
  a host directory and a target directory rather than one

A key that is too coarse costs a rebuild, never a wrong answer, because cargo has
the final say.

## Commands

```
cargo turbo <cargo-command> [args…]   run a cargo command, accelerated
cargo turbo status                    what is stored
cargo turbo clean                     remove every snapshot and unit
```

### When cargo is invoked by something else

Release pipelines usually call cargo from a script or another build tool, and
rewriting that to go through `cargo turbo` is often not worth it. The two halves
are available as their own steps instead:

```
cargo turbo prepare build --release --target x86_64-unknown-linux-musl
./whatever-already-runs-the-build
cargo turbo store build --release --target x86_64-unknown-linux-musl
```

`prepare` fills the target directory with prebuilt dependencies and stops; `store`
adds what the build produced. Pass the arguments the build will use, since they say
which profile and target it is for and so where the units belong. Both are
optimisations, so neither fails a pipeline: if anything is wrong they say so and
exit successfully, and the build that follows is correct, only slower.

Measured with plain cargo doing the build, a second project cross-compiling to a
target the store already holds: 2.89s to **0.35s**, one crate compiled.

## Environment

| variable | effect |
|---|---|
| `CARGO_TURBO_DIR` | where snapshots live, default `~/.cache/cargo-turbo` |
| `CARGO_TURBO_JOBS` | cores to divide between invocations, default all |
| `CARGO_TURBO_THREADS=0` | leave rustc single-threaded |
| `CARGO_TURBO_NEAR=0` | require an exact key, never restore a near match |
| `CARGO_TURBO_FRESHNESS=checksum` | judge freshness by content rather than timestamps |
| `CARGO_TURBO_OFF=1` | forward to cargo unchanged |

`CARGO_TURBO_FRESHNESS=checksum` is worth knowing about. It suits one case, a
snapshot restored into a checkout whose files all have new timestamps, such as a
cache unpacked over a fresh clone: tokio measured 0.11s that way against 0.97s
under timestamps. It costs the sharing between projects, because a unit recorded
under one mode is rejected under the other, so the two stores are kept apart and
the timestamp one is the default. The choice is recorded with each snapshot, so a
project keeps whichever mode its first build used.

## Requirements

It works on stable, and better on nightly.

One unstable flag is used when it is available: `-Zthreads`, which is how rustc is
asked to use more than one core. Everything else needs nothing unstable, so both
the snapshot and the shared dependencies work on stable exactly as they do on
nightly. What stable gives up is the thread allocation, so a build with nothing to
reuse matches plain cargo.

`-Zchecksum-freshness` is used only when `CARGO_TURBO_FRESHNESS=checksum` is asked
for, and is unavailable on stable.

A filesystem with copy-on-write clones (APFS, Btrfs, XFS) keeps snapshots free.
Elsewhere they fall back to real copies and cost their size.

## What it does not do

Code that has never been built anywhere on the machine costs what it always did.
A workspace's own crates are never shared between checkouts either, so the floor
for a fresh clone is however long its own crates take: 8.88s of rust-analyzer's
23.22s is exactly that. The wins are in CI, fresh clones, second worktrees, wiped
target directories, and switching between branches.

Snapshots are local to the machine, because a build script can read a system
library or an environment variable that cargo never fingerprints, and its result
is only reliably reusable where those are the same.
