# Design critique: Partition Manager main window

Date: 2026-11-16. Reviewed: the main window in the light theme (one screenshot, nothing
selected, three disks) and the code that draws it (`crates/pm-gui/src/views`, `theme.rs`).
Not reviewed on screen: the dark theme, dialogs, wizards and tool windows. Findings about
those come from the code only and are marked as such.

## Overall impression

The layout is the right one for this kind of tool: actions on the left, the partition table
on top, the disk map below, a queue that nothing touches until Apply. It reads as a
partition manager at first glance. The biggest opportunity is the left panel: with nothing
selected it is a wall of about 25 greyed-out rows, so the first thing a new user sees is a
list of things they cannot do, with no hint of what to do first.

## Usability

| # | Finding | Severity | Recommendation |
|---|---------|----------|----------------|
| 1 | With no selection every action in the side panel is disabled, and nothing says why. The only hint ("Pick an action on the left or right-click a disk or partition") sits in the pending list and points at actions that are all greyed out. | Critical | Make the panel follow the selection. Nothing selected: show one line, "Select a disk or a partition to see what you can do", plus the tools. Disk selected: show only the disk groups. Partition selected: show only "Manage partition". Actions that apply but are blocked keep a tooltip saying why. |
| 2 | Apply, the one button that writes to disks, looks like every other toolbar button and is far from the queue it applies. | Critical | Give Apply a filled accent style when the queue is not empty and show the count ("Apply 3"). Move Apply / Undo / Discard next to the pending list, or repeat them there. |
| 3 | The side panel does not fit: the groups scroll, and the Tools group adds nine more rows on top. On a 850 px tall window "Manage partition" is cut off. | Moderate | Follows from 1: a panel that shows only what applies is short enough not to scroll. Until then, collapse the groups that do not apply to the selection. |
| 4 | Destructive actions (Wipe Disk, Delete All Partitions, Delete, Format, Wipe Partition) look the same as Change Label. | Moderate | Mark them in the panel and the context menu: a warning glyph and the danger colour on hover. The queue already colours them red; the place where you pick them should too. |
| 5 | Small partitions are unreadable in the disk map: "D:SYST", "1015.", "*:WinI", "3.82 C". The text is clipped mid-word. | Moderate | When a segment is narrower than its label, drop the text and keep the colour strip, with the full label in the tooltip. Never show half a number. Consider a wider minimum segment for partitions under 1% of the disk. |
| 6 | "Backup / Image" and "Disk tools" open menus but look like plain buttons. | Moderate | Add a caret (▾) to both. |
| 7 | "Partition Recovery" appears three times: toolbar, Wizards group, Tools group. Surface Test also appears under "Check disk" and under Tools. | Minor | Decide one home per entry. Suggested: wizards that act on the selection stay in the action groups, tools that open their own window live in Tools, and the toolbar keeps only Apply / Undo / Discard / Refresh / Settings. |
| 8 | The pending list always takes 180 px, also when empty. | Minor | Collapse it to a single line ("No pending operations") when empty and grow it with the queue. |
| 9 | No keyboard shortcuts in the main window (code: none registered). | Minor | F5 Refresh, Ctrl+Z Undo, Ctrl+Enter Apply, Delete for Delete, F2 for Change Label; show them in tooltips. |

## Visual hierarchy

- **What draws the eye first:** the disk map, because it is the only coloured area. That is
  correct: it is the best summary of the machine.
- **Reading flow:** toolbar, then the greyed-out panel on the left, then the table. The
  panel takes attention it has not earned yet (finding 1).
- **Emphasis:** disk header rows in the table are bold but otherwise identical to partition
  rows, and they end in a clipped device path (`\\.\Physic…`). Give the disk rows a tinted
  background across the full width, and move the device path to the tooltip or to Disk
  Properties.
- **Whitespace:** the table leaves a large empty area with column separators running
  through it. Stop the separators at the last row, or let the disk map take the free space
  with taller bars.
- **Numbers:** Capacity, Used and Unused are left-aligned, so sizes cannot be compared down
  a column. Right-align them. A thin usage bar in the Used cell would make full partitions
  stand out without reading.

## Consistency

| Element | Issue | Recommendation |
|---------|-------|----------------|
| Icons | Apply, Undo, Discard, Refresh and Settings have icons; the tool buttons have none. The window shows the default eframe icon. | Either icons on every toolbar button or on none. Add an app icon (needed for the installer anyway). |
| Selection colour | A selected table row is light blue, the same item in the disk map gets an amber outline. | One selection colour in both views. Blue fill in the table and a blue 2 px outline in the map would do. |
| Partition names | `D:SYSTEM`, `*:`, `*:EFI`: no space after the colon, and `*` for "no drive letter" is not explained anywhere. | `D: SYSTEM`; for no letter show the label alone (`EFI`), or `(no letter)` when there is no label either. |
| Empty values | Unknown usage is `-`, no status is the word `None`, no drive letter is `*`. | One convention: an em dash or an empty cell for "nothing to say". |
| Colours | The same green, orange and blue are written as literals in six files (`smart.rs`, `surface_test.rs`, `benchmark.rs`, `rescue.rs`, `tools/mod.rs`, `main_window.rs`). | Move them into `theme.rs` as `OK`, `WARN`, `DANGER`, `ACCENT` and give each a dark-theme value. |
| Font sizes | Painted text uses 9, 10, 11, 11.5, 12 and 13 px in different files. | Three sizes in `theme.rs` (small, body, strong) and nothing below 11 px. |
| Legend | "GPT/Primary" mixes a table kind and a partition kind; the pale fill that means "used space" is not in the legend. | "Primary / data", and add "Used" and "Free" swatches. |

## Accessibility

- **Colour as the only signal:** EFI/System (`#1E4E8C`), Recovery (`#4B5AA8`) and
  GPT/Primary (`#3C78C8`) are three blues shown on a 6 px strip. They are hard to tell
  apart for anyone, and close to impossible with a colour vision deficiency. Use hues that
  differ in lightness as well (for example blue, teal, purple, grey), make the strip 8 to
  10 px, and keep the type readable in the tooltip and the table so colour is never the
  only carrier.
- **Contrast:** body text (`#202020` on white and on `#F0F2F5`) passes comfortably.
  Disabled text is egui's default light grey on a light grey panel; that is allowed for
  disabled controls, but here it is most of the panel (finding 1). The hint text in the
  pending list uses the same weak colour and is the only guidance on screen: give it the
  normal text colour.
- **Text size:** the disk map draws at 11.5 px, the benchmark chart at 9 to 10 px. Small
  for a dense screen and not scaled by any setting. Raise the floor to 11 px and add a
  zoom setting (egui supports `set_zoom_factor`; Ctrl + and Ctrl - are conventional).
- **Target size:** side-panel rows are frameless text about 20 px tall. Fine for a mouse,
  too small for touch. Not a priority for this audience.
- **Keyboard and screen readers:** not verified. Worth checking that the table and the
  disk map can be reached with Tab and arrows, and whether eframe's AccessKit support is
  enabled in the build.

## What works well

- The three-pane layout matches what people know from other partition tools, so there is
  nothing to learn.
- The disk map shows used space inside each partition, and the table and the map stay in
  step with the queued operations.
- Nothing is written until Apply, and the queue lists each step in plain words with
  destructive ones in red.
- The status bar says whether the app is elevated, in red when it is not.
- A dry-run mode exists and is labelled on the Apply button when it is on.

## Priority recommendations

1. **Make the side panel follow the selection.** It fixes the greyed-out first impression,
   the missing "what do I do first" hint and the scrolling in one change.
2. **Make Apply unmistakable.** Filled accent button with the count of pending
   operations, placed with the queue.
3. **Fix the disk map for small partitions and for colour.** No clipped labels, distinct
   hues, a wider strip, "Used" in the legend.
4. **Tidy the table.** Right-aligned sizes, tinted disk rows without the device path,
   readable partition names, one convention for empty values.
5. **One home per tool, and one place for colours and font sizes.** Remove the triple
   entries, then centralise the literals in `theme.rs`.
