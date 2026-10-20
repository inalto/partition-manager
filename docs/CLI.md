# `pm` command reference

Generated from the clap definitions by `scripts/gen-cli-docs.sh`; the output of
`pm <command> --help` is authoritative. Regenerate this file after changing the CLI.

**Device arguments.** Commands that take a device accept `disk:N` (physical disk N),
`disk:N:P` (partition P of disk N, 1-based), a drive letter (`D:`), a `\\?\Volume{..}\`
path, or an image file (`file:PATH`, `file:PATH:P`, or a bare path). With the global
`--disk-file PATH` option (repeatable) the image files become `disk:0`, `disk:1`, ... and no
administrator rights are needed.

**Sizes** accept binary suffixes: `4096`, `64K`, `1M`, `1.5G`.

**Exit codes.** 0 success, 1 usage error or invalid argument, 2 platform or runtime error.

**Writing commands.** The operation commands (`init-disk`, `create`, `delete`, `format`,
`resize`, ...) and `apply` show the operation and validate it against the current disks, and
write nothing unless `--yes` is given; `--dry-run` validates only; `--emit-json` prints the
operation as JSON for `pm apply`. `clone`, `restore`, `image`, `convert` and `rescue` copy
immediately when run, after refusing the unsafe combinations listed in their help (source
equals destination, the running system disk, overlapping targets).

## pm

```text
Partition Manager command line

Usage: pm.exe [OPTIONS] <COMMAND>

Commands:
  list             List disks and their partitions
  table            Show the partition table of a disk, volume or image file
  mount            Mount a volume at a drive letter or an empty folder
  unmount          Remove a drive letter or folder mount point from a volume
  doctor           Print environment diagnostics (elevation, toolchain, disks seen)
  clone            Copy one device onto another (disk, partition, volume or image file)
  image            Save a device to an image file (raw, VHD, VMDK or qcow2)
  restore          Write an image (or any device) onto a device
  convert          Convert an image file to another format
  rescue           Copy a failing device with ddrescue's algorithm, keeping a resumable mapfile
  probe            Identify the filesystem on a device
  ls               List a directory of the filesystem on a device
  cat              Write a file of the filesystem on a device to standard output
  extract          Copy a file or directory tree out of the filesystem on a device
  apply            Apply the operations listed in a JSON file (see `--emit-json` of the operation commands)
  init-disk        Write an empty partition table to a disk that has no partitions
  create           Create a partition in free space (and format it when --fs is given)
  delete           Delete a partition
  format           Format a partition
  resize           Resize and/or move a partition
  set-active       Mark an MBR partition active (bootable)
  set-label        Change the volume label of a partition
  set-letter       Assign a drive letter to a partition (`none` removes it)
  set-type         Change the type of a partition
  wipe-disk        Overwrite a whole disk (destroys everything on it)
  wipe-partition   Overwrite one partition (destroys its data)
  convert-table    Convert a disk's partition table between MBR and GPT (`pm convert` converts image files)
  align            Align every partition to 1 MiB
  rebuild-mbr      Rewrite the MBR (or protective MBR and GPT copies) from the partition table
  clone-disk       Copy a whole disk onto another, laying out the partitions on the target (overwrites it)
  migrate-os       Copy the Windows system partitions (or all partitions) onto another disk
  image-disk       Save a whole disk to an image file
  image-partition  Save one partition to an image file
  restore-image    Write an image file onto a disk (overwrites)
  recover          Find lost partitions, deleted files and carve files out of raw data
  analyze          Space usage, surface test, benchmark and SMART health
  help             Print this message or the help of the given subcommand(s)

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
  -h, --help              Print help
  -V, --version           Print version
```

## pm list

```text
List disks and their partitions

Usage: pm.exe list [OPTIONS]

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --json              Print the disks as JSON
  -h, --help              Print help
  -V, --version           Print version
```

## pm table

```text
Show the partition table of a disk, volume or image file

Usage: pm.exe table [OPTIONS] <DEVICE>

Arguments:
  <DEVICE>  `disk:N`, a drive letter (`D:`), a `\\?\Volume{..}\` path, or an image file (`file:PATH` or PATH)

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --json              Print the table as JSON
  -h, --help              Print help
  -V, --version           Print version
```

## pm mount

```text
Mount a volume at a drive letter or an empty folder

Usage: pm.exe mount [OPTIONS] <--letter <LETTER>|--folder <FOLDER>> <VOLUME>

Arguments:
  <VOLUME>  Drive letter (`D:`) or `\\?\Volume{..}\` path

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --letter <LETTER>   Drive letter, e.g. `E` or `E:`
      --folder <FOLDER>   Empty NTFS folder to use as the mount point
  -h, --help              Print help
  -V, --version           Print version
```

## pm unmount

```text
Remove a drive letter or folder mount point from a volume

Usage: pm.exe unmount [OPTIONS] <--letter <LETTER>|--folder <FOLDER>> <VOLUME>

Arguments:
  <VOLUME>  Drive letter (`D:`) or `\\?\Volume{..}\` path

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --letter <LETTER>   Drive letter, e.g. `E` or `E:`
      --folder <FOLDER>   Empty NTFS folder to use as the mount point
  -h, --help              Print help
  -V, --version           Print version
```

## pm doctor

```text
Print environment diagnostics (elevation, toolchain, disks seen)

Usage: pm.exe doctor [OPTIONS]

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
  -h, --help              Print help
  -V, --version           Print version
```

## pm clone

```text
Copy one device onto another (disk, partition, volume or image file)

Refuses to write onto the source, onto a partition or volume overlapping it, onto the running system disk, or onto a VMDK descriptor's extent file. Limitation: other ways of reaching the same storage (hard links, junctions, a file stored on the disk being copied, a differencing image's parent) are not detected.

Usage: pm.exe clone [OPTIONS] <SRC> <DST>

Arguments:
  <SRC>
          Source device: `disk:N[:P]`, a drive letter, or an image file (`file:PATH[:P]`)

  <DST>
          Destination device; must be at least as large as the source

Options:
      --disk-file <PATH>
          Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files

      --smart
          Copy only the blocks the filesystem marks used

      --sector
          Copy every sector

      --dst-offset <DST_OFFSET>
          Write the copy starting at this byte offset of the destination (e.g. `1M`)
          
          [default: 0]

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

## pm image

```text
Save a device to an image file (raw, VHD, VMDK or qcow2)

The image is written to FILE.partial and renamed when complete; a failed run leaves an existing FILE untouched. Refuses to overwrite the source or a VMDK extent it is read from. Limitation: other ways of reaching the same storage (hard links, junctions, a differencing image's parent) are not detected.

With --rescue a failing source is imaged anyway (GNU ddrescue's algorithm): unreadable blocks are retried, then left as zeros in the image, and FILE.map (or --mapfile) records them; an interrupted run resumes when the same command is run again. A `disk:N[:P]` source runs the `image-disk` / `image-partition` operation.

Usage: pm.exe image [OPTIONS] <SRC> <FILE>

Arguments:
  <SRC>
          Source device: `disk:N[:P]`, a drive letter, or an image file

  <FILE>
          Image file to create (an existing file is overwritten)

Options:
      --disk-file <PATH>
          Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files

      --format <FORMAT>
          Image format; by default chosen from the file extension

      --smart
          Copy only the blocks the filesystem marks used

      --sector
          Copy every sector

      --rescue
          Continue on read errors (ddrescue mode): unreadable blocks are retried, then written as zeros; a mapfile records them so the export can be resumed

  -r, --retries <N>
          Rescue mode: retry passes over the bad sectors

  -K, --skip-size <SIZE>
          Rescue mode: initial skip after a read error (doubles on further errors; default 64K)

  -c, --cluster-size <SECTORS>
          Rescue mode: sectors per read in the copy phase (default: 128 KiB worth)

      --no-trim
          Rescue mode: skip the trimming phase

      --no-scrape
          Rescue mode: skip the scraping phase

      --reverse
          Rescue mode: copy from the end of the source towards the start

      --mapfile <PATH>
          Rescue mode: the ddrescue mapfile (default: the image path plus `.map`)

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

## pm restore

```text
Write an image (or any device) onto a device

Refuses to write onto the source, onto a partition or volume overlapping it, onto the running system disk, or onto a VMDK descriptor's extent file. Limitation: other ways of reaching the same storage (hard links, junctions, a file stored on the disk being copied, a differencing image's parent) are not detected.

Usage: pm.exe restore [OPTIONS] <FILE> <DST>

Arguments:
  <FILE>
          Image file (`file:PATH[:P]` or PATH) or other source device

  <DST>
          Destination device; must be at least as large as the image

Options:
      --disk-file <PATH>
          Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files

      --dst-offset <DST_OFFSET>
          Write starting at this byte offset of the destination (e.g. `1M`)
          
          [default: 0]

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

## pm convert

```text
Convert an image file to another format

The output is written to OUTPUT.partial and renamed when complete. Refuses to overwrite the input or its VMDK extent file.

Usage: pm.exe convert [OPTIONS] <INPUT> <OUTPUT>

Arguments:
  <INPUT>
          Existing image file (any supported format)

  <OUTPUT>
          Image file to create (an existing file is overwritten)

Options:
      --disk-file <PATH>
          Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files

      --format <FORMAT>
          Target format; by default chosen from the output extension

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

## pm rescue

```text
Copy a failing device with ddrescue's algorithm, keeping a resumable mapfile

Short options follow GNU ddrescue: -b sector size, -c sectors per read, -K skip size, -r retry passes, -R reverse, -n no scrape, -N no trim.

Usage: pm.exe rescue [OPTIONS] <SRC> <DST> <MAPFILE>

Arguments:
  <SRC>
          Source device: `disk:N[:P]`, a drive letter, or an image file

  <DST>
          Destination device; must be at least as large as the source. A path that does not exist yet is created as an image file of the source's size (format from --format or the extension)

  <MAPFILE>
          Mapfile (ddrescue format): resumes from it when it exists, kept up to date while copying

Options:
      --disk-file <PATH>
          Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files

      --format <FORMAT>
          Format of a new destination image (raw, vhd, vhd-fixed, vmdk, vmdk-flat, qcow2). With --format an existing destination is only written to resume it: its mapfile must exist

  -b, --sector-size <SECTOR_SIZE>
          Sector size in bytes: the unit of trimming, scraping and retrying (default: the source's)

  -c, --cluster-size <CLUSTER_SIZE>
          Sectors per read in the copy phase (default: 128 KiB worth)

  -K, --skip-size <SKIP_SIZE>
          Initial skip after a read error (doubles on further errors)

  -r, --retries <RETRIES>
          Retry passes over bad sectors
          
          [default: 0]

  -R, --reverse
          Copy from the end of the source towards the start

  -n, --no-scrape
          Skip the scraping phase

  -N, --no-trim
          Skip the trimming phase

      --dst-offset <DST_OFFSET>
          Write starting at this byte offset of the destination
          
          [default: 0]

      --used-map
          Skip areas the filesystem marks unused (they are zero-filled in the destination)

      --ansi
          Redraw the status block in place with ANSI escapes even when the terminal is not detected

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

## pm probe

```text
Identify the filesystem on a device

Usage: pm.exe probe [OPTIONS] <DEVICE>

Arguments:
  <DEVICE>  `disk:N[:P]`, a drive letter, or an image file (`file:PATH[:P]`)

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --offset <OFFSET>   Look for the filesystem at this byte offset of the device [default: 0]
  -h, --help              Print help
  -V, --version           Print version
```

## pm ls

```text
List a directory of the filesystem on a device

Usage: pm.exe ls [OPTIONS] <DEVICE> [PATH]

Arguments:
  <DEVICE>  `disk:N[:P]`, a drive letter, or an image file (`file:PATH[:P]`)
  [PATH]    Directory or file, `/`-separated (default: the root)

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
  -h, --help              Print help
  -V, --version           Print version
```

## pm cat

```text
Write a file of the filesystem on a device to standard output

Usage: pm.exe cat [OPTIONS] <DEVICE> <PATH>

Arguments:
  <DEVICE>  `disk:N[:P]`, a drive letter, or an image file (`file:PATH[:P]`)
  <PATH>    File path, `/`-separated

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
  -h, --help              Print help
  -V, --version           Print version
```

## pm extract

```text
Copy a file or directory tree out of the filesystem on a device

Usage: pm.exe extract [OPTIONS] <DEVICE> <PATH> <DEST>

Arguments:
  <DEVICE>  `disk:N[:P]`, a drive letter, or an image file (`file:PATH[:P]`)
  <PATH>    File or directory path, `/`-separated (`/` extracts everything)
  <DEST>    Existing or new directory to extract into

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
  -h, --help              Print help
  -V, --version           Print version
```

## pm apply

```text
Apply the operations listed in a JSON file (see `--emit-json` of the operation commands)

The operations are validated against the current disks first. Nothing is written unless --yes is given; --dry-run validates and shows what would be done.

Usage: pm.exe apply [OPTIONS] <FILE>

Arguments:
  <FILE>
          JSON file holding a list of operations (or a single operation object)

Options:
      --disk-file <PATH>
          Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files

      --dry-run
          Validate and show what would be done; write nothing

      --yes
          Apply the operations (without it nothing is written; there is no interactive prompt)

      --allow-system-disk
          Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

## pm init-disk

```text
Write an empty partition table to a disk that has no partitions

Usage: pm.exe init-disk [OPTIONS] <--gpt|--mbr> <DISK>

Arguments:
  <DISK>  Disk index (`1` or `disk:1`)

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --gpt                
      --mbr                
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm create

```text
Create a partition in free space (and format it when --fs is given)

Usage: pm.exe create [OPTIONS] --start <START> --len <LEN> <DISK>

Arguments:
  <DISK>  

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --start <START>      Start offset, e.g. `1M` (aligned to 1 MiB)
      --len <LEN>          Length, e.g. `100G`
      --fs <FS>            [possible values: ntfs, fat32, fat16, exfat, refs]
      --label <LABEL>      [default: ""]
      --letter <LETTER>    Drive letter to assign, e.g. `E`
      --name <NAME>        GPT partition name [default: ""]
      --type <PTYPE>       Partition type (default: basic data) [possible values: basic, efi, msr, recovery, linux]
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm delete

```text
Delete a partition

Usage: pm.exe delete [OPTIONS] <DISK> <PART>

Arguments:
  <DISK>  
  <PART>  Partition number as `pm table` prints it

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm format

```text
Format a partition

Usage: pm.exe format [OPTIONS] --fs <FS> <DISK> <PART>

Arguments:
  <DISK>  
  <PART>  

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --fs <FS>            [possible values: ntfs, fat32, fat16, exfat, refs]
      --label <LABEL>      [default: ""]
      --cluster <CLUSTER>  Cluster size in bytes (default: the filesystem's choice)
      --quick              Quick format (skip the full surface write)
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm resize

```text
Resize and/or move a partition

Usage: pm.exe resize [OPTIONS] --start <START> --len <LEN> <DISK> <PART>

Arguments:
  <DISK>  
  <PART>  

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --start <START>      New start offset
      --len <LEN>          New length
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm set-active

```text
Mark an MBR partition active (bootable)

Usage: pm.exe set-active [OPTIONS] <DISK> <PART>

Arguments:
  <DISK>  
  <PART>  

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm set-label

```text
Change the volume label of a partition

Usage: pm.exe set-label [OPTIONS] <DISK> <PART> <LABEL>

Arguments:
  <DISK>   
  <PART>   
  <LABEL>  

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm set-letter

```text
Assign a drive letter to a partition (`none` removes it)

Usage: pm.exe set-letter [OPTIONS] <DISK> <PART> <LETTER>

Arguments:
  <DISK>    
  <PART>    
  <LETTER>  `E`, `E:` or `none`

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm set-type

```text
Change the type of a partition

Usage: pm.exe set-type [OPTIONS] --type <PTYPE> <DISK> <PART>

Arguments:
  <DISK>  
  <PART>  

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --type <PTYPE>       [possible values: basic, efi, msr, recovery, linux]
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm wipe-disk

```text
Overwrite a whole disk (destroys everything on it)

Usage: pm.exe wipe-disk [OPTIONS] <DISK>

Arguments:
  <DISK>  

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --method <METHOD>    [default: zero] [possible values: zero, random, dod3]
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm wipe-partition

```text
Overwrite one partition (destroys its data)

Usage: pm.exe wipe-partition [OPTIONS] <DISK> <PART>

Arguments:
  <DISK>  
  <PART>  

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --method <METHOD>    [default: zero] [possible values: zero, random, dod3]
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm convert-table

```text
Convert a disk's partition table between MBR and GPT (`pm convert` converts image files)

Usage: pm.exe convert-table [OPTIONS] --to <TO> <DISK>

Arguments:
  <DISK>  

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --to <TO>            [possible values: gpt, mbr]
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm align

```text
Align every partition to 1 MiB

Usage: pm.exe align [OPTIONS] <DISK>

Arguments:
  <DISK>  

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm rebuild-mbr

```text
Rewrite the MBR (or protective MBR and GPT copies) from the partition table

Usage: pm.exe rebuild-mbr [OPTIONS] <DISK>

Arguments:
  <DISK>  

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm clone-disk

```text
Copy a whole disk onto another, laying out the partitions on the target (overwrites it)

Usage: pm.exe clone-disk [OPTIONS] <SRC> <DST>

Arguments:
  <SRC>  Source disk index
  <DST>  Target disk index

Options:
      --disk-file <PATH>     Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --sizing <SIZING>      [default: fit] [possible values: exact, fit, proportional]
      --smart                Copy only the blocks the filesystem marks used
      --sector               Copy every sector
      --boot-fix <BOOT_FIX>  [default: none] [possible values: none, uefi, bios, all]
      --table <TABLE>        Partition table of the copy (default: the same kind as the source); converting maps the partition types, an MBR copy holds at most 4 partitions within 2 TiB [possible values: gpt, mbr]
      --dry-run              Validate and show what would be done; write nothing
      --yes                  Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk    Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json            Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help                 Print help
  -V, --version              Print version
```

## pm migrate-os

```text
Copy the Windows system partitions (or all partitions) onto another disk

Usage: pm.exe migrate-os [OPTIONS] <SRC> <DST>

Arguments:
  <SRC>  
  <DST>  

Options:
      --all-partitions       Copy every partition, not only the system ones
      --disk-file <PATH>     Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --sizing <SIZING>      [default: fit] [possible values: exact, fit, proportional]
      --boot-fix <BOOT_FIX>  [default: none] [possible values: none, uefi, bios, all]
      --table <TABLE>        Partition table of the copy (default: the same kind as the source); converting maps the partition types, an MBR copy holds at most 4 partitions within 2 TiB [possible values: gpt, mbr]
      --keep-ids             Keep the source's disk and partition ids on the copy, so Windows keeps its drive letters (MountedDevices); detach the source before the copy boots, Windows takes a disk with a duplicate id offline. Needs the same table kind as the source
      --dry-run              Validate and show what would be done; write nothing
      --yes                  Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk    Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json            Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help                 Print help
  -V, --version              Print version
```

## pm image-disk

```text
Save a whole disk to an image file

With --rescue read errors do not stop the copy: unreadable blocks become zeros and a resumable mapfile records them (see `pm image --help`).

Usage: pm.exe image-disk [OPTIONS] <DISK> <PATH>

Arguments:
  <DISK>
          

  <PATH>
          Image file to create

Options:
      --disk-file <PATH>
          Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files

      --format <FORMAT>
          raw, vhd, vhd-fixed, vmdk, vmdk-flat or qcow2 (default: from the file extension)

      --smart
          Copy only the blocks the filesystem marks used

      --sector
          Copy every sector

      --rescue
          Continue on read errors (ddrescue mode): unreadable blocks are retried, then written as zeros; a mapfile records them so the export can be resumed

  -r, --retries <N>
          Rescue mode: retry passes over the bad sectors

  -K, --skip-size <SIZE>
          Rescue mode: initial skip after a read error (doubles on further errors; default 64K)

  -c, --cluster-size <SECTORS>
          Rescue mode: sectors per read in the copy phase (default: 128 KiB worth)

      --no-trim
          Rescue mode: skip the trimming phase

      --no-scrape
          Rescue mode: skip the scraping phase

      --reverse
          Rescue mode: copy from the end of the source towards the start

      --mapfile <PATH>
          Rescue mode: the ddrescue mapfile (default: the image path plus `.map`)

      --dry-run
          Validate and show what would be done; write nothing

      --yes
          Apply the operations (without it nothing is written; there is no interactive prompt)

      --allow-system-disk
          Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)

      --emit-json
          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

## pm image-partition

```text
Save one partition to an image file

With --rescue read errors do not stop the copy: unreadable blocks become zeros and a resumable mapfile records them (see `pm image --help`).

Usage: pm.exe image-partition [OPTIONS] <DISK> <PART> <PATH>

Arguments:
  <DISK>
          

  <PART>
          

  <PATH>
          

Options:
      --disk-file <PATH>
          Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files

      --format <FORMAT>
          

      --smart
          Copy only the blocks the filesystem marks used

      --sector
          Copy every sector

      --rescue
          Continue on read errors (ddrescue mode): unreadable blocks are retried, then written as zeros; a mapfile records them so the export can be resumed

  -r, --retries <N>
          Rescue mode: retry passes over the bad sectors

  -K, --skip-size <SIZE>
          Rescue mode: initial skip after a read error (doubles on further errors; default 64K)

  -c, --cluster-size <SECTORS>
          Rescue mode: sectors per read in the copy phase (default: 128 KiB worth)

      --no-trim
          Rescue mode: skip the trimming phase

      --no-scrape
          Rescue mode: skip the scraping phase

      --reverse
          Rescue mode: copy from the end of the source towards the start

      --mapfile <PATH>
          Rescue mode: the ddrescue mapfile (default: the image path plus `.map`)

      --dry-run
          Validate and show what would be done; write nothing

      --yes
          Apply the operations (without it nothing is written; there is no interactive prompt)

      --allow-system-disk
          Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)

      --emit-json
          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

## pm restore-image

```text
Write an image file onto a disk (overwrites)

Usage: pm.exe restore-image [OPTIONS] <PATH> <DISK>

Arguments:
  <PATH>  Image file
  <DISK>  

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --start <START>      Write at this offset of the disk instead of over the whole disk (a partition image)
      --dry-run            Validate and show what would be done; write nothing
      --yes                Apply the operations (without it nothing is written; there is no interactive prompt)
      --allow-system-disk  Let resize, label, drive letter, mount and creating a partition in free space touch the running system disk (destructive operations stay refused)
      --emit-json          Print the operation as JSON instead of running it (a file of such objects, in a JSON array, is what `pm apply` reads)
  -h, --help               Print help
  -V, --version            Print version
```

## pm recover

```text
Find lost partitions, deleted files and carve files out of raw data

Usage: pm.exe recover [OPTIONS] <COMMAND>

Commands:
  partitions    Search a disk or image for partitions the partition table no longer describes
  files         List the deleted files of a filesystem, and restore them with --dest
  carve         Find files by their content signatures in raw data
  open-session  Show a saved scan session
  help          Print this message or the help of the given subcommand(s)

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
  -h, --help              Print help
  -V, --version           Print version
```

### pm recover partitions

```text
Search a disk or image for partitions the partition table no longer describes

Usage: pm.exe recover partitions [OPTIONS] <DEVICE>

Arguments:
  <DEVICE>  `disk:N`, or an image file (`file:PATH` or PATH)

Options:
      --disk-file <PATH>
          Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --quick
          Look at the backup GPT and the free space only, not the whole disk
      --apply
          Add the found partitions to the partition table (needs --yes to write)
      --yes
          With --apply: actually write the table
      --min-confidence <MIN_CONFIDENCE>
          With --apply: only candidates with at least this confidence (0-100) [default: 0]
      --select <SELECT>
          With --apply: only these candidates, by the numbers in the list (e.g. `1,3`)
      --table <TABLE>
          With --apply: table to create when the disk has none [default: gpt] [possible values: gpt, mbr]
      --allow-system-disk
          With --apply: allow writing the table of the running system disk
      --session <SESSION>
          Save the scan result to this file
  -h, --help
          Print help
  -V, --version
          Print version
```

### pm recover files

```text
List the deleted files of a filesystem, and restore them with --dest

Usage: pm.exe recover files [OPTIONS] <DEVICE>

Arguments:
  <DEVICE>  `disk:N:P`, a drive letter, or an image file with a partition (`PATH:P`)

Options:
      --dest <DEST>                    Restore the listed files into this directory
      --disk-file <PATH>               Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --session <SESSION>              Save the list to this file
      --min-integrity <MIN_INTEGRITY>  Restore only files whose integrity estimate is at least this (0-100) [default: 0]
  -h, --help                           Print help
  -V, --version                        Print version
```

### pm recover carve

```text
Find files by their content signatures in raw data

Usage: pm.exe recover carve [OPTIONS] <DEVICE>

Arguments:
  <DEVICE>  `disk:N`, a drive letter, or an image file

Options:
      --dest <DEST>        Extract the carved files into this directory (implies --extract)
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --types <TYPES>      Comma-separated categories: image, document, archive, audio, video, executable, database, other (default: all)
      --sector <SECTOR>    File starts are looked for at multiples of this many bytes (1 = anywhere, slow) [default: 512]
      --session <SESSION>  Save the scan result to this file
      --extract            Write the carved files to --dest
  -h, --help               Print help
  -V, --version            Print version
```

### pm recover open-session

```text
Show a saved scan session

Usage: pm.exe recover open-session [OPTIONS] <FILE>

Arguments:
  <FILE>  

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
  -h, --help              Print help
  -V, --version           Print version
```

## pm analyze

```text
Space usage, surface test, benchmark and SMART health

Usage: pm.exe analyze [OPTIONS] <COMMAND>

Commands:
  space    Show where the space in a directory tree goes
  surface  Read every block of a device and report unreadable and slow ones
  bench    Measure read and write throughput with a temporary file (Windows only)
  smart    Show the SMART health of a disk
  help     Print this message or the help of the given subcommand(s)

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
  -h, --help              Print help
  -V, --version           Print version
```

### pm analyze space

```text
Show where the space in a directory tree goes

Usage: pm.exe analyze space [OPTIONS] <PATH>

Arguments:
  <PATH>  Directory (or drive root) to scan

Options:
      --disk-file <PATH>   Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --threads <THREADS>  Worker threads (default: the number of CPUs)
      --top <TOP>          Entries shown per table [default: 10]
      --json               Print the whole report as JSON
  -h, --help               Print help
  -V, --version            Print version
```

### pm analyze surface

```text
Read every block of a device and report unreadable and slow ones

Usage: pm.exe analyze surface [OPTIONS] <DEVICE>

Arguments:
  <DEVICE>  `disk:N`, a drive letter, or an image file

Options:
      --block <BLOCK>     Block size (default 1M)
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
  -h, --help              Print help
  -V, --version           Print version
```

### pm analyze bench

```text
Measure read and write throughput with a temporary file (Windows only)

Usage: pm.exe analyze bench [OPTIONS] <TARGET>

Arguments:
  <TARGET>  Directory or drive letter (`D:`) to test

Options:
      --disk-file <PATH>     Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --sizes <SIZES>        Comma-separated transfer sizes (default 4K,8K,16K,32K)
      --total <TOTAL>        Bytes moved per pass (default 200M)
      --queue <QUEUE>        Requests issued back to back per worker (default 8)
      --threads <THREADS>    Worker threads (default 4)
      --random               Random instead of sequential access
      --cooldown <COOLDOWN>  Seconds to rest between transfer sizes (default 5)
  -h, --help                 Print help
  -V, --version              Print version
```

### pm analyze smart

```text
Show the SMART health of a disk

Usage: pm.exe analyze smart [OPTIONS] <DISK>

Arguments:
  <DISK>  Disk index (`1` or `disk:1`)

Options:
      --disk-file <PATH>  Use this raw image file as a disk instead of the physical disks (repeatable: the first is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`, then works on the files; no administrator rights are needed. Mounting, formatting and everything else that needs a volume layer is not available on image files
      --json              Print the data as JSON
  -h, --help              Print help
  -V, --version           Print version
```

