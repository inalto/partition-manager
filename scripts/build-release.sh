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
cargo build --release

version="$("$target_dir/release/pm" --version | awk '{print $2}')"
name="partition-manager-${version}-${os}-${arch}"
out="$root/dist/${os}-${arch}"
rm -rf "$out"
mkdir -p "$out"

for bin in pm partition-manager; do
  src="$target_dir/release/$bin"
  [ -x "$src" ] || { echo "missing build output: $src" >&2; exit 1; }
  cp "$src" "$out/$bin"
done
for doc in README.md LICENSE; do
  [ -f "$root/$doc" ] && cp "$root/$doc" "$out/"
done

echo
echo "dist/${os}-${arch}/:"
for f in "$out"/*; do
  printf '%-24s %12s bytes\n' "$(basename "$f")" "$(wc -c < "$f" | tr -d ' ')"
done

echo
"$out/pm" --version

tarball="$root/dist/${name}.tar.gz"
tar -czf "$tarball" -C "$root/dist" "${os}-${arch}"
echo
echo "wrote dist/${name}.tar.gz"
echo "Run with sudo for physical disks: sudo $out/pm list"
