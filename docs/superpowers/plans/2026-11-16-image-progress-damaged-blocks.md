# Create Image progress bar that shows damaged blocks: plan

**Goal:** while Create Image runs, the progress bar is a map of the source: each slice of the
bar is coloured by what happened to that part of the disk, so damaged blocks show up in
another colour, at the place where they are, while the copy is still running.

**Status:** implemented on 2026-11-16, with these differences from the plan below: the
states are ordered Skipped, Done, Pending, Retrying, Bad; the text under the bar is the
engine's existing `detail` line plus a legend that counts the damaged and retrying slices
(no separate "Damaged: n areas" sentence); the tooltip gives the state and the position as
a percentage of the source, not a byte range. Never seen on screen and never run against
a failing disk: tested with `FaultyDevice` only.

## How it works today

- Create Image is a queued operation (`Operation::ImageDisk` / `ImagePartition`). Apply runs
  it in `crates/pm-ops/src/steps/image.rs`, which calls `pm_image::rescue`.
- Progress travels as `pm_core::ProgressReport { phase, done, total, rate_bps, eta_secs,
  detail }` into `JobHandle::progress`; `crates/pm-gui/src/dialogs/progress.rs` draws one
  `egui::ProgressBar` from `done / total`.
- Damaged blocks only exist in one mode. Without "Continue on read errors" the engine runs
  with `abort_on_error: true` and the first read error stops the job. With it (rescue
  mode), the engine keeps a `BlockMap` of every area: finished, non-tried, non-trimmed,
  non-scraped, bad.
- That map never reaches the GUI during the run. It is written to the mapfile every 30 s
  and the Rescue Copy tool draws it only when the job has ended (`map_bar` /
  `map_bar_ui` / `status_color` in `crates/pm-gui/src/wizards/rescue.rs`).

So the data exists; what is missing is a live, cheap summary of it in the progress report,
and a bar that draws it.

## Design

One new optional field on the progress report carries the map as a fixed number of slices.
Everything else follows from it.

```rust
// pm-core/src/progress.rs
/// What happened to one slice of the source, worst case in the slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SliceState { Pending, Done, Skipped, Retrying, Bad }

pub struct ProgressReport {
    // existing fields
    /// The source cut into equal slices, each with the worst state found in it.
    /// `None`: the job has no map (every job but a copy that can meet read errors).
    pub slices: Option<Arc<[SliceState]>>,
}
```

- `Pending`: not read yet. `Done`: read and written. `Skipped`: unused space left out by
  the used map (Smart mode). `Retrying`: failed once, still to be trimmed or scraped
  (non-trimmed, non-scraped). `Bad`: unreadable.
- Worst state wins inside a slice (Bad > Retrying > Pending > Done > Skipped), the same
  rule `map_bar` uses, so a single bad sector is visible even on a 2 TB disk.
- 512 slices: more than the pixels of the dialog's bar, small enough to copy on every report.
- `Arc<[SliceState]>` so that cloning the report for `JobHandle::snapshot` stays cheap.

### Engine (`pm-image/src/rescue`)

- `BlockMap` gains a per-slice summary kept up to date inside `insert` / `remove`, where it
  already keeps `bytes` and `count` per status: `slice_bytes: Vec<[u64; STATUSES]>`. A
  block that spans slices adds its share to each. This keeps the cost at the blocks
  touched by a `mark`, not at the whole map: iterating the map on every report is ruled
  out, because a fragmented used map produces millions of blocks.
- `BlockMap::slices(&self, unused: ...) -> Vec<SliceState>` reads the summary: per slice,
  the worst status with bytes greater than zero. Unused areas are marked finished in the
  map today, so the engine also records them as their own counter to tell `Skipped` from
  `Done`.
- The engine attaches the slices to the report where it already refreshes `detail`, at
  most every 250 ms (`DETAIL_INTERVAL`), and once more at the end.
- `ProgressTracker` gets `set_slices`, next to `set_detail`.
- Sent in both modes. In the default mode (abort on first error) the bar shows Done /
  Skipped / Pending and, when a read fails, the slice that failed in the Bad colour before
  the error dialog appears: it tells the user where the disk is damaged, which is the
  first thing they need for deciding whether to switch to rescue mode.

### GUI (`pm-gui`)

- `dialogs/progress.rs`: when `p.slices` is `Some`, draw `slice_bar` in place of the
  `egui::ProgressBar`; otherwise nothing changes for the other jobs.
- `slice_bar`: same height and rounding as the current bar; one rectangle per run of
  equal slices; the percentage stays, drawn as text under the bar, with "Done x of y".
- Colours from `theme.rs` (already centralised): Done `ACCENT`, Skipped the bar
  background a little darker, Pending the bar background, Retrying `WARN`, Bad `DANGER`.
- Never colour alone: under the bar a line of text appears as soon as something is not
  clean, for example "Damaged: 3 areas, 1.5 MB. Still retrying: 64 KB." (from the
  `detail` the engine already produces: errsize, errors). A small legend (Done, Skipped,
  Retrying, Damaged) shows only when a non-clean state is present, so a healthy disk gets
  a plain bar.
- Tooltip on the bar: the byte range of the slice under the pointer and its state.
- The Rescue Copy tool keeps its own five-colour map; `status_color` and the new slice
  colours come from the same `theme.rs` constants so the two agree. Making Rescue Copy
  live with the same field is a follow-up, not part of this change.

### What it looks like

```
Creating image D:\backup\disk1.vhdx
[#########x####~#######.............................]     x bad   ~ retrying   . pending
44%   Done 812 GB of 1.82 TB   Speed 142 MB/s   Time left 1:58:10
Damaged: 1 area, 64 KB. Still retrying: 128 KB.
```

## Tasks

1. **pm-core: `SliceState`, `ProgressReport::slices`, `ProgressTracker::set_slices`.**
   Tests: default report has `None`; a tracker report carries the slices set last.
2. **pm-image: per-slice summary in `BlockMap`.** Tests: marking a range that spans three
   slices updates the three; re-marking bad as finished clears the bad slice; the summary
   equals one recomputed from `to_blocks()` after a random sequence of marks (property
   test against the existing `map_bar` rule); a zero-length source gives no slices.
3. **pm-image: engine reports slices.** Tests with the existing `FaultyDevice`: a source
   with bad sectors at known offsets ends with exactly those slices `Bad`; Smart mode
   reports `Skipped` for unused areas; clone mode (`abort_on_error`) reports the failing
   slice as `Bad` before returning the error.
4. **pm-gui: `slice_bar` and the progress dialog.** Tests on the pure parts: runs of equal
   slices (`[Done, Done, Bad, Pending]` gives three runs with their fractions), the
   summary line from the report, the legend shown only when needed.
5. **Docs:** GUI Guide (progress dialog), CHANGELOG.

Tasks 1 to 3 are independent of the GUI and testable on their own; 4 needs 1.

## What to watch

- **Cost of the summary** on a heavily fragmented used map: measure `mark_many` before and
  after on the existing large-map test; the summary must not change its complexity.
- **Slices vs sectors:** one slice of a 2 TB disk is about 3.7 GB. "Bad" on the bar means
  "something in here is bad", never "this whole slice is lost"; the text line carries the
  real size.
- **Resumed runs** start from a mapfile: the first report must already show the earlier
  state, not an empty bar.
- **Not verifiable on real hardware here:** the project rule is never to run against a
  real physical disk, so this is tested with `FaultyDevice` and image files only.
