# Wiki source

These pages are the source of the GitHub wiki at
https://github.com/inalto/partition-manager/wiki (a separate git repository,
`https://github.com/inalto/partition-manager.wiki.git`).

To publish a change: edit here, then copy the files into a clone of the wiki repository and push.

```powershell
git clone https://github.com/inalto/partition-manager.wiki.git $env:TEMP\pm-wiki
Copy-Item docs\wiki\*.md $env:TEMP\pm-wiki\ -Exclude README.md
git -C $env:TEMP\pm-wiki add -A
git -C $env:TEMP\pm-wiki commit -m "Update wiki"
git -C $env:TEMP\pm-wiki push
```
