# Partition Manager

A Windows-first disk partition manager written in Rust: clone, image, rescue and
recover disks, with a wizard-style GUI (planned).

## Workspace layout

- `crates/pm-core` - block device abstraction, progress/cancellation, size units and
  the disk/partition domain model. Everything else builds on this crate.

## Building

```
cargo build
cargo test -p pm-core
```

`.cargo/config.toml` and `rust-toolchain.toml` select the GNU toolchain with a portable
MinGW on machines without MSVC Build Tools; delete both if you have MSVC installed.

## Conventions

- All sizes are bytes (`u64`); human-readable sizes use binary units shown as KB/MB/GB/TB.
- Long operations take a `&dyn ProgressSink` and a `&CancelToken`.
- `pm-core` contains no `unsafe` code.
