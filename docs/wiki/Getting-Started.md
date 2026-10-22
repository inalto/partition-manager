# Getting Started

This page covers building Partition Manager, running it, and practicing on an image file
before you go near a real disk. The Windows commands are run from PowerShell; Linux and
macOS have their own sections ([build](#linux-and-macos), [run](#on-linux-and-macos)).

You need a Rust toolchain from <https://rustup.rs>. Get the source from
<https://github.com/inalto/partition-manager>.

**Prebuilt binaries.** Every tagged version has `pm` and `partition-manager` on the
[Releases page](https://github.com/inalto/partition-manager/releases) for Windows x86_64,
Linux x86_64 (glibc 2.35 or newer) and macOS on Apple Silicon and Intel (unsigned: run
`xattr -d com.apple.quarantine pm partition-manager` after downloading), with
`SHA256SUMS.txt`. The rest of this page is for building from source.

## Building

The release binaries land in `target\release\`: `pm.exe` (command line) and
`partition-manager.exe` (GUI). `scripts\build-release.ps1` runs the build and copies both
into `dist\`.

There are two ways to get a linker on Windows. Pick one.

### Route A: Visual Studio Build Tools and the MSVC toolchain (default)

1. Install **Visual Studio Build Tools** with the **Desktop development with C++** workload
   and a **Windows SDK**.
2. Make sure no `rust-toolchain.toml` or `.cargo\config.toml` in the checkout pins the GNU
   toolchain (the repository ships them only as `*.example` files, so a fresh checkout has
   neither).
3. Build with the default stable `msvc` toolchain:

```powershell
cargo build --release
```

### Route B: GNU toolchain with a portable WinLibs MinGW

Use this on a machine without Visual Studio Build Tools.

1. Install the portable MinGW-w64 (WinLibs):

   ```powershell
   winget install --id BrechtSanders.WinLibs.POSIX.UCRT --scope user
   ```

2. Copy the two example files and edit them:

   ```powershell
   Copy-Item .cargo\config.example.toml .cargo\config.toml
   Copy-Item rust-toolchain.example.toml rust-toolchain.toml
   ```

3. `rust-toolchain.toml` pins the GNU toolchain:

   ```toml
   [toolchain]
   channel = "stable-x86_64-pc-windows-gnu"
   ```

4. `.cargo\config.toml` points the linker and `dlltool` at the WinLibs `mingw64\bin`
   directory. Adjust the paths to your user name and to wherever winget put the package:

   ```toml
   [target.x86_64-pc-windows-gnu]
   linker = "C:/Users/<you>/AppData/Local/Microsoft/WinGet/Packages/BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe/mingw64/bin/gcc.exe"
   rustflags = ["-C", "dlltool=C:/Users/<you>/AppData/Local/Microsoft/WinGet/Packages/BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe/mingw64/bin/dlltool.exe"]
   ```

5. Build:

   ```powershell
   cargo build --release
   ```

Because the linker path is explicit, the same `cargo build --release` also works from Git
Bash. The first build installs the pinned toolchain through rustup if it is missing.

### Linux and macOS

```sh
cargo build --release          # target/release/pm and target/release/partition-manager
cargo test --workspace
scripts/build-release.sh       # copies both, README and LICENSE into dist/<os>-<arch>/ + a tarball
```

On Debian/Ubuntu the GUI needs the windowing development packages:

```sh
sudo apt-get install -y build-essential pkg-config libgtk-3-dev libxkbcommon-dev \
  libwayland-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
```

At run time Linux uses the system tools when installed: dosfstools (`mkfs.vfat`),
exfatprogs (`mkfs.exfat`), ntfs-3g (`mkfs.ntfs`, `ntfsresize`), e2fsprogs (`mkfs.ext4`,
`e2fsck`, `resize2fs`), parted (`partprobe`), udev (`udevadm`) and smartmontools
(`smartctl`). A missing tool fails only the operation that needs it; `pm doctor` lists what
it found. macOS needs the Xcode command line tools to build; `diskutil` ships with the
system, and SMART data needs `brew install smartmontools`.

If your checkout carries a `rust-toolchain.toml` pinned to the Windows GNU channel (Route B),
override it: `RUSTUP_TOOLCHAIN=stable cargo build --release`.

### About the elevation manifest

The release GUI carries a `requireAdministrator` manifest and asks for UAC on launch. It is
embedded with `embed-resource` under MSVC and with `windres` from the MinGW directory under
GNU, and only in release builds. `PM_GUI_MANIFEST=1` forces it into a debug build,
`PM_GUI_MANIFEST=0` leaves it out of a release build.

### Running the tests

```powershell
cargo test --workspace
```

The tests run on synthetic images and the mock platform and need no administrator rights.
Tests that touch real Windows volumes or disks are marked `#[ignore]`; run them from an
**elevated** prompt with `cargo test --workspace -- --ignored`.

## Running

### The GUI

```powershell
.\dist\partition-manager.exe                       # elevated (UAC prompt), real disks
.\dist\partition-manager.exe --dry-run             # Apply only simulates
.\dist\partition-manager.exe --disk-file C:\img\practice.img
```

The release build always starts elevated. For development, `cargo run -p pm-gui` runs
unelevated: the real disk list is then empty and a banner says so, while `--disk-file` still
works. See [[GUI Guide]].

### The command line

`pm.exe` needs an elevated prompt for physical disks and no elevation for image files. Exit
codes: 0 success, 1 usage error, 2 platform or runtime error. Add `--disk-file PATH`
(repeatable; `disk:0` is the first file) to run any disk-taking command against image files.
Start with:

```powershell
pm doctor        # elevation, toolchain, disks seen
pm list          # disks and partitions (add --json for scripts)
```

Every command is listed in the [[CLI Reference]]. Commands that change a disk print what
they would do and write nothing until you add `--yes`.

### On Linux and macOS

Run as root for physical disks, as yourself for image files:

```sh
sudo ./target/release/pm doctor        # Platform, root, system disk, external tools found
sudo ./target/release/pm list          # each disk with its node: "Disk 1 (...)  /dev/sdb"
sudo ./target/release/pm table /dev/sdb
sudo ./target/release/partition-manager
./target/release/partition-manager --disk-file practice.img
```

Name disks by their node (`/dev/sdb`, `/dev/nvme0n1`, `/dev/disk2`): a whole-disk node works
wherever `disk:N` does, `/dev/sdb:2` is its partition 2, and a partition node (`/dev/sdb2`,
`/dev/disk2s1`) is a volume. `disk:N` is a position in the list and can change when a disk is
plugged in. Volumes mount at folders (`pm mount /dev/sdb2 --folder /mnt/data`); there are no
drive letters. Without root everything is read-only.

## Try it on an image file

This walkthrough never touches a real disk. Creating the image file is the only step that
might need an elevated prompt on some systems (`fsutil`); the `pm` steps need none.

```powershell
# 1. Make a 2 GB raw image (it occupies 2 GB on disk until you delete it)
mkdir C:\img
fsutil file createnew C:\img\practice.img 2147483648

# 2. Give it a GPT and two partitions (omit --yes first to see what would happen)
pm --disk-file C:\img\practice.img init-disk 0 --gpt --yes
pm --disk-file C:\img\practice.img create 0 --start 1M --len 1G --name Data --yes
pm --disk-file C:\img\practice.img create 0 --start 1025M --len 512M --type efi --yes

# 3. Look at the result
pm --disk-file C:\img\practice.img list
pm --disk-file C:\img\practice.img table disk:0

# 4. Open the GUI on it: queue, undo, dry-run and Apply all work on the file
partition-manager.exe --disk-file C:\img\practice.img

# 5. Save it as a VHD and as qcow2, then check them
pm image C:\img\practice.img C:\img\practice.vhd
pm convert C:\img\practice.vhd C:\img\practice.qcow2
```

Things to know about practice mode:

- There is no volume layer on image files. Creating a partition leaves it unformatted;
  `format`, `mount`, `unmount` and drive letters report "not available on image files".
- To practice a file-system workflow, image or clone a real partition into the file.
- To check the images you made with external tools (`qemu-img`, Hyper-V), see
  [[Imaging and VM Formats]].

## Before a real disk

Read [[Safety Model]], then work through the [[First Live Test]] on scratch media; on Linux
and macOS it starts with a loop device or a disk image.
