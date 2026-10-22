#!/usr/bin/env bash
# Regenerates docs/CLI.md from the clap definitions. Usage: scripts/gen-cli-docs.sh [path-to-pm]
set -eu
PM="${1:-target/debug/pm.exe}"
[ -x "$PM" ] || PM="${1:-target/debug/pm}"
OUT=docs/CLI.md
help() { "$PM" "$@" --help 2>&1 | tr -d '\r'; }
section() { # level title args...
  local level="$1" title="$2"; shift 2
  printf '%s %s\n\n```text\n%s\n```\n\n' "$level" "$title" "$(help "$@")"
}
{
  cat <<'EOF'
# `pm` command reference

Generated from the clap definitions by `scripts/gen-cli-docs.sh`; the output of
`pm <command> --help` is authoritative. Regenerate this file after changing the CLI.

**Device arguments.** Commands that take a device accept `disk:N` (physical disk N),
`disk:N:P` (partition P of disk N, 1-based), a drive letter (`D:`) or a `\\?\Volume{..}\`
path (Windows), a `/dev/...` node (Linux, macOS: a whole disk such as `/dev/sdb`,
`/dev/nvme0n1` or `/dev/disk2` / `/dev/rdisk2` is the same as its `disk:N`, `/dev/sdb:2` is its
partition 2, and a partition node such as `/dev/sdb2` or `/dev/disk2s1` is a volume), or an
image file (`file:PATH`, `file:PATH:P`, or a bare path). Commands that take a disk index
(`pm create 1 ...`) accept the whole-disk node too. On Linux and macOS prefer the `/dev/...`
form: `disk:N` is a position in the disk list that can change when a disk is plugged in or
removed; `pm list` prints each disk's node next to its number. With the global
`--disk-file PATH` option (repeatable) the image files become `disk:0`, `disk:1`, ... and no
administrator (root) rights are needed.

**Sizes** accept binary suffixes: `4096`, `64K`, `1M`, `1.5G`.

**Exit codes.** 0 success, 1 usage error or invalid argument, 2 platform or runtime error.

**Writing commands.** The operation commands (`init-disk`, `create`, `delete`, `format`,
`resize`, ...) and `apply` show the operation and validate it against the current disks, and
write nothing unless `--yes` is given; `--dry-run` validates only; `--emit-json` prints the
operation as JSON for `pm apply`. `clone`, `restore`, `image`, `convert` and `rescue` copy
immediately when run, after refusing the unsafe combinations listed in their help (source
equals destination, the running system disk, overlapping targets).

EOF
  section '##' 'pm'
  for c in list table mount unmount doctor clone image restore convert rescue probe ls cat extract apply; do
    section '##' "pm $c" "$c"
  done
  for c in init-disk create delete format resize set-active set-label set-letter set-type wipe-disk wipe-partition convert-table align rebuild-mbr clone-disk migrate-os image-disk image-partition restore-image; do
    section '##' "pm $c" "$c"
  done
  section '##' 'pm recover' recover
  for c in partitions files carve open-session; do section '###' "pm recover $c" recover "$c"; done
  section '##' 'pm analyze' analyze
  for c in space surface bench smart; do section '###' "pm analyze $c" analyze "$c"; done
} > "$OUT"
echo "wrote $OUT"
