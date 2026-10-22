# Rescue Mode

For failing drives, Partition Manager has a copier that follows the algorithm of GNU
ddrescue and writes the same mapfile format. It never writes to the source. There are three
ways in:

- `pm rescue SRC DST MAPFILE`: copy onto a device or image file, ddrescue style.
- `pm image SRC FILE --rescue` (and the `image-disk` / `image-partition` operation forms):
  export a failing disk or partition straight into a VM image.
- The GUI: the **Rescue Copy** wizard and the **Damaged disk** page of **Create Image**
  (see [[GUI Guide]]).

If a drive is failing, stop using it, and copy it once with this mode before trying
anything that writes to it or reads it repeatedly. Recovery from the copy is covered in
[[Recovery]].

## How it works

The copy proceeds in phases, each recorded in the mapfile:

1. **Copy.** Reads large blocks (128 KiB worth of sectors by default). On a read error it
   skips ahead (initially 64 KiB, doubling on further errors) and leaves the skipped area
   for later.
2. **Trim.** Reads the edges of each bad area sector by sector, from both ends, to recover
   every readable sector next to an error.
3. **Scrape.** Reads every sector of the remaining bad areas one by one.
4. **Retry.** Up to *N* further passes over the sectors that still fail, optionally in
   reverse direction.

Unreadable sectors end up as **zeros** in the destination, and the mapfile records them. Bad
sectors are a success with a warning, as with ddrescue: the command exits 0, and the log (and
the GUI report) carries the rescued size, the error size, the number of errors and the
mapfile path.

### The mapfile

The mapfile is GNU ddrescue's text format: a status line `current_pos status pass`, then one
line `pos size status` per block, where status is one of:

| Status | Meaning |
|---|---|
| `?` | not yet tried |
| `*` | failed in the copy phase, not yet trimmed |
| `/` | trimmed, not yet scraped |
| `-` | bad sector (read failed in every phase so far) |
| `+` | finished (rescued) |

It is kept up to date while copying, so you can stop at any time and any GNU ddrescue
installation can read it.

## `pm rescue`

```powershell
pm rescue disk:3 D:\rescue\copy.img D:\rescue\copy.map -r 2 -K 64K
pm rescue disk:3 D:\vm\old-pc.vmdk D:\vm\old-pc.map --format vmdk
```

Flags (letters follow GNU ddrescue):

| Flag | Meaning |
|---|---|
| `-b, --sector-size` | Sector size in bytes: the unit of trimming, scraping and retrying (default: the source's) |
| `-c, --cluster-size` | Sectors per read in the copy phase (default: 128 KiB worth) |
| `-K, --skip-size` | Initial skip after a read error (doubles on further errors) |
| `-r, --retries` | Retry passes over bad sectors (default 0) |
| `-R, --reverse` | Copy from the end of the source towards the start |
| `-n, --no-scrape` | Skip the scraping phase |
| `-N, --no-trim` | Skip the trimming phase |
| `--format` | Format of a new destination image: raw, vhd, vhd-fixed, vmdk, vmdk-flat, qcow2 |
| `--dst-offset` | Write starting at this byte offset of the destination |
| `--used-map` | Skip areas the file system marks unused (they are zero-filled in the destination) |
| `--ansi` | Redraw the status block in place with ANSI escapes even when the terminal is not detected |

The destination must be at least as large as the source. A destination path that does not
exist yet is created as an image file of the source's size (format from `--format` or the
extension). An existing destination image with `--format` is only written to resume it, so
its mapfile must exist; otherwise the command refuses. Writing to a disk or partition
destination goes through a locked write session, like the other copy commands
([[Safety Model]]).

## `pm image --rescue`

```powershell
pm image disk:3 D:\vm\old-pc.vhd --rescue -r 2            # mapfile D:\vm\old-pc.vhd.map
pm image disk:3 D:\vm\old-pc.vhd --rescue --mapfile D:\maps\old-pc.map
pm image-disk 3 D:\vm\old-pc.qcow2 --rescue --yes         # the operation form (also image-partition)
```

With `--rescue`, read errors do not stop the export: unreadable blocks are retried and then
left as zeros in the image, and a mapfile records them. The mapfile defaults to the image
path plus `.map`. The rescue-mode options are `-r/--retries`, `-K/--skip-size`,
`-c/--cluster-size`, `--no-trim`, `--no-scrape`, `--reverse` and `--mapfile`. They apply to
`image`, `image-disk` and `image-partition`. Without `--rescue` the first read error aborts
the export and no image is left behind.

### Resuming, retrying in place, starting over

- **Resume.** The image is written to `FILE.partial`. A failed or cancelled rescue export
  keeps `FILE.partial` and its mapfile, and running the same command again resumes where it
  stopped. Areas already marked `+` are not read again.
- **Retry in place.** Running the same command again on a finished image, with its mapfile
  beside it, retries the unreadable areas in place instead of exporting from scratch. Give
  more retries with `-r` to try harder.
- **Start over.** To discard a previous export and begin again, delete the mapfile (and the
  old image). The mapfile is what makes the command resume rather than start fresh.

### Sparse images

New sparse images (VHD dynamic, VMDK sparse, qcow2) allocate nothing for zero areas, and
bad blocks are written as zeros, so a rescue export of a mostly unreadable disk stays small.
Raw, fixed VHD and flat VMDK are not sparse formats, so those images are the full size of
the source. See
[[Imaging and VM Formats]].

## The GUI: the Damaged disk page

In **Create Image**, the **Damaged disk** page has a checkbox "Continue on read errors
(ddrescue mode)" and explains that unreadable blocks are retried, then written as zeros,
and that a mapfile next to the image records them so the export can be resumed. It sets the
number of retry passes (0 to 10), the initial skip size, and the trim, scrape and reverse
toggles. The mapfile path defaults to the image path plus `.map`; it must be an absolute
path, its folder must exist, and it must not be on the source drive. The page is not shown
in **Restore Image**. After the export, the report lists rescued size, error size and the
number of errors per image, in orange when there were errors.

The **Rescue Copy** wizard is the GUI form of `pm rescue`; if the destination image file
does not exist, it can be created there with a format of your choice.

## Tips

- Give the mapfile an explicit path on a healthy drive that is not the source. Check where
  the GUI proposes the default mapfile.
- Start with few retries (`-r 0` or `-r 1`) to get the readable data first; run again with
  more retries later, as every retry stresses the drive.
- The copy phase's skip size matters on drives that hang on errors; use `-K` to skip larger
  areas at first.
- Surface test (`pm analyze surface`) only reads and reports; it does not copy.
