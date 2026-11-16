# Windows installer: design

Date: 2026-11-16. Scope: Windows only (decided by the user). Linux and macOS keep the
tar.gz archives.

## Why

Today the only Windows download is a zip to unpack somewhere. An installer gives a Start
menu entry, an icon, an entry in "Installed apps" with an uninstaller, and a known place
for the files. The portable zip stays for people who prefer it.

## Goals, non-goals

- One `partition-manager-<version>-windows-x86_64-setup.exe` per release, built by the
  release workflow and listed in `SHA256SUMS.txt`.
- An installed copy keeps updating itself from inside the app, like a portable one.
- Not a goal: code signing (no certificate; Windows SmartScreen will say "Unknown
  publisher" on first run), MSI, per-user installs, installers for Linux or macOS, version
  information in the executable's file properties.
- Must not happen: an update that leaves "Installed apps" showing the old version, or an
  uninstaller that leaves files behind.

## Approaches

1. **Inno Setup (chosen).** One short script, one `setup.exe`, silent mode for updates,
   uninstaller and shortcuts for free, available on the GitHub Windows runners through
   Chocolatey.
2. **MSI with WiX.** Right for group-policy deployment, which nobody asked for; far more
   to write and maintain.
3. **NSIS.** Same result as Inno Setup with a more awkward script and no advantage here.

## What the installer does

- Installs for all users into `C:\Program Files\Partition Manager` (the app needs
  administrator rights anyway): `partition-manager.exe`, `pm.exe`, `README.md`, `LICENSE`,
  `CHANGELOG.md`.
- Start menu entry; optional desktop icon (unchecked); optional "add `pm` to PATH"
  (unchecked), removed again on uninstall.
- Entry in "Installed apps" with version, publisher, icon and uninstaller.
- Offers to start the app at the end.
- 64-bit Windows only.
- Script: `installer/partition-manager.iss`, version passed as `/DAppVersion=`.

## Icon

The logo is `assets/logo.png` (supplied by the user, on a white background).
`scripts/make-icon.py` cuts the background away and writes
`crates/pm-gui/assets/partition-manager.ico` and a raw RGBA copy. The `.ico` is embedded in
`partition-manager.exe` (release builds) and used by the installer and the shortcuts; the
window, the taskbar and the About window use the RGBA copy. Run the script again when the
logo changes.

## Updating an installed copy

- `pm_update::install_kind` returns a new `InstallKind::Installed` when the app's folder
  holds Inno Setup's uninstaller (`unins000.exe`).
- For an installed copy `pm_update::update` downloads the release's `-setup.exe` into the
  staging folder (`.pm-update`, inside Program Files, so only administrators can touch it)
  and verifies it against `SHA256SUMS.txt`, exactly like the archive. Nothing is replaced
  yet.
- "Restart" then runs the installer with `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART
  /RELAUNCH=1` and quits. The installer replaces the files, updates "Installed apps" and
  starts the app again (the script starts it only when it sees `/RELAUNCH=1`).
- A release without an installer (0.3.0 and older) cannot update an installed copy; the
  app says so and links the release page.
- Limit: the app restarts without the command-line arguments it was started with.
- A portable copy is unchanged: it swaps the files in place.

## Build

- `.github/workflows/release.yml`, Windows job: install Inno Setup with Chocolatey, run
  `ISCC.exe`, upload `dist/*-setup.exe` next to the zip. The publish job already hashes
  every asset.
- `scripts/build-release.ps1 -Installer` builds it locally when Inno Setup is installed.

## Not verifiable before the first tagged build

Inno Setup is not installed on the development machine: the script is first compiled by
the release workflow (`workflow_dispatch` builds the artifacts without publishing, which
is the way to try it). Installing, updating through the app and uninstalling have to be
tried by hand on a Windows machine.
