#!/usr/bin/env bash
#
# Which dependencies run code at build time, held to a reviewed list.
#
# `--locked` and a pinned toolchain stop an unreviewed *version* bump. Neither does anything about a
# `build.rs` appearing inside a dependency tree that was already approved, and that is the live
# vector: a campaign is stealing Sui keystores from developer and CI machines through build scripts
# that run during `cargo build`. A build script runs arbitrary code with the privileges of whoever
# typed the build command, before any test of ours executes, so by the time anything could notice,
# the key has already left the machine. `cargo test` does not help. Review is the only control, and
# review needs a diff.
#
# So the set of build-script-bearing packages is checked in, and any change to it fails the build.
# The list is an audit surface, not a promise: it says these build scripts were looked at once, and
# a line appearing in it is the moment to look at one.
#
#   scripts/audit-build-scripts.sh check    what CI runs; exits 1 on any difference
#   scripts/audit-build-scripts.sh update   rewrite the baseline, after reading the new build script
#
# `cargo metadata` is asked for every platform and every feature, deliberately: a build script that
# only runs on Windows is still a build script in the tree, and the release builds three targets.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
baseline="$root/scripts/build-scripts.baseline"

header() {
  cat <<'EOF'
# Dependencies that run code at build time, reviewed and pinned.
#
# One line per package, `name version`, sorted. `scripts/audit-build-scripts.sh check` fails when
# this list and the tree disagree; `scripts/audit-build-scripts.sh update` rewrites it, and belongs
# in the same commit as the review that accepted the change.
EOF
}

# The captured document, when a test hands us one. A nested `cargo metadata` can block on the
# package-cache lock the outer cargo holds, and a test that hangs is worse than no test, so the
# checking logic is exercised through this seam rather than by running cargo inside cargo.
metadata() {
  if [ -n "${RILL_CARGO_METADATA:-}" ]; then
    cat "$RILL_CARGO_METADATA"
  else
    cargo metadata --format-version 1 --locked --manifest-path "$root/Cargo.toml"
  fi
}

# A package carries a build script when one of its targets is of kind `custom-build`. That is the
# same fact cargo acts on, rather than a guess from whether a `build.rs` file is present.
current() {
  metadata \
    | jq -r '.packages[] | select(any(.targets[]; .kind[] == "custom-build")) | "\(.name) \(.version)"' \
    | LC_ALL=C sort -u
}

reviewed() {
  grep -v -e '^#' -e '^[[:space:]]*$' "$baseline" || true
}

case "${1:-check}" in
check)
  if ! command -v jq > /dev/null; then
    echo "::error::jq is needed to read cargo metadata. brew install jq, or apt-get install jq."
    exit 1
  fi
  if diff=$(diff -u --label reviewed --label tree <(reviewed) <(current)); then
    printf '%s build-script dependencies, every one of them on the reviewed list.\n' \
      "$(reviewed | wc -l | tr -d ' ')"
    exit 0
  fi
  printf '%s\n' "$diff"
  echo "::error::The set of dependencies that run code at build time changed. Review each new build script, then update scripts/build-scripts.baseline in the same commit."
  cat <<'EOF'

A line marked + is a package that now runs code during `cargo build`. Find out why it is here and
read what it does before accepting it:

  cargo tree --invert --locked --package <name>   who pulled it in
  cargo metadata --format-version 1 --locked \
    | jq -r '.packages[] | select(.name == "<name>") | .manifest_path'
  # then read the build.rs beside that manifest, in full

A line marked - is a build script that left the tree. That is good news, and still a baseline edit.
Once the change is understood and accepted:

  scripts/audit-build-scripts.sh update        # commit the baseline with the change, not after it
EOF
  exit 1
  ;;
update)
  { header; current; } > "$baseline"
  printf 'wrote %s: %s entries\n' "$baseline" "$(reviewed | wc -l | tr -d ' ')"
  ;;
*)
  echo "usage: $(basename "$0") [check|update]" >&2
  exit 2
  ;;
esac
