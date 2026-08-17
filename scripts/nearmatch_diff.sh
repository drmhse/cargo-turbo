#!/bin/bash
# Differential: a sequence of dependency-version changes, each built twice --
# once by plain cargo in a pristine directory, once by cargo-turbo restoring a
# near match. The program's own output must agree at every step, or the
# near-match restore is serving stale artifacts.
set -u
# Absolute, because each step runs cargo from inside a probe directory.
ROOT=$(cd "$(dirname "$0")/.." && pwd)
TURBO=${TURBO:-$ROOT/target/release/cargo-turbo}
if [ ! -x "$TURBO" ]; then
  echo "build it first: cargo build --release" >&2
  exit 2
fi
export RUSTUP_TOOLCHAIN=nightly
export CARGO_TURBO_DIR=${TMPDIR:-/tmp}/nearmatch-store
S=${TMPDIR:-/tmp}/nearmatch
rm -rf "$CARGO_TURBO_DIR" "$S"
mkdir -p "$S"

# Versions of `semver` whose behaviour is observable at runtime.
VERSIONS=(1.0.20 1.0.22 1.0.20 1.0.25 1.0.22 1.0.26 1.0.20 1.0.26)

setup() {
  local dir=$1 ver=$2
  mkdir -p "$dir/src"
  cat > "$dir/Cargo.toml" <<EOF
[package]
name = "diffprobe"
version = "0.1.0"
edition = "2021"

[dependencies]
semver = "=$ver"
EOF
  cat > "$dir/src/main.rs" <<'EOF'
fn main() {
    // Printed so a stale artifact from another version is visible as a
    // difference in output rather than only as a silent reuse.
    let v = semver::Version::parse("1.2.3-rc.1+build.5").unwrap();
    let r = semver::VersionReq::parse(">=1.0, <2.0").unwrap();
    println!("{v} {} {} {}", v.pre, r.matches(&v), semver::Version::parse("0.0.0").is_ok());
}
EOF
}

fails=0
for i in "${!VERSIONS[@]}"; do
  ver=${VERSIONS[$i]}

  # Reference: a pristine directory, plain cargo, no acceleration whatsoever.
  rm -rf "$S/diff-a"; setup "$S/diff-a" "$ver"
  ref=$( (cd "$S/diff-a" && cargo run -q 2>&1) )
  ref_status=$?

  # Under test: the same source, but the target directory is whatever the
  # near-match restore hands cargo.
  setup "$S/diff-b" "$ver"
  rm -rf "$S/diff-b/target"
  # The key is derived from the lock file, so it has to exist before the plan is
  # resolved -- otherwise cargo-turbo forwards to cargo unchanged and the test
  # silently measures nothing.
  (cd "$S/diff-b" && cargo generate-lockfile -q)
  raw=$( (cd "$S/diff-b" && "$TURBO" run -q 2>&1) )
  status=$?
  out=$(printf '%s\n' "$raw" | grep -v '^cargo-turbo:')
  note=$(printf '%s\n' "$raw" | grep -o 'restored[^,]*' | head -1)
  [ -z "$note" ] && note="cold"

  if [ "$ref" = "$out" ] && [ "$ref_status" = "$status" ]; then
    echo "step $((i+1)) semver=$ver  agree  [$note]"
  else
    echo "step $((i+1)) semver=$ver  MISMATCH"
    echo "  plain cargo: $ref"
    echo "  cargo turbo: $out"
    fails=$((fails+1))
  fi
done
echo
[ "$fails" = 0 ] && echo "all ${#VERSIONS[@]} steps agree" || echo "$fails of ${#VERSIONS[@]} steps disagree"
exit $fails
