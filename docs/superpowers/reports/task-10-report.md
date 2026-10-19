# Task 10 report: pm-image virtual disk formats

**Status:** DONE. Commit `66782ad feat(image): raw, VHD, VMDK, qcow2 virtual disks` on `dev`. Only `crates/pm-image` is staged. Cargo.lock already had the pm-image entry from a concurrent commit, so it was unchanged.

## What was implemented
- `src/lib.rs`: `CreateOpts`, `create`, `open`, `identify`, `convert`.
  - `create` returns `Error::Invalid` when the capacity is 0 or not a multiple of 512.
  - `convert` copies in 1 MiB chunks under the phase name "Converting". It checks cancel before each chunk (and before creating the destination), skips all-zero chunks when the target is sparse, and leaves a partial destination on cancel.
- `src/format.rs`:
  - `ImageFormat` derives Serialize/Deserialize.
  - `from_extension` is case-insensitive.
  - `extension()` returns the extension without the dot ("img", "vhd", "vmdk", "qcow2").
  - `display()` uses the strings given in the brief.
  - New public helper `is_sparse()`.
  - `sniff` checks in this order: KDMV, QFI\xfb, "# Disk DescriptorFile", then a "conectix" cookie in the last 512 bytes (type 2 = fixed, 3 = dynamic, anything else = Unsupported). Anything else is raw.
- `src/sparse.rs`:
  - `ImageFile`: a growable container. It wraps `FileDevice` with a `u64::MAX` window and tracks the real file length itself.
  - `BlockMap` (block_size, get/set/read/write): the allocate-on-write engine. Unallocated blocks read as zeros. A write to a new block calls `place_block`, writes the full zero-padded block, then calls `on_allocate(idx, file_off)`.
  - `BlockAllocator` trait and the generic `SparseDevice<A>` BlockDevice (len = virtual capacity, read-only enforcement, bounds checks).
- `src/raw.rs`: wraps `FileDevice`. The sparse flag is ignored, as decided.
- `src/vhd.rs`:
  - Fixed images: data followed by the footer, exposed through `SliceDevice` over `FileDevice`.
  - Dynamic images: footer copy at 0, cxsparse header at 512, BAT at 1536 padded to 512, 512-byte all-0xFF bitmap plus a 2 MiB block. The footer is re-appended after every allocation, then the BAT entry is written.
  - Geometry follows the spec algorithm, including the clamp. The checksum is the ones-complement byte sum.
  - On open, the footer and header cookies and checksums are verified.
- `src/vmdk.rs`:
  - Flat: the descriptor plus `<stem>-flat.vmdk`. Open handles a single FLAT extent resolved against the descriptor's directory, and honours RDONLY.
  - Sparse:
    - Layout: header, 20-sector descriptor, rGD, rGTs, GD, GTs, padded to 1 MiB (= overHead). Grains are appended at EOF.
    - Each GTE is written to both the redundant and the primary grain table.
    - On open, compressed or streamOptimized images and a GD stored at the end of the file are rejected.
- `src/qcow2.rs`:
  - Layout: v3 header (length 112), L1 at cluster 1, a 1-cluster refcount table, then refblock 0.
  - `set_refcount` allocates refblocks recursively as needed. L2 tables and data clusters get refcount 1 before they are referenced, and the COPIED bit is set in L1 and L2 entries.
  - On open, these are rejected: backing files, encryption, incompatible feature bits, compressed clusters, and write access to images with snapshots or non-16-bit refcounts.
- `README-formats.md`: how to check images with `qemu-img info/check/compare`, the VHD force_size_calc caveat, Attach VHD in Disk Management and Mount-DiskImage, VirtualBox `showmediuminfo`, and what is not supported.

## Tests (RED then GREEN)
- **RED:** API stubs that return `Unsupported("todo")`. `cargo test -p pm-image` gave `0 passed; 15 failed`, with every test panicking on `Unsupported("todo")`.
- **GREEN:** `cargo test -p pm-image` gives 5 unit tests and 15 integration tests passed, 0 failed. `cargo build -p pm-image` and the test build both have 0 warnings. Clippy is not installed for the GNU toolchain, so it was not run.
- **Integration tests (`tests/formats.rs`):**
  - `create_write_reopen_read_{raw,vhd_fixed,vhd_dynamic,vmdk_flat,vmdk_sparse,qcow2}`:
    - Capacity is 37 MiB + 2.5 KiB, which is not aligned to any block size.
    - Writes go at 0, at 2 MiB+7 (200 KB), at end-4 KiB, and across a 2 MiB boundary.
    - The full device is compared against a MemDevice reference before close, after a read-only reopen, and after a read-write reopen that appends more writes.
    - Unwritten areas read as zero, out-of-range access errors, and a read-only device rejects writes.
  - `vhd_footer_checksum_valid`:
    - Every footer field and the checksum are checked for fixed and dynamic images.
    - CHS for 40 MiB is 963/5/17. A 1000 GiB disk clamps to 65535/16/255.
    - The footer at EOF equals the copy at 0.
    - All dynamic header fields and the header checksum are checked, and the BAT is all 0xFFFFFFFF.
  - `vhd_dynamic_bat_grows`:
    - The file grows by exactly 512 + 2 MiB per block.
    - BAT sector values are exact, the bitmap is all 0xFF, data and zero padding sit in place, untouched blocks stay 0xFFFFFFFF, and the footer copies are equal.
  - `vmdk_sparse_header_fields`:
    - All header fields, the newline characters, compress=0 and the zero padding are checked.
    - The descriptor contains createType, the RW line, CID and the ddb lines.
    - GD and rGD entries are present, the redundant GTE mirrors the primary, data lands at the GTE sector, the other GTEs are 0, and the file size is exactly overHead plus 1 grain.
  - `vmdk_flat_descriptor_text`: checks the descriptor text, the RW FLAT line, the flat file size and content, and open through the descriptor.
  - `qcow2_header_fields_and_refcounts_consistent` (1.5 GiB image, 3 L1 entries in use):
    - Checks all header fields.
    - Walks the refcount table. Every cluster referenced by the header, L1, refcount table, refblocks, L2 tables and data has refcount exactly 1.
    - No cluster is used twice, there are no leaked refcounts, nothing points beyond EOF, and every cluster in the file is in use.
    - COPIED bits are set, and the guest data is at the host offsets the tables give.
  - `identify_each_format`: identify works for all 6 formats. A tiny file or junk is raw, and a missing file errors. It also checks `from_extension`, `display`, `extension` and a serde round trip.
  - `convert_raw_to_each_and_back_equal`: raw goes to each format and back to raw, byte-equal. Sparse outputs stay under 12 MiB for a 24 MiB disk holding about 4 MiB of data.
  - Extra tests: `create_rejects_bad_capacity` and `convert_honours_cancel`.
- **Unit tests:** BlockMap engine, VHD geometry (40 MiB, 4 GiB = 8322/16/63, 127 GiB, clamp), checksum, VMDK layout, extent-line parsing.

## Files
All new: `crates/pm-image/{Cargo.toml, README-formats.md, src/{lib,format,raw,sparse,vhd,vmdk,qcow2}.rs, tests/formats.rs}`.

## Self-review against the pinned constants
- **VHD:** every footer and dynamic header constant listed in the brief is checked (and asserted in tests). Data offset is u64::MAX for fixed and 512 for dynamic. Block size is 0x200000. The BAT is in big-endian sectors, uses 0xFFFFFFFF for unallocated and is padded to 512. The bitmap is ceil(4096/8) = 512 bytes, all bits set. The footer is rewritten after every allocation.
- **VMDK sparse:** checked KDMV LE, v1, flags 3, capacity in sectors, grainSize 128, descriptorOffset 1, descriptorSize 20, numGTEsPerGT 512, rgd/gd, overHead aligned to 1 MiB, uncleanShutdown 0, the 4 line-end characters, compress 0 and 433 bytes of padding. The descriptor text matches the template.
- **VMDK flat:** checked `createType="monolithicFlat"` and `RW <n> FLAT "<name>-flat.vmdk" 0`.
- **qcow2:** checked magic, v3, backing 0/0, cluster_bits 16, size, crypt 0, l1_size = ceil(size/512 MiB), L1 at cluster 1, refcount table at the next cluster (1 cluster), features 0, refcount_order 4, header_length 112, refcount 1 everywhere, COPIED bits.
- **`from_extension`, `display`, sniff order, `Error::Invalid` for unaligned capacity, convert behaviour:** all as specified.
- **Deliberate deviations:**
  1. BlockMap stores a paged `u64` table (0 = unallocated) instead of `Vec<Option<u64>>`. The API is the same, but a near-empty multi-TB image costs about 0 RAM instead of 16 bytes per block.
  2. `ImageFile` widens `FileDevice`'s window to let the container grow, because `FileDevice` has a fixed length.
  3. New public `ImageFormat::is_sparse()`.
  4. Size limits return `Unsupported`: VHD above 2040 GiB (spec limit), sparse VMDK above about 2 TiB (u32 sector GTEs), and qcow2 created above 16 TiB (1-cluster refcount table).

## Concerns
1. **qemu and VHD size.** Because the creator app is "pman", qemu's vpc driver computes the size from CHS, not `current_size`, so qemu sees the disk a few KiB smaller than its capacity (for 40 MiB, it loses about 35 KiB). Windows, Hyper-V and VirtualBox use `current_size`. The README documents `force_size_calc=current_size`. If qemu interop must work out of the box, the creator would have to be one qemu trusts, such as "win " or "qem2"; that is a decision for the controller.
2. **VMDK sparse costs on large disks.** Open reads every grain table up front (128 MiB of grain-table reads for a 2 TiB disk). Create writes zeros over the whole metadata area (about 257 MiB for a 2 TiB disk). Neither honours cancel or progress, because `create`/`open` have no such parameters.
3. **No crash consistency.** There are no dirty or unclean-shutdown flags. Metadata is written after data, so the worst case is a leaked block, not corruption.
4. **qcow2 preallocated zero clusters.** Clusters flagged "all zeroes" but still holding an offset (from qemu images) are treated as unallocated. Writing to one leaks the old cluster, which `qemu-img check` reports as a leak, not an error.
5. **Not checked with external tools.** None of the images were run through qemu-img, VirtualBox, VMware or Hyper-V, because those tools are not installed. Commands to do so are in README-formats.md.

## Fix round 1

**Commit:** `fe71659 fix(image): bound header-derived sizes, VHD footer-first allocation, win creator, clear qcow2 autoclear`.

I staged and committed only these six files: README-formats.md, src/{qcow2,sparse,vhd,vmdk}.rs and tests/formats.rs. `lib.rs` and `Cargo.toml`, which the other agent is editing, were not touched.

### What changed
1. **VHD creator application is now `"win "`.** Changed in `vhd.rs` `build_footer`. The `check_vhd_footer` test now asserts `b"win "`. The README caveat is replaced: it now explains that qemu reads `current_size` for `win ` and keeps `force_size_calc` only as a tip for VHDs made by other tools. Concern 1 above is resolved.
2. **Corrupt headers can no longer panic or trigger huge allocations.** Each limit below returns `Error::Parse`.
   - **sparse.rs:** `BlockMap` pages now live in a `BTreeMap`. Its memory used to scale with the block count read from the header; it now scales with allocated data only. This was the 281 TB allocation abort that the RED test hit.
   - **qcow2:**
     - l1_size ≤ 32 MiB/8 entries.
     - refcount_table_clusters is between 1 and 8 MiB/cluster size.
     - The L1 table, refcount table and every L2 table must be cluster-aligned and lie inside the file (checked with `checked_add`) before anything is allocated.
     - Every non-zero refcount-table entry must be cluster-aligned and inside the file. Without this, `rt[i] + n` could overflow during a later write.
   - **VHD dynamic:**
     - The block size must be a power of two, from 512 bytes to 256 MiB.
     - max_entries ≤ 2 TiB / block size. This bounds the coverage multiplication.
     - The BAT range must lie inside the file (checked) before it is allocated.
   - **VMDK sparse:**
     - The grain must be a power of two of at most 0x200000 sectors. This also rules out zero, so there is no division by zero.
     - numGTEsPerGT must be between 1 and 512.
     - The capacity is computed with `checked_mul(512)`.
     - `read_directory` checks offset × 512 and n × 4 with checked math and requires the range to lie inside the file before allocating. This covers both the GD and the redundant GD.
   - **VMDK flat:**
     - The descriptor file must be at most 1 MiB.
     - The extent's offset × 512, sectors × 512 and offset + len are all checked.
3. **VHD allocation order is now footer, bitmap, data, BAT entry.** `place_block` first writes the footer at `start + bitmap + block`, which extends the file. It then writes the bitmap over the old footer position and returns. The engine writes the data, and `on_allocate` writes the BAT entry last. The file therefore ends in a valid footer at every step; a crash only leaks space.
4. **qcow2 read-write open clears `autoclear_features`.** When a version 3 image is opened read-write and the field at offset 88 is non-zero, it is zeroed and flushed. A read-only open leaves it untouched.

### Covering tests (RED, then GREEN)
- **`corrupt_headers_never_panic`:** builds a 4 MiB image of each format and patches it, recomputing VHD checksums where needed. Every case must make `open` fail cleanly, both read-only and read-write.
  - qcow2 (10 cases): l1_size huge or over 32 MiB, refcount table clusters huge or over 8 MiB, L1 and refcount-table offsets near u64::MAX, cluster_bits 0 or 63, size u64::MAX, an L1 entry beyond EOF.
  - VHD dynamic (9 cases): block size 0, 3 MiB, 2 GiB or 512 MiB; max entries u32::MAX; table offset huge; footer data offset huge; current size huge; a BAT entry beyond EOF.
  - VHD fixed (1 case): current size huge.
  - VMDK sparse (9 cases): grain 0, 100, u64::MAX or 0x400000; numGTEsPerGT 0 or u32::MAX; capacity u64::MAX; gdOffset or rgdOffset at u64::MAX/256.
  - VMDK flat (3 cases): descriptors whose extent numbers overflow.
  - The test runs in well under a second.
  - **RED:** the process aborted with `memory allocation of 281474976645120 bytes failed`.
- **`vhd_dynamic_footer_valid_after_each_allocation`:** writes to four new blocks, reopening the image each time. After each one it checks that the EOF footer is valid and equals the copy at offset 0, that `identify` still says VhdDynamic, and that the whole device matches a MemDevice reference.
- **Unit test `vhd::tests::place_block_keeps_a_footer_at_eof`:** checks the state right after `place_block`, before the data or BAT entry are written. The file must end in a valid footer, the old footer position must hold an all-0xFF bitmap, and the BAT entry must still be 0xFFFFFFFF. After `on_allocate` the BAT entry is set and the footer is still valid. This is the test that actually pins the ordering. The public-API test above would also pass with the old order.
- **`qcow2_autoclear_features_cleared_on_write`:** sets offset 88 to 1. A read-only open and read leave it at 1. A read-write open plus a one-sector write leaves it at 0, and the data reads back.
- **RED for the other new tests:** `vhd_footer_checksum_valid`, `vhd_dynamic_bat_grows` and `vhd_dynamic_footer_valid_after_each_allocation` failed on `pman` vs `win `. `qcow2_autoclear_features_cleared_on_write` failed because offset 88 was still 1.

### Commands and output
- `cargo test -p pm-image --test formats`: `test result: ok. 18 passed; 0 failed`.
- `cargo test -p pm-image`: the whole crate passes, including the other agent's uncommitted rescue files, which compile in the tree.
  - Unit tests: 9 passed.
  - formats: 18 passed.
  - The other agent's two test binaries: 7 and 14 passed.
  - Doc tests: 0.
- `cargo build -p pm-image`: 0 warnings.

### Still open
- **Convert self-overwrite guard:** needs `lib.rs`, so it is deferred to a later round as instructed.
- **Remaining concerns:** items 2 to 5 above are unchanged.

## Fix round 2

**Commit:** `2f6beca fix(image): refuse convert onto its own source (incl. flat VMDK extents)`. It is based on dev head 8393197, and only `src/lib.rs`, `src/vmdk.rs` and `tests/formats.rs` were committed.

### What changed
- **`lib.rs` `convert`:** after the cancel check, it calls `refuse_same_file(src, dst, fmt)` before opening or creating anything.
  - **Files read:** `src` itself and, when `src` is a flat VMDK descriptor, its extent.
  - **Files written:** `dst` itself and, when `fmt` is VmdkFlat, `dst`'s `<stem>-flat.vmdk` sibling.
  - Each path goes through `file_identity`. That is `std::fs::canonicalize` if the file exists; otherwise the canonical parent directory joined with the file name, because a sibling may not exist yet. Paths are compared case-insensitively on Windows.
  - Any overlap returns `Error::Invalid("destination is the same file as the source")`.
  - The doc comment now says that existing destinations are overwritten and only same-file cases are refused.
- **`vmdk.rs` refactor (no behaviour change):**
  - `flat_sibling(path)`: the `<stem>-flat.vmdk` naming. `create_flat` now uses it too, so create and the guard always agree on the name.
  - `read_flat_descriptor(path)`: returns the single extent and its resolved path. `open_flat` and the new `flat_extent_path(path)` both use it.

### Covering test: `convert_refuses_same_file`
- **Refused cases:**
  - raw onto itself;
  - the same file spelled `sub/../src.img` (converting to qcow2);
  - `vm-flat.vmdk` (read as raw) into a VmdkFlat destination `vm.vmdk`, whose sibling is the source;
  - the descriptor `vm.vmdk` into raw `vm-flat.vmdk`, which is the source's own extent.
- **Source untouched:** after these cases, the source, descriptor and extent bytes are all unchanged.
- **Overwrite still works:** a conversion to a distinct, already-existing `sub/out.vmdk` succeeds, overwrites it, and its data reads back.
- **RED:** `raw onto itself: expected Invalid, got Ok(())`, meaning the source had been truncated.
- **GREEN:** `cargo test -p pm-image` passes:
  - Unit tests: 9 passed.
  - formats: 19 passed.
  - rescue: 7 passed.
  - mapfile: 14 passed.
  - `cargo build -p pm-image`: 0 warnings.

### Limitation
Hard links, and two different paths to the same file through junctions or other links that `canonicalize` does not resolve, are not detected. Detecting them would need a comparison of file IDs, which needs OS APIs. This is out of scope here.

## Fix round 3

**Commit:** `25ebd3e fix(image): fail-closed path identity for the convert same-file guard`. Only `src/lib.rs` and `tests/formats.rs` were committed.

### What changed
- **`file_identity` now returns `Result<PathBuf>`:**
  - A relative path is first made absolute against the current directory.
  - It walks the components from the full path downwards and finds the longest prefix whose `try_exists()` is not `Ok(false)`, then canonicalizes that prefix.
  - The remaining components are applied lexically: `.` is dropped, `..` pops (also past the canonical ancestor; popping at the root is a no-op, like Windows), and names are pushed.
  - It fails closed with `Error::Invalid("cannot resolve path ...")` in four cases: a prefix that exists (or whose existence cannot be determined) fails to canonicalize, a root or prefix component appears in the tail, `current_dir` fails, or nothing exists. It no longer falls back to the raw spelling.
- **`refuse_same_file`:** computes the destination identities first (plus the VmdkFlat sibling). It then compares the `src` path before reading it, and only after that sniffs `src` and compares its flat extent. The source path is checked even when `src` cannot be read on this platform.
- **`convert` doc comment:** now has the one-line note that hard links are a known limitation.

### Note on RED
The reviewer's exact probe, `convert(src, dir/no_such_dir/../src.img)`, was already refused before this change. Rust's `std::fs::canonicalize` on Windows goes through GetFullPathNameW, which resolves `..` lexically, so `canonicalize(dst)` succeeded and matched the source.
- **The new regression test passes both before and after.** It is kept as a guard against regressions.
- **The change is still defence in depth.** It removes the raw-spelling fallback and makes the resolution independent of how the platform's `canonicalize` treats `..`.
- **New fail-closed `Result` API:** the new unit test only compiles against it, so it was written against the new code (compile-RED rather than a behavioural RED).

### Covering tests
- **`convert_refuses_same_file_through_missing_dir` (integration).** Each spelling is tried for both Raw and Qcow2 targets, and every call must return `Invalid("destination is the same file as the source")` with the source bytes unchanged. Afterwards no `no_such_dir` or `a` directory may have been created.
  - `dst = dir/no_such_dir/../src.img`.
  - The reverse, `src = dir/no_such_dir/../src.img` with `dst = src.img`.
  - `dst = dir/a/b/../.././src.img`.
- **`tests::file_identity_resolves_missing_ancestors_lexically` (unit, in lib.rs).**
  - An existing file resolves to its canonical path.
  - `a/b/.././c` under a missing `a` resolves to `<canon>/a/c`.
  - `missing/../f.img` resolves to `<canon>/f.img`.
  - `m/../../z` resolves to `<canon parent>/z`, i.e. `..` pops past the existing ancestor.
  - A relative `Cargo.toml` resolves to `canonicalize("Cargo.toml")`.
- **`convert_refuses_same_file` and all earlier tests:** still green.

### Command and output
`cargo test -p pm-image`:
```
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (unit)
test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (formats)
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out    (mapfile)
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out   (rescue)
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out    (doc)
```
`cargo build -p pm-image`: 0 warnings.

### Residual
Hard links and aliases that canonicalization cannot see remain undetected. This is accepted and documented in the `convert` doc comment.
