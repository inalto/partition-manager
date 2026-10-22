# Recovery

Partition Manager can find partitions that a damaged partition table no longer lists,
list and restore deleted files, and carve files out of raw data by their content. All of it
is read-only on the scanned device; the only write is adding recovered partitions back to a
partition table, and that needs an explicit confirmation. If the media is failing, copy it
first with [[Rescue Mode]] and recover from the copy.

In the GUI these are the **Partition Recovery** and **Data Recovery** wizards ([[GUI Guide]]);
on the command line they are `pm recover ...` ([[CLI Reference]]).

## Partition recovery

Searches a disk or image for partitions the table no longer describes.

- **Quick scan** (`--quick`) looks at the backup GPT and the free space only.
- **Full scan** (the default) looks through the whole disk, using file-system boot sectors
  and backup GPT headers to propose candidates.
- Every candidate has a **confidence** from 0 to 100, a start and length, a file system and
  a label where known, and a note about overlaps.

```powershell
pm recover partitions disk:3 --quick --session scan.json      # look, and save the scan
pm recover partitions disk:3 --apply --yes --min-confidence 80
pm recover partitions disk:3 --apply --select 1,3 --yes       # only candidates 1 and 3
```

`--apply` adds the found partitions to the partition table; without `--yes` nothing is
written. `--min-confidence` and `--select` narrow the candidates and only take effect with
`--apply`. `--table gpt|mbr` chooses the table to create when the disk has none (default
GPT). Writing the table of the running system disk needs `--allow-system-disk`.

## Deleted files

Lists the deleted files of a file system and restores them to a destination folder.

- **File systems:** NTFS (records in the master file table that are no longer in use),
  FAT12/16/32 and exFAT (directory entries marked deleted), and ext2/3/4 (inodes with a
  deletion time and no links). ext keeps no name for a deleted inode, so those entries are
  listed as `inode-N`; a carve is the better route when you need names on ext. Other file
  systems can be browsed and extracted from while intact (`pm ls`, `pm cat`, `pm extract`)
  but are not supported for deleted-file recovery.
- **Integrity estimate** (0 to 100) for each file: on NTFS, whether its clusters have been
  reused by other data; on FAT, whether the cluster chain is plausible. FAT does not keep
  the chain of a deleted file, so recovery is a heuristic and works best on unfragmented
  files. A deleted FAT entry also loses the first character of its short name.
- Restore only files likely to be intact with `--min-integrity`.

```powershell
pm recover files disk:3:1                                # list
pm recover files disk:3:1 --dest C:\restore --min-integrity 50
pm recover files disk:3:1 --session deleted.json         # save the list
```

Always restore to a different disk than the one you are recovering from. The GUI checks
the destination against the scanned device by drive letter only.

## Carving

Finds files by their content signatures in raw data, regardless of the file system.

```powershell
pm recover carve disk:3 --types image,document --dest C:\carved --sector 512
pm recover carve disk:3 --session carve.json             # list only, save the result
```

- **Categories** (`--types`, comma separated): `image`, `document`, `archive`, `audio`,
  `video`, `executable`, `database`, `other`. The default is all of them. The design covers
  the common signatures: JPEG, PNG, GIF, BMP, TIFF, PDF, ZIP-based formats (docx, xlsx,
  pptx, odt, jar, apk), 7z, RAR, GZIP, MP3, MP4/MOV, AVI/WAV, MKV, OLE2 documents, SQLite,
  EXE/DLL, ELF, TTF, PSD and text heuristics for XML and HTML.
- **`--sector`** is the granularity at which file starts are looked for (default 512; `1`
  looks everywhere and is slow).
- **Extraction** happens only with `--dest` (or `--extract` with `--dest`); otherwise the
  scan just lists what it found.
- **Read errors.** Carving continues past unreadable sectors and counts them. A file that
  spans a bad sector may come out incomplete, and the unreadable count can be overstated.
  Extraction itself is not fully tolerant of read errors. Carve from a [[Rescue Mode]] copy
  of a failing disk instead.
- Very short signatures (such as BMP and EXE headers) can produce many false hits. Carved
  files have generated names, not their original names or folders.

## Sessions and regions

A scan can be saved with `--session FILE` (partitions, files and carve) and reopened later
without scanning again:

```powershell
pm recover open-session scan.json
```

A session is a JSON file holding the scan parameters, the identity of the device (model,
serial, size, table signature), the region that was scanned and the entries found. In the
GUI, Data Recovery can save and reopen sessions the same way.

The scanned **region** matters: a carve over a partition covers only that partition's
bytes. Scan sessions saved before the region was recorded load as whole-device sessions, so
a carve session saved by an older `pm recover carve` on a *partition* restores the wrong
bytes. Delete such a session and scan again. Sessions have no version field yet.
