#!/bin/bash
# Differential: several projects with overlapping dependencies, each built twice
# -- once by plain cargo in a pristine directory, once by cargo-turbo against a
# store the other projects have filled. Every program's output must agree, or a
# shared unit is standing in for one that differs.
#
# The feature sets are deliberately varied, because that is what a shared store
# can most plausibly get wrong: serving a unit built with `derive` to a project
# that asked for a build without it.
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TURBO=${TURBO:-$ROOT/target/release/cargo-turbo}
if [ ! -x "$TURBO" ]; then
  echo "build it first: cargo build --release" >&2
  exit 2
fi
# Extra arguments for both sides, so the same comparison can be run for a
# cross-compiled build, where cargo writes into two directories rather than one.
EXTRA=("$@")
S=${TMPDIR:-/tmp}/unitstore${EXTRA[*]:+-cross}
export CARGO_TURBO_DIR=$S/store
rm -rf "$S"
mkdir -p "$S"

# name : the serde dependency spec each project asks for
NAMES=(alpha beta gamma delta epsilon)
SPECS=(
  'features = ["derive"]'
  'features = ["derive"]'
  'default-features = false'
  'features = ["derive", "rc"]'
  'features = ["derive"]'
)

setup() {
  local dir=$1 name=$2 spec=$3
  mkdir -p "$dir/src"
  cat > "$dir/Cargo.toml" <<EOF
[package]
name = "$name"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1.0.200", $spec }
EOF
  # Printed rather than merely compiled, so a unit built for a different feature
  # set shows up as a difference in output and not only as a silent reuse.
  cat > "$dir/src/main.rs" <<EOF
fn main() {
    let derive = cfg!(feature = "unused");
    println!("$name {} {} {}", derive, size_of::<usize>(), env!("CARGO_PKG_NAME"));
}
EOF
}

fails=0
# bash arrays are zero-indexed.
for i in $(seq 0 $((${#NAMES[@]} - 1))); do
  name=${NAMES[$i]}
  spec=${SPECS[$i]}

  # Reference: a pristine directory, plain cargo, no acceleration whatsoever and
  # no store to draw on.
  rm -rf "$S/ref"
  setup "$S/ref" "$name" "$spec"
  ref=$( (cd "$S/ref" && cargo run -q "${EXTRA[@]+${EXTRA[@]}}" 2>&1) )
  ref_status=$?

  # Under test: the same project, built with whatever the store already holds
  # from the projects before it. Each one gets a directory of its own, because a
  # shared path would make them all one lineage and the snapshot would answer
  # instead of the unit store -- which is how the store came to be filled with
  # crates belonging to other projects.
  test=$S/test-$name
  rm -rf "$test"
  setup "$test" "$name" "$spec"
  (cd "$test" && cargo generate-lockfile -q)
  raw=$( (cd "$test" && "$TURBO" run -q "${EXTRA[@]+${EXTRA[@]}}" 2>&1) )
  status=$?
  out=$(printf '%s\n' "$raw" | grep -v '^cargo-turbo:')
  note=$(printf '%s\n' "$raw" | grep -o 'supplied [0-9]* prebuilt units')
  [ -z "$note" ] && note="nothing to reuse"

  if [ "$ref" = "$out" ] && [ "$ref_status" = "$status" ]; then
    echo "$name ($spec)  agree  [$note]"
  else
    echo "$name ($spec)  MISMATCH"
    echo "  plain cargo: $ref"
    echo "  cargo turbo: $out"
    fails=$((fails + 1))
  fi
done

echo
[ "$fails" = 0 ] && echo "all ${#NAMES[@]} projects agree" || echo "$fails of ${#NAMES[@]} projects disagree"
exit $fails
