#!/usr/bin/env bash
# Builds the release binaries on Linux or macOS and packages them into dist/.
#
# Runs `cargo build --release` at the repository root, copies `pm` and `partition-manager`
# with README.md and LICENSE into dist/<os>-<arch>/, prints their sizes and `pm --version`,
# and writes dist/partition-manager-<version>-<os>-<arch>.tar.gz. dist/ is gitignored.
# The Windows counterpart is scripts/build-release.ps1.
#
# A checkout that carries a rust-toolchain.toml pinned to the Windows GNU channel needs
# RUSTUP_TOOLCHAIN=stable (the script sets it when the pin is a windows toolchain).
#
# PM_TARGET=<triple> cross-builds for another target of the same OS (for example
# PM_TARGET=x86_64-apple-darwin on an Apple Silicon Mac): the binaries come from
# target/<triple>/release and the archive is named after the triple's architecture.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) echo "build-release.sh: unsupported OS $(uname -s); on Windows use scripts/build-release.ps1" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) arch="$(uname -m)" ;;
esac

if [ -z "${RUSTUP_TOOLCHAIN:-}" ] && [ -f rust-toolchain.toml ] && grep -q 'windows' rust-toolchain.toml; then
  echo "rust-toolchain.toml pins a Windows toolchain; building with RUSTUP_TOOLCHAIN=stable"
  export RUSTUP_TOOLCHAIN=stable
fi

target_dir="${CARGO_TARGET_DIR:-$root/target}"
if [ -n "${PM_TARGET:-}" ]; then
  cargo build --release --target "$PM_TARGET"
  bin_dir="$target_dir/$PM_TARGET/release"
  case "$PM_TARGET" in
    x86_64-*) arch=x86_64 ;;
    aarch64-* | arm64-*) arch=aarch64 ;;
    *) arch="${PM_TARGET%%-*}" ;;
  esac
else
  cargo build --release
  bin_dir="$target_dir/release"
fi

# the workspace version (a cross-built pm may not run on this machine)
version="$(grep -m1 '^version = ' "$root/Cargo.toml" | cut -d'"' -f2)"
name="partition-manager-${version}-${os}-${arch}"
out="$root/dist/${os}-${arch}"
rm -rf "$out"
mkdir -p "$out"

for bin in pm partition-manager; do
  src="$bin_dir/$bin"
  [ -x "$src" ] || { echo "missing build output: $src" >&2; exit 1; }
  cp "$src" "$out/$bin"
done
for doc in README.md LICENSE CHANGELOG.md; do
  [ -f "$root/$doc" ] && cp "$root/$doc" "$out/"
done

echo
echo "dist/${os}-${arch}/:"
for f in "$out"/*; do
  printf '%-24s %12s bytes\n' "$(basename "$f")" "$(wc -c < "$f" | tr -d ' ')"
done

echo
"$out/pm" --version 2>/dev/null || echo "pm $version (cross-built for $PM_TARGET; not run here)"

tarball="$root/dist/${name}.tar.gz"
tar -czf "$tarball" -C "$root/dist" "${os}-${arch}"
echo
echo "wrote dist/${name}.tar.gz"
echo "Run with sudo for physical disks: sudo $out/pm list"
