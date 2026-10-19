# Final fix wave report (after the whole-branch review)

Status: DONE. All items implemented; live (elevated, real-disk) verification is still
outstanding for I3, I4, I6, I7 and I10 and is now documented in README and
`docs/FIRST-LIVE-TEST.md`.

## Commits (branch `dev`, on top of `cce21c9`)

| Commit | Item |
|---|---|
| `b17538e` | fix(ops): never trim a partition whose filesystem is unknown (C1, incl. I5) |
| `b817d15` | fix(ops): check partition and disk identity before every operation (C2) |
| `123dc99` | fix(cli): clone/restore/rescue write disks only through a write session (C3) |
| `017b24f` | fix(ops): clear old GPT areas from their headers, never inside a partition (I2) |
| `0efce65` | fix(ops): recognise the Windows partition by C:\ or \Windows\System32 (I9) |
| `0dae594` | feat(gui): log to stderr and %LOCALAPPDATA%\PartitionManager (I8) |
| `7dc9155` | fix(platform): request extended DASD I/O on volume handles (I3) |
| `51ce054` | fix(platform): FormatEx gets RemovableMedia for volumes on removable disks (I10) |
| `9cc7378` | fix(ops): minor review items (clone fit check, SetType extended, read-only/offline) |
| `2b3a892` | docs: first live test checklist, system-disk "device busy" limitation (I1) |

Every message ends with `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`; files were
staged individually.

## Test summary

`cargo test --workspace --no-fail-fast`: exit 0, **608 passed, 0 failed, 3 ignored** (was 588;
the 3 ignored are the elevated `windows_live` tests), zero compiler warnings.
`scripts/build-release.ps1`: release build OK; `dist/partition-manager.exe` 8.71 MiB (embeds
`requireAdministrator`), `dist/pm.exe` 4.21 MiB (`pm 0.1.0`, no manifest).

Selected `test result:` lines from the final run:

```
pm_ops (lib)   : test result: ok. 4 passed; 0 failed; 0 ignored
executor       : test result: ok. 29 passed; 0 failed; 0 ignored
planner        : test result: ok. 16 passed; 0 failed; 0 ignored
pm (cli bin)   : test result: ok. 50 passed; 0 failed; 0 ignored
cli            : test result: ok. 7 passed; 0 failed; 0 ignored
cli_image      : test result: ok. 14 passed; 0 failed; 0 ignored
cli_ops        : test result: ok. 18 passed; 0 failed; 0 ignored
pm_gui (lib)   : test result: ok. 121 passed; 0 failed; 0 ignored
pm_platform    : test result: ok. 54 passed; 0 failed; 0 ignored
file_platform  : test result: ok. 4 passed; 0 failed; 0 ignored
mock           : test result: ok. 5 passed; 0 failed; 0 ignored
windows_live   : test result: ok. 0 passed; 0 failed; 3 ignored
gpt            : test result: ok. 19 passed; 0 failed; 0 ignored
mbr            : test result: ok. 29 passed; 0 failed; 0 ignored
```

Note: on the very first baseline run `pm-analyze --test surface`
(`surface_flags_slow_blocks_over_ten_times_median`) failed once under full-workspace load and
passed on rerun and in every later run: a timing-sensitive test, untouched by this wave.

## Per item

### C1 — table-only shrink of an unknown filesystem (Critical)
- (a) `pm_platform::detect_filesystems(disk, dev)` (new, `pm-platform/src/lib.rs`): for every
  partition whose `fs` is still unknown (no mounted volume reported one), skipping MBR extended
  containers, runs `pm_fs::detect` at its start and fills `fs`/`label`; `used` stays `None`.
  Windows enumeration (`windows/disk.rs`) calls it after `merge_volumes` when the disk handle
  is readable (query-only handles skip it silently); `FilePlatform` uses the same helper.
- (b) Planner (`resize_move`, `align`) and `precheck` (`check_resize`, `align_changes` /
  `check_shrink`) refuse shrinking or align-trimming a partition with `fs == None` (extended
  containers exempt): "Cannot shrink partition N on Disk D: filesystem unknown; refresh or
  check the volume". The executor steps call the same checks with `verified_later = true` and,
  inside the write session and before any byte is moved or the table written, run
  `ensure_no_filesystem` (`steps/mod.rs`): an unreadable head, any `pm_fs::detect` signature or
  a BitLocker `-FVE-FS-` header refuses the operation. Align checks every trimmed partition
  first, then moves. NTFS shrinks still go through `shrink_volume`.
- (c) `steps::wait_for_volumes` (next to `locate_volume`): after a successful operation that
  wrote disk D, and before the next operation's live re-read, waits until `enumerate_volumes`
  reports at least as many volumes on D as the simulated model has partitions-with-volume on D,
  or until the set has stopped changing for 1 s after changing once; bounded 30 s, 250 ms polls,
  cancellable; a timeout only logs a note. `VOLUME_WAIT` raised to 30 s (I5).
- Tests: `shrink_of_a_partition_with_unknown_filesystem_is_refused` (executor.rs: NTFS boot
  sector, no volume -> precheck, queue and execute refuse, no session, bytes unchanged; growing
  works); in-crate `table_only_shrink_checks_the_partition_head_in_session` (direct step call
  past precheck: NTFS head refused with nothing written, a truly raw partition is trimmed),
  `table_only_align_trim_checks_every_partition_head_first` (BitLocker head),
  `waiting_for_volumes_returns_once_they_are_back`; `detect_filesystems_fills_unmounted_partitions_only`
  (pm-platform). Updated: `resize_refuses_shrink_with_unknown_used` (a raw partition no longer
  shrinks through the planner), and the two align executor tests now give their partitions
  NTFS volumes.
- Behaviour change to note: a partition with no recognised filesystem can no longer be shrunk
  or aligned through the planner/queue at all (ruling: no `force_raw` field).

### C2 — partitions targeted by number only (Critical)
- New `pm_ops::identity` with `layout_matches(expected, live, op)`: every disk the operation
  names must match on size, serial and disk id; every partition it names on start, length and
  (GPT) GUID; clone/migrate sources, align, convert and rebuild compare the whole partition list.
  Failure: "disk layout changed since the operations were queued; refresh and queue again (...)".
  `Operation::disks()` added.
- `execute_queued(platform, base, ops, ...)`: the expected model starts at `base` and is folded
  with `Planner::simulate` after each completed operation; identifiers the simulation invents
  (new partition GUIDs, disk ids after create/convert/clone/initialize) are replaced by what was
  really written (`adopt_generated_ids`, only for ids the simulation generated). Checked before
  every operation and in dry runs. `execute()` reads its own base and calls it.
- `Planner::simulate` renumbers MBR logicals 5.. in start (chain) order after every operation
  that rewrites the table, exactly as `read_table` numbers them after `write_table`.
- GUI Apply passes `queue.base()`; GUI `rebase` checks `layout_matches` op by op against the old
  chain (with `adopt_generated_ids`) and drops from the first mismatch; `pm apply` / CLI
  operation commands pass the model they planned on.
- Tests: `deleting_a_logical_renumbers_the_rest_in_the_preview_and_on_disk` (3 logicals; queued
  Delete 5 then Wipe of the partition the preview shows as 5 = old 6: old 6 wiped, old 7
  untouched; a list still using pre-delete numbering [Delete 5, Wipe 7] stops at op 2 with old 7
  and old 6 untouched), `operations_stop_when_the_disk_changed_since_they_were_queued` (disk
  swapped with other size+serial -> refused in dry run and real run, nothing written; serial
  only; partition replaced at another position; unchanged runs),
  `partitions_created_earlier_in_the_run_keep_their_identity` (create -> wipe -> convert GPT->MBR
  -> wipe; verified it fails without the id adoption), GUI `rebase_drops_ops_whose_partition_was_replaced`.
- Semantics: a queued operation's partition numbers refer to the preview after the operations
  before it (now renumbered like the disk). A literal list [Delete 5, Format 6] therefore
  formats the old 7 — which is what the preview shows as 6; the test asserts the preview and the
  disk agree, and that stale numbering is refused.

### C3 — CLI clone/restore/rescue without a session (Critical)
- `cmd_clone.rs`: `open_checked` = `probe_checked` (read-only, all checks) + `open_writable`.
  Disk, `disk:N:P` and volume destinations are opened through `platform.begin_write(disk)`;
  writes go through `SessionSlice` (bounded slice of the session device); `SessionGuard`
  finishes the session explicitly (`finish`, error reported) or on drop (error, cancel, panic).
  Image files are unchanged.
- Volume destinations whose disk extent is unknown (spanned/virtual) are refused before
  anything is opened.
- System-disk check uses `Model::from_platform(..).effective_system_disk()` and fails closed
  ("Disk N: cannot determine the running system disk; refusing to write to it"). These commands
  never had `--allow-system-disk`.
- `pm rescue` validates mapfile/argument errors before taking the session.
- `MockPlatform::add_spanned_volume` added for tests.
- Tests: `disk_destinations_are_written_in_a_finished_write_session` (disk, partition, volume,
  cancelled copy: sessions logged and all finished), `unknown_system_disk_refuses_every_disk_destination`
  (clone and restore, no session, nothing written; image files still fine),
  `spanned_volume_destination_is_refused`. `volume_and_disk_aliasing_is_refused` now sets a
  system disk (fail-closed would refuse it otherwise).

### I2 — 4Kn `zero_table_areas`
- Areas now come from the GPT headers: primary header at LBA 1 and its entry array, backup
  header at the last LBA (and at the primary's alternate LBA if different) and its array;
  unreadable headers fall back to `Table::empty`'s usable bounds for the sector size. Array
  sizes over 16 MiB are not believed. Nothing inside an entry of the new table is zeroed
  (callers pass it: initialize, convert, clone-disk; wipe passes an empty table).
- Test: `converting_a_4kn_gpt_to_mbr_keeps_data_at_the_end_of_the_disk` (4096-byte sectors,
  partition ending at the Windows-style last usable LBA; the 132 KiB before it survive — the old
  code zeroed 112 KiB of it — while the backup GPT and primary header are cleared).

### I9 — Windows partition detection
- `windows_partition(d, probe)`: C:\ -> `probe(disk, partition)` says the filesystem contains
  `\Windows\System32` -> largest NTFS data partition; `flags.boot`/`flags.required` rules removed.
- `platform_windows_probe(platform)`: opens the disk read-only, slices the partition,
  `pm_fs::open`, lists the root for a `Windows` dir then `System32` (listings over 4096 entries
  are ignored; any error = no). `Model::from_platform` runs it on NTFS/unknown candidates of
  disks without a C:\ partition and records the result in the new serializable field
  `Model::windows_found` (`#[serde(default)]`); `Model::windows_probe()` /
  `windows_partition_of()` feed it to the planner guards, `plan_disk_clone` (new `probe`
  parameter), the clone source-shrink plan, the migrate step and the GUI migrate wizard.
- Tests: rewritten `windows_partition_detection_order` (C:\ wins without asking the probe; the
  probe beats a larger flagged data partition; nothing found -> largest NTFS, flags ignored;
  guards and sizing follow; serde round-trip), new
  `model_from_platform_finds_windows_by_its_system32_folder` (FAT volume with
  WINDOWS\SYSTEM32 found through the mock platform). Fixture: `boot` flag dropped (the
  `required` bit stays only because a clone test checks GPT attributes are kept).

### I8 — GUI logger
- `pm-gui/src/logging.rs`: `env_logger` (level from `RUST_LOG`, default `info`) writing through
  a `Tee` to stderr and `%LOCALAPPDATA%\PartitionManager\partition-manager.log` (append; folder
  created); never fails the app. Called first thing in `main`. Settings menu: "Open log folder"
  (`Cmd::OpenLogFolder` -> `pm_platform::shell::open_in_file_manager`).
- Tests: `tee_appends_to_the_file`, `log_file_is_under_local_app_data`.

### I3 — FSCTL_ALLOW_EXTENDED_DASD_IO
- `WinDevice::open` issues `FSCTL_ALLOW_EXTENDED_DASD_IO` (windows-sys constant, 0x90083) via
  the ioctl helper on volume paths (`\\.\X:`, `\\?\Volume{..}`); failure is a `warn` log.
- Test: `extended_dasd_io_is_requested_for_volume_paths_only` (constant value and path
  classification). Live verification still needed (command in FIRST-LIVE-TEST.md).

### I10 — FormatEx media type
- winioctl `MEDIA_TYPE`: RemovableMedia = 11, FixedMedia = 12. The old constant was already 12
  (fixed). `format_ex` now takes the media type; `WindowsPlatform::format` looks up the
  volume's disk and its removable flag (`disk::is_removable`, storage device descriptor;
  unknown -> fixed).
- Test: `format_media_types_follow_winioctl`. Live verification on a USB stick still needed.

### I1 — documentation
- README Known limitations: operations on the running system disk stop with "device busy"
  (C: cannot be locked); nothing is written; a lock-free path is planned. The GUI Apply
  confirmation shows the same note whenever the system disk is involved
  (`SYSTEM_DISK_BUSY_NOTE`).

### Docs — before the first real-disk run
- README section "Before the first real-disk run" (summary + link) and
  `docs/FIRST-LIVE-TEST.md` (target/setup, the 12 steps, recovery steps, what to know — no-VSS
  warning, "nothing has run elevated against a real disk yet", live-verification items I3, I4,
  I6, I7, I10, GUI log path). README safety model and `docs/ARCHITECTURE.md` safety-layer table
  updated for the identity check, unknown-filesystem refusal, read-only/offline refusal and the
  sessions of the direct-copy commands.

### Minor items
- `steps/clone.rs`: both fit checks use `FsFacts::footprint()` = max(`total`, used-map extent)
  (NTFS: volume + backup boot sector). Test `filesystem_footprint_includes_the_ntfs_backup_boot_sector`.
- Planner: `SetType` refuses MBR extended types for logicals as well as primaries.
- `precheck` refuses any write to a disk with `flags.read_only` or `flags.offline`, naming the
  `Set-Disk` command. Test `read_only_or_offline_disks_and_extended_types_are_refused`.

## Not done / open
- Nothing was run elevated or against a real disk (not possible here): I3, I4, I6, I7, I10 and
  the C1 Windows enumeration/volume-wait paths need the live checklist.
- I4 (automount letter race), I6 (MountedDevices after migration) and I7 (no VSS) are
  documented, not fixed (the ruling put them in the live checklist).
- `cargo clippy` is not installed for the `stable-x86_64-pc-windows-gnu` toolchain, so only
  compiler warnings were checked (zero).
- Model::from_platform now also probes NTFS/unknown partitions of disks without C:\ for
  `\Windows\System32` on every read (executor re-reads before each operation): a few metadata
  reads per such partition.
