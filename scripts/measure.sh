#!/bin/bash
# Measures the three situations cargo-turbo is meant to help with, against plain
# cargo, and prints medians of three.
#
#   cold            an empty store and an empty target directory: nothing to reuse
#   warm            the same checkout again, target directory wiped
#   fresh checkout  a second checkout at another path, so no snapshot applies but
#                   the dependencies are already in the shared store
#
# Usage: scripts/measure.sh <git-url> <command…>
#        scripts/measure.sh https://github.com/tokio-rs/tokio check --workspace
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TURBO=${TURBO:-$ROOT/target/release/cargo-turbo}
if [ ! -x "$TURBO" ]; then
  echo "build it first: cargo build --release" >&2
  exit 2
fi

url=${1:?a git url}
shift
cmd=("$@")
[ ${#cmd[@]} -eq 0 ] && cmd=(check --workspace)

work=${TMPDIR:-/tmp}/turbo-measure
name=$(basename "$url")
export CARGO_TURBO_DIR=$work/store
a=$work/$name-a

rm -rf "$work"
mkdir -p "$work"
git clone -q --depth 1 "$url" "$a" || exit 1
# Three byte-for-byte second checkouts. One would not do: after its first build it
# has a snapshot of its own, and every later run would be an exact hit rather than
# the first-build-of-a-new-checkout this is measuring.
for i in 1 2 3; do
  cp -Rc "$a" "$work/$name-b$i" 2>/dev/null || cp -R "$a" "$work/$name-b$i"
done

median() { printf '%s\n' "$@" | sort -n | sed -n 2p; }
timeit() { { /usr/bin/time -p sh -c "$1 > $work/out.txt 2>&1"; } 2>&1 | awk '/^real/{print $2}'; }

# Three runs in one directory, target wiped before each. The store is left alone,
# so this measures a repeat build.
warm_three() {
  local dir=$1 line=$2 times=()
  local i
  for i in 1 2 3; do
    rm -rf "$dir/target"
    times+=("$(cd "$dir" && timeit "$line")")
  done
  median "${times[@]}"
}

# Three runs with the store wiped as well, so each one really starts from nothing.
cold_three() {
  local dir=$1 line=$2 times=()
  local i
  for i in 1 2 3; do
    rm -rf "$dir/target" "$CARGO_TURBO_DIR"
    times+=("$(cd "$dir" && timeit "$line")")
  done
  median "${times[@]}"
}

# One run in each of the three second checkouts, against a store warmed by the
# first. Each is a first build of a directory the store has never seen.
fresh_three() {
  local line=$1 times=()
  local i
  for i in 1 2 3; do
    rm -rf "$work/$name-b$i/target"
    times+=("$(cd "$work/$name-b$i" && timeit "$line")")
  done
  median "${times[@]}"
}

echo "$name, ${cmd[*]}"
echo

plain_cold=$(cold_three "$a" "cargo ${cmd[*]}")
turbo_cold=$(cold_three "$a" "$TURBO ${cmd[*]}")

# The store is now warm for checkout A, so this is the repeat-build case.
turbo_warm=$(warm_three "$a" "$TURBO ${cmd[*]}")

# The second checkouts have never been built and have no snapshot of their own,
# but every dependency is in the shared store by now.
plain_fresh=$(fresh_three "cargo ${cmd[*]}")
turbo_fresh=$(fresh_three "$TURBO ${cmd[*]}")
units=$(grep -o 'supplied [0-9]* prebuilt units' "$work/out.txt" | head -1)
built=$(grep -cE '^ +(Checking|Compiling)' "$work/out.txt")

printf '%-22s %10s %10s\n' "" "cargo" "turbo"
printf '%-22s %10s %10s\n' "cold, empty store" "$plain_cold" "$turbo_cold"
printf '%-22s %10s %10s\n' "warm, target wiped" "$plain_cold" "$turbo_warm"
printf '%-22s %10s %10s\n' "fresh checkout" "$plain_fresh" "$turbo_fresh"
echo
echo "on the fresh checkout: ${units:-nothing reused}, $built units compiled"
