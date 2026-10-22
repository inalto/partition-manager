# Getting Started

This page covers building Partition Manager, running it, and practicing on an image file
before you go near a real disk. All commands are run from PowerShell.

You need a Rust toolchain from <https://rustup.rs>. Get the source from
<https://github.com/inalto/partition-manager>.

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

Read [[Safety Model]], then work through the [[First Live Test]] on scratch media.
