# Task 19 report: pm-gui wizards (partition ops)

**Status:** DONE
**Commit:** `5b59f9c feat(gui): partition wizards` (branch `dev`, on top of the concurrent shell fix `1e21ea7`; it contains only my files)

## What was implemented

### Framework: `crates/pm-gui/src/wizard.rs`
- `WizardStep` trait (`title`, `ui(&mut Ui, &WizardCtx) -> StepResult`, `summary() -> Vec<(String, String)>`). `StepResult` is `Continue` or `Invalid(String)`.
- `WizardCtx { model (preview), platform_free_letters, selection, system_disk }`. `WizardCtx::new` drops the letters the preview already uses and works out the system disk with `effective_system_disk`.
- `Wizard { title, steps, idx, error, on_finish, valid }`. `show(ctx, &WizardCtx) -> WizardOutcome { Open | Cancelled | Finished(ops) }` draws a modal window with:
  - a numbered step list (the current step in bold, later steps weak);
  - the page;
  - Back, Next, Finish and Cancel buttons. Next is disabled while the step is `Invalid`, and the invalid message is shown in red under the page. Escape cancels.
- The summary page is generated as the last page. It lists every step's `summary()` rows and the `op.summary()` lines. It runs `check_ops`, which does `precheck` and then `Planner::simulate` in order, exactly like `OpQueue::push`. Any error is shown inline and Finish stays disabled until it is fixed. `set_error` puts a queue rejection back on the summary page.
- To make writing wizards easy for Task 20:
  - `Shared<T>` = `Rc<RefCell<T>>`.
  - `FnStep<S>`: a step made of two functions over a shared state.
  - `Wizard::over_state(title, state, [(title, ui_fn, summary_fn)], build)`.
  - The module docs explain the pattern.

### Wizards: `crates/pm-gui/src/wizards/`
Every wizard follows the same pattern: a `…State` struct, a pure `build(&State) -> Result<Vec<Operation>, String>`, `initial(ctx)`, step functions, `wizard(state)` and `new(ctx) -> Option<Wizard>`. `for_action(Action, &WizardCtx)` and `ACTIONS` map the sidebar actions to the wizards.

| Wizard | Steps → op |
|---|---|
| `create` | **Location**: disk combo and a free-range list from `free_ranges(MIB)`, preselected from the selection. **Size and position**: the draggable bar over the free range plus the before/size/after fields (MB by default, or `parse_size` units), snapped to 1 MiB with a 1 MiB minimum. The end never goes off the grid, and there is a preview bar of the simulated disk. **Properties**: GPT types (basic/EFI/MSR/recovery/Linux) or MBR presets (0x07/0x0C/0x83, which follow the filesystem); filesystem None/NTFS/FAT32/exFAT (FAT32 disabled above 32 GiB, with a note); label; letter from the free letters; GPT name. → `CreatePartition` |
| `format` | Filesystem (NTFS, FAT32 up to 32 GiB, exFAT, ReFS from 2 GiB), label, cluster size (Default or 512 B to 64 KB), Quick checkbox, and a red "all data … will be erased" warning. → `FormatPartition` |
| `resize` (+ `resize_bar`) | The MiniTool bar spans the partition's maximal extent (free neighbours; for a logical partition, its container and the 1 MiB EBR slack). It has a left handle, a right handle and a draggable body. The used data is hatched and the partition cannot shrink into it; a filesystem whose usage is unknown cannot shrink at all. The before/size/after fields stay in sync. Edges snap to 1 MiB, but the legacy unaligned start and end are kept for in-place resizes. A live before/after `disk_preview` comes from `check_ops`, with errors inline. → `ResizeMove` |
| `delete` | A confirmation page with the partition's details and a red warning. → `DeletePartition` |
| `label_letter` | One wizard with two modes. Label: ≤ 32 characters, ≤ 11 on FAT/exFAT. Letter: the free letters, the partition's own letter, or None. → `SetLabel` / `SetLetter` |
| `wipe` | Target (the disk, or the selected partition) and method (Zero, Random, DoD 3 passes) with duration hints at an assumed 150 MB/s, then "type WIPE to confirm" (case-sensitive). → `WipeDisk` / `WipePartition` |
| `convert` | An explanation for each direction, a requirements checklist (≤ 4 partitions and ≤ 2 TB for MBR, ≤ 128 for GPT), the EFI boot warning, and the planner's verdict. → `ConvertTable` |
| `initialize` | GPT (recommended, the default) or MBR (disabled above 2 TB). → `InitializeDisk` |
| `mount` | **Mount**: a drive letter, or a folder (it must be a full path to an existing, empty folder). The filesystem check runs only when the text changes. → `Mount`. **Unmount**: a radio list of the current mount points. → `Unmount` |

Shared helpers in `wizards/mod.rs`: `parse_size_mb`, `size_edit`, `letter_combo`, `disk_preview` (a compact disk-map bar that reuses `disk_map::segments` and `map_colors`), `label_problem`, `danger` and `partition_desc`.

### Hooks into the shell (kept small)
- `app.rs`:
  - New `AppState::wizard: Option<OpenWizard>` (the wizard, its selection, and the free letters read once at open).
  - `modal()` now includes the wizard.
  - `try_push_ops` was split out of `push_ops`, and `wizard_ctx()` / `open_wizard()` were added.
  - The `Effect::Wizard` and `ChooseWipeMethod` / `ChooseMountLetter` / `ChooseUnmount` effects now open wizards. Actions without a wizard yet keep the `pending_action` toast; a wizard action whose selection does not fit shows a "not available" toast.
  - New `Cmd::FinishWizard(ops)`: queues all or none and closes on success; otherwise it calls `set_error` on the wizard. New `Cmd::CloseWizard`.
- `main_window.rs`: draws the open wizard each frame and turns its outcome into those Cmds.
- `actions.rs`:
  - New `Action::InitializeDisk` in "Convert disk", enabled for an uninitialized disk or its free space.
  - Convert MBR↔GPT now opens its wizard instead of pushing directly.
  - Create requires an initialized table.
- Task 18's direct actions are unchanged: Delete, Delete All, Set Active, Align, Rebuild MBR, Properties and Explore.
- `lib.rs`: `pub mod wizard; pub mod wizards;`.

## Tests (TDD evidence)
- `cargo test -p pm-gui`: **78 passed, 0 failed** (baseline 44, +34 new). `cargo build -p pm-gui`: **zero warnings**.
- New tests:
  - **Pure math:** `wizards::resize_bar::tests::resize_bar_math` (as named in the brief: pixels↔bytes round trip, minimum = used, clamping to the neighbours, 1 MiB snapping, field sync), `legacy_unaligned_partition_keeps_or_aligns_its_edges`, `create_extent_never_ends_off_grid`.
  - **Framework:** `wizard::tests::{next_waits_for_a_valid_step_and_summary_checks_ops, ctx_narrows_letters_and_finds_system_disk, check_ops_is_in_order_and_rejects_empty}`.
  - **Op builders from a filled state (no egui):** `create::{build_create_op_from_filled_state, create_size_snapping_and_clamping, create_validation, mbr_types_follow_the_filesystem_and_free_range_is_preselected}`, `format::{build_format_op_from_filled_state, format_validation}`, `resize::{build_resize_op_from_state, extent_spans_free_neighbours_and_min_is_used, container_and_non_partition_selections_have_no_wizard}`, `delete::build_delete_op`, `label_letter::{build_label_and_letter_ops, fat_labels_and_unformatted_partitions}`, `wipe::{build_wipe_ops_needs_typed_confirmation, duration_hints}`, `convert::{build_convert_ops_and_constraints, mbr_limits}`, `initialize::build_initialize_ops`, `mount::{build_mount_ops, build_unmount_ops}`. Each also checks that `wizard(state).build_ops()` returns the same ops and that `check_ops` accepts them on the fixture model.
  - **Headless UI:** `wizards::tests::wizards_open_and_render_every_page` opens all 13 wizard actions through `for_action` and renders every page, including the summary, in light and dark themes without a panic.
  - **App integration:** `app::tests::{wizard_actions_open_their_wizard, finishing_a_wizard_queues_or_shows_the_error_on_it, actions_without_a_wizard_yet_are_remembered_with_a_toast, push_command_and_properties_window}`, `actions::tests::uninitialized_disk_offers_initialize_not_create`, and `main_window::frames_render_headless`, which now opens Resize/Move inside a real frame.
- **RED evidence:**
  - The tests for each module were written together with its code. Nothing failed on first compile apart from a borrow error in `mount.rs`.
  - To prove the tests bite, I ran mutations:
    1. Disabling grid snapping in `resize_bar::snap_within` made all 3 `resize_bar` tests FAIL (lines 218, 245, 270).
    2. Not trimming the label in `CreateState::build` made `build_create_op_from_filled_state` FAIL.
    3. A case-insensitive WIPE confirmation made `build_wipe_ops_needs_typed_confirmation` FAIL.
  - All were restored and are GREEN.
  - The framework, shell and actions tests were written before the hook code. The old Task 18 tests that asserted the WipeMethod/Unmount dialogs and the Format toast were replaced.
- **Startup check:**
  - A throwaway scratch crate (outside the repo) used pm-parttable and pm-fs to write a 100 MiB raw image with a GPT and one 60 MiB FAT32 basic-data partition labelled "TEST".
  - `partition-manager.exe --disk-file test.img` was still running after 6 s with empty stderr, then I stopped it.

## Files changed
- Added: `crates/pm-gui/src/wizard.rs`, `crates/pm-gui/src/wizards/{mod,resize_bar,resize,create,format,delete,label_letter,wipe,convert,initialize,mount}.rs`
- Modified: `crates/pm-gui/src/{lib.rs, actions.rs, app.rs, main_window.rs}`

## Self-review / deviations from the brief
- **`on_finish` is `Fn`, not `FnOnce`.** The summary page has to build the operations to show and check them before Finish, so it is called more than once. It returns `Result<Vec<Operation>, String>` for UI messages.
- **Wizard `new` functions return `Option<Wizard>`** (`None` when the selection does not fit), so they never panic.
- **Steps share state through `Rc<RefCell<State>>`.** `on_finish` ignores its `steps` argument and calls the pure `build` on that state. Tests build the state directly.
- **The Delete wizard exists but the sidebar Delete is still the direct push**, as instructed ("keep Task 18's direct actions working"): it is a pending, undoable operation, as in MiniTool. Routing it to the wizard is a one-line change in `actions.rs` (`DeletePartition => Some(Effect::Wizard(self))`).
- **Convert MBR↔GPT now opens its wizard instead of pushing directly**, so the brief's explanation and constraints are shown. I also added an Initialize Disk action, without which the Initialize wizard was unreachable.

## Concerns / follow-ups
1. **Dead code left in the shell under review.** `Dialog::WipeMethod`, `Dialog::MountLetter`, `Dialog::Unmount` and their rendering in `dialogs/mod.rs` (`wipe_method`, `mount_point_of`) can no longer be reached, because dispatch opens wizards. I did not delete them to keep my edits out of `dialogs/*`, which is under review. Remove them in the shell-fix pass. `wizards::mount::mount_point_of` duplicates the dialogs helper for the same reason.
2. **Create cannot make MBR logical partitions.** Free space inside an extended container is not offered: `free_ranges` treats the container as occupied, and that matches the planner, which has no logical-create path.
3. **Resize bar on a tiny partition.** On a partition that is very small compared with its extent, the two handles overlap the body and the end handle wins. Moving is then done with the "Unallocated space before" field.
4. **SetLabel is refused by the executor's precheck**, as the brief expected. The message shows on the Label wizard's summary page and Finish is disabled.
5. **Choices of mine:**
   - ReFS is offered from 2 GiB.
   - Wipe duration hints assume 150 MB/s.
   - A bare number typed in a size field means MB.
6. **Visual check was limited.** The pages were verified by headless rendering and the app starts with a test image, but no human has clicked through the dialogs. Layout polish (widths, spacing) may need a visual pass.
7. **Environment:**
   - clippy and rustfmt are not installed on this toolchain, so the code was formatted by hand to the repo's style.
   - I once saw cargo treat edited files as fresh (a timestamp quirk); `touch` fixed it.
   - Another agent committed `1e21ea7` to the pm-gui shell while I was working. My commit is on top of it and contains only my changes.

## Fix round 1

**Commit:** `82676c1 fix(gui): wizard review fixes: unformatted create, size fields, delete wizard`. It holds only my 11 files. Task 20's in-progress `Cargo.toml`, `jobs.rs`, `lib.rs` (`pub mod tools;`) and `tools/` were not staged.

1. **(Important) Create without a filesystem.**
   - `CreateState::letter_and_label()` gives `(None, "")` when `fs` is `None`. Both `build()` and the summary use it.
   - `props_problem()` no longer checks the letter or label when there is no filesystem.
   - In `props_ui`, the label and letter controls are disabled without a filesystem, with a note explaining why.
   - Choosing a filesystem again restores what the user had entered.
   - Test: `create::tests::create_without_fs_has_no_letter_or_label`.
2. **(Important) Size fields re-parsing their rounded text.**
   - New `wizards::SizeText { text, at_focus }`:
     - On focus gain the text switches to the full-precision MB value (`precise_mb`, 7 decimals, which round-trips to the byte).
     - On focus loss a value is applied only if the text differs from what it was at focus gain.
     - Unfocused, the field shows the `format_size` value.
   - `size_edit` takes a `&mut SizeText`. `BarState.texts` is now `[SizeText; 3]`.
   - Test: `wizards::tests::size_field_applies_only_user_changes`, on a value that is not a round number of MB. It checks that:
     - tabbing through the field applies nothing;
     - retyping the same text applies nothing;
     - the precise text parses back to the exact byte;
     - a real edit is applied exactly once, and not while the user is still typing.
   - The first version used 6 decimals and failed the byte round-trip (it came back 1 byte off). That was the RED evidence; it is fixed with 7.
3. **Cleanup.**
   - **Delete:** the sidebar and context-menu Delete open the Delete wizard (`actions.rs`: `DeletePartition` → `Effect::Wizard`). Delete All still queues its batch directly.
     - App tests that only needed a queued deletion now use `Cmd::Push`.
     - `selection_follows_deleted_partition_to_its_disk` goes through the wizard: open, then `FinishWizard`.
     - `wizard_actions_open_their_wizard` now includes Delete.
   - **Dead dialogs:** removed `Dialog::WipeMethod`, `Dialog::MountLetter`, `Dialog::Unmount`, their rendering, `wipe_method`, the duplicate `mount_point_of` and its test from `dialogs/mod.rs`, plus the now-unused imports in `app.rs` and `dialogs/mod.rs`.
   - **Change Label refusal:** `LabelLetterState::step_problem(model)` adds the queue check, so the executor's SetLabel refusal shows on the first step and Next is disabled. Letter mode is unaffected. Asserted in `label_letter::tests::build_label_and_letter_ops`.
   - **Wipe and Back:**
     - New `WizardStep::on_leave(forward)` (default does nothing). `FnStep` gained `FnStep::new` and `with_on_leave`, and `Wizard::back`/`next` call the hook.
     - Wipe's Confirm step clears the typed WIPE when the user goes Back. Going back from the summary page to Confirm keeps it, because it is still on screen.
     - Test: `wipe::tests::going_back_from_the_confirmation_clears_it`.

**Verification.**
- Commit `82676c1` alone, in a temporary detached worktree: `cargo test -p pm-gui` and `cargo build -p pm-gui`. The count is 78 + 3 new − 1 removed (`dialogs::tests::mount_point_strings`). The build finished with zero warnings.
  ```
  test result: ok. 80 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.78s
  test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
  test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
  ```
- The shared working tree: earlier, with Task 20's in-progress treemap included, it reported `test result: ok. 82 passed; 0 failed`. At the end of this round it did not compile, because Task 20 had declared `tools::{benchmark,smart,space_analyzer,surface_test}` before creating those files. That breakage is theirs, not this commit's.
- **Note for Task 20:**
  - `FnStep` has a new field, `on_leave`. Build it with `FnStep::new` (or `Wizard::over_state`) rather than a struct literal.
  - `size_edit` now takes `&mut SizeText`.
