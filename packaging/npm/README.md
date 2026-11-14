# @akshaytheflash/ctrlz platform packages

`@akshaytheflash/ctrlz` is a thin wrapper: the actual executable ships in one of
four per-platform packages (`@akshaytheflash/ctrlz-win32-x64`,
`-darwin-x64`, `-darwin-arm64`, `-linux-x64`) declared as
`optionalDependencies`, and the wrapper's `bin/ctrlz.js` resolves the one that
matches the host and `spawn`s it, so the CLI behaves exactly like a native
binary. These platform packages contain a prebuilt `ctrlz` binary that is
extracted from the matching GitHub Release archive by CI at **publish** time —
nothing is downloaded and no install script runs at **install** time, so
`npm install -g @akshaytheflash/ctrlz` works offline behind a registry mirror
and never executes a postinstall script. Each platform package lists `os` and
`cpu` so npm installs only the one that fits the machine; on any other platform
the wrapper exits with a clear error listing the four supported slugs.
