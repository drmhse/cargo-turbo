# cargo-turbo

[![crates.io](https://img.shields.io/crates/v/cargo-turbo.svg)](https://crates.io/crates/cargo-turbo)
[![license](https://img.shields.io/crates/l/cargo-turbo.svg)](#license)

A drop-in `cargo` wrapper that makes **cold** builds fast, by reusing work the
machine has already done and handing rustc the cores the dependency graph leaves
idle. It patches neither cargo nor rustc, and cargo still decides what is stale.

```sh
cargo install cargo-turbo
cargo turbo check --workspace          # anywhere you'd type `cargo check --workspace`
```

## What you get

Four situations, and cargo-turbo treats each differently. This is the whole
mental model:

| situation | what happens |
|---|---|
| **Warm rebuild** — you edited a file and built again | nothing. The target directory already holds what cargo needs, so cargo-turbo stays out of the way |
| **Same checkout, cold** — target directory wiped, branch switched back | the snapshot of that exact build is restored. Nothing is compiled |
| **New checkout** — fresh clone, second worktree, CI runner | no snapshot applies, but the dependencies were built here by *some* project. Only your own crates compile |
| **Cold machine** — cargo-turbo has never run here | there is nothing to reuse, so all that is left is the extra cores |

Each cell below is `cargo turbo` against plain `cargo` **for that same
situation** — the baselines differ per row, so the multipliers are not
comparable across rows.

| | rust-analyzer | tokio | ripgrep |
|---|---|---|---|
| Same checkout, cold | 0.25s vs 21.74s (**87x**) | 0.10s vs 3.40s (**34x**) | 0.08s vs 3.27s (**41x**) |
| New checkout | 10.05s vs 25.02s (**2.5x**) | 1.33s vs 5.79s (**4.4x**) | 0.65s vs 3.11s (**4.8x**) |
| Cold machine | 15.85s vs 21.74s (**1.37x**) | 2.90s vs 3.40s (**1.17x**) | 3.00s vs 3.27s (**1.09x**) |
| *your crates, of total units* | *38 of 307* | *7 of 50* | *11 of 70* |

Ten-core Apple M4, `rustc 1.100.0-nightly`, `check --workspace`, medians of
three, 2026-08-18. Reproduce with `scripts/measure.sh <git-url> check --workspace`.

**New checkout is the row most builds actually meet.** It is CI, fresh clones,
second worktrees, and wiped target directories — and it is the row that holds up
across all three projects. Only your own crates compile; everything else is
handed over prebuilt.

**Cold machine is the weakest row, and it depends on the shape of the graph.**
The only lever left there is parallelism, and how much of it there is to reclaim
is decided by how long the build spends narrow. rust-analyzer ends in a long
chain of workspace crates compiling one at a time, so there are idle cores to
hand out and it gains the most. tokio and ripgrep stay wide almost to the end,
where every core is already busy and there is nothing to give away, so they gain
little. Treat this row as "no worse than cargo", not as a reason to adopt.

Stable gets the first two rows in full — they need nothing unstable:

| tokio, stable toolchain | `cargo` | `cargo turbo` |
|---|---|---|
| Same checkout, cold | 3.53s | **0.09s (39x)** |
| New checkout | 7.31s | **2.07s (3.5x)** |
| Cold machine | 3.53s | 3.72s (0.95x) |

The cold-machine row is what stable gives up. Without `-Zthreads` there is no
thread allocation to make, so nothing is added to the build and the small loss is
what it costs to record the result for next time.

> Figures in the rest of this document come from earlier releases and have not
> been re-measured on the toolchain above.

## How it works

Three independent mechanisms.

### 1. Snapshots of the target directory

After a build, the target directory is recorded and keyed on everything that
could invalidate it: the resolved dependency set, the compiler and cargo
versions, the build flags, and the environment variables cargo folds into its own
fingerprints. The same key later means the directory is put back and nothing is
compiled.

Snapshots cost almost no disk at first. APFS and Btrfs can copy a file by
sharing its blocks until one copy is written, so a store reporting 2.6 GB
measured 2 MB of actual consumption. That holds only while the target directory
still has the same blocks, though; once it is rebuilt they belong to the
snapshot alone. A lineage gains a snapshot for every distinct `Cargo.lock` a
workspace is built with, so the five most recent are kept and the rest removed
as each new one lands (`CARGO_TURBO_KEEP`).

**When the key does not match exactly**, the nearest snapshot is restored rather
than falling back to a cold build. Snapshots of one workspace, profile and
command form a lineage, and the most recent member is used when the exact key is
absent. It is only ever a starting point — cargo's freshness pass decides what
survives. A changed `Cargo.lock` is the common case:

| what changed in the lock file | nearest snapshot | no snapshot |
|---|---|---|
| a leaf dependency added | **0.86s (2.9x)** | 2.50s |
| a dependency several levels down bumped | **1.84s (1.4x)** | 2.65s |

### 2. A shared store of third-party units

Most of what a build compiles is not yours: of rust-analyzer's 307 units, 269 are
third-party. Every artifact cargo writes is named `<crate>-<hash>`, where the hash
covers the package, its features, the profile, the compiler and the resolved
dependencies. Two unrelated projects depending on the same version of a crate with
the same features therefore produce the *same* names, and one project handed the
other's directories treats them as its own.

That hash is cargo's own answer to "would this have to be rebuilt", so a
different feature set or profile gets a different name and is never reused. This
is what makes a **new checkout** fast even though no snapshot applies.

### 3. Cores the dependency graph leaves idle

A cold build is latency-bound, not throughput-bound. Dependencies fan out and
saturate the machine early on, then concurrency collapses while the workspace's
own crates compile in a chain — and during that tail every core but one is idle.
rustc's frontend can use several threads, so there is idle machine to give away.

Each rustc invocation registers itself and counts how many others are running, so
its share reflects how wide the build actually is at that moment. This is the one
part that needs nightly (`-Zthreads`), and the one whose payoff varies most by
project.

## Why it cannot give a wrong answer

**Cargo still decides what is stale.** Restoring only pre-populates a directory;
cargo's own freshness pass then runs unchanged. An edited file is always rebuilt
and an error is always reported. A key that is too coarse therefore costs a
rebuild, never a wrong answer.

Verified on every release:

- restoring, then editing a source, rebuilds that crate and its dependents
- restoring, then introducing a type error, reports the error
- restoring into a checkout where every file timestamp is new stays fast and correct
- eight dependency-version changes in sequence, each built both by plain cargo in
  a pristine directory and by `cargo turbo` on top of a near match, produce
  identical program output (`scripts/nearmatch_diff.sh`)
- five projects with overlapping dependencies and deliberately varied feature
  sets, each built both by plain cargo in a pristine directory and by
  `cargo turbo` against a store the others filled, produce identical program
  output (`scripts/unitstore_diff.sh`) — and again when cross-compiled, where
  cargo writes a host directory and a target directory rather than one

## Commands

```sh
cargo turbo <cargo-command> [args…]   # run a cargo command, accelerated
cargo turbo status                    # what is stored, and how much space
cargo turbo clean                     # remove every snapshot and unit
```

### When cargo is invoked by something else

Release pipelines usually call cargo from a script or another build tool, and
rewriting that to go through `cargo turbo` is often not worth it. The two halves
are available as their own steps:

```sh
cargo turbo prepare build --release --target x86_64-unknown-linux-musl
./whatever-already-runs-the-build
cargo turbo store   build --release --target x86_64-unknown-linux-musl
```

`prepare` fills the target directory with prebuilt dependencies and stops;
`store` adds what the build produced. Pass the arguments the build will use —
they say which profile and target it is for, and so where the units belong.

Both are optimisations, so neither can fail a pipeline: if anything is wrong they
say so and exit successfully, and the build that follows is correct, only slower.

## Environment

| variable | effect |
|---|---|
| `CARGO_TURBO_DIR` | where snapshots live, default `~/.cache/cargo-turbo` |
| `CARGO_TURBO_JOBS` | cores to divide between invocations, default all |
| `CARGO_TURBO_THREADS=0` | leave rustc single-threaded |
| `CARGO_TURBO_NEAR=0` | require an exact key, never restore a near match |
| `CARGO_TURBO_KEEP` | snapshots kept per workspace and profile, default 5 |
| `CARGO_TURBO_FRESHNESS=checksum` | judge freshness by content rather than timestamps |
| `CARGO_TURBO_OFF=1` | forward to cargo unchanged |
| `CARGO_TURBO_TIME=1` | report how long each phase of this tool took |

`CARGO_TURBO_FRESHNESS=checksum` is the one worth knowing about. It suits a
snapshot restored into a checkout whose files all have new timestamps — a cache
unpacked over a fresh clone, say.

The cost is that it gives up sharing between projects: a unit recorded under one
mode is rejected under the other, so the two stores are kept apart. Timestamps
are the default, and the choice is recorded with each snapshot, so a project
keeps whichever mode its first build used.

## Requirements

Works on stable, better on nightly.

Exactly one unstable flag is used when available: `-Zthreads`, which is how rustc
is asked to use more than one core. Snapshots and the shared unit store need
nothing unstable and behave identically on stable — what stable gives up is
mechanism 3.

(`-Zchecksum-freshness` is used only if you ask for
`CARGO_TURBO_FRESHNESS=checksum`, and is unavailable on stable.)

A filesystem with copy-on-write clones (APFS, Btrfs, XFS) keeps snapshots free.
Elsewhere they fall back to real copies and cost their size.

## Limits

**Code that has never been built anywhere on the machine costs what it always
did.** There is no magic; there is only reuse.

**A workspace's own crates are never shared between checkouts.** So the floor for
a fresh clone is however long your own crates take — 10.05s of rust-analyzer's
25.02s is exactly that.

**Snapshots are local to the machine.** A build script can read a system library
or an environment variable that cargo never fingerprints, and its result is only
reliably reusable where those are the same.

## License

MIT — see [LICENSE-MIT](LICENSE-MIT).
