# Distribution packaging

Templates for every channel `ctrlz` ships through. No file here contains a real
version or checksum: each one carries placeholder tokens that a release run
substitutes, so the checked-in copies never go stale.

| channel | file | what fills it in | secret it needs |
| --- | --- | --- | --- |
| GitHub Releases | `.github/workflows/release.yml` | version from the tag, `SHA256SUMS` computed from the built archives | none (`GITHUB_TOKEN`) |
| crates.io | `Cargo.toml` (version only) | version from the tag via `cargo publish` | `CARGO_REGISTRY_TOKEN` |
| npm | `packaging/npm/package.json`, `packaging/npm/platforms/*/package.json` | `{{VERSION}}` in all five manifests | `NPM_TOKEN` |
| Chocolatey | `packaging/chocolatey/ctrlz.nuspec`, `packaging/chocolatey/tools/chocolateyinstall.ps1` | `{{VERSION}}`, `{{URL}}`, `{{CHECKSUM}}` | `CHOCO_API_KEY` |
| Scoop | `packaging/scoop/ctrlz.json` | `{{VERSION}}`, `{{CHECKSUM}}` (manual bump) | none |
| Homebrew | `packaging/homebrew/ctrlz.rb` | `{{VERSION}}`, `{{SHA_MAC_X64}}`, `{{SHA_MAC_ARM64}}`, `{{SHA_LINUX_X64}}` (manual bump) | none |
| winget | `packaging/winget/manifests/a/akshaytheflash/ctrlz/{{VERSION}}/*.yaml` | `{{VERSION}}`, `{{URL}}`, `{{CHECKSUM}}` (manual PR) | none |

The `release` workflow publishes GitHub Releases, crates.io, npm and Chocolatey.
Scoop, Homebrew and winget are review-gated destinations, so their templates are
filled in and opened as pull requests by hand:

- **Scoop** — the manifest goes to a bucket repository (for example
  `ScoopInstaller/Extras`) as `bucket/ctrlz.json`. `checkver` tracks the GitHub
  release and `autoupdate` reads the published `SHA256SUMS`, so later versions
  only need a `checkver -u` run, not a hand-edited hash.
- **Homebrew** — the formula goes to a tap (for example
  `akshaytheflash/homebrew-ctrlz`) as `Formula/ctrlz.rb`; `brew install
  akshaytheflash/ctrlz/ctrlz` then works. `homebrew-core` generally does not
  accept prebuilt-binary-only formulae, so a tap is the intended channel.
- **winget** — the three manifests must be submitted as a pull request to
  [`microsoft/winget-pkgs`](https://github.com/microsoft/winget-pkgs) under
  `manifests/a/akshaytheflash/ctrlz/<version>/`. winget has no API push.
- **Chocolatey** — `choco push` accepts the package immediately, but the first
  submission of a new package id goes through manual moderation before it is
  listed, which can take a few days. Later versions of an approved package are
  usually available sooner.

## Secrets

Set these as repository secrets (`Settings -> Secrets and variables -> Actions`).
Every publishing job checks its own secret first and reports "skipping" instead
of failing, so a release is green with none of them configured.

| secret | used for |
| --- | --- |
| `CARGO_REGISTRY_TOKEN` | `cargo publish` to crates.io |
| `NPM_TOKEN` | `npm publish` for the wrapper and the four platform packages |
| `CHOCO_API_KEY` | `choco push` to `https://push.chocolatey.org/` |

## Placeholders

| token | meaning |
| --- | --- |
| `{{VERSION}}` | release version without the leading `v`, e.g. `0.1.0` |
| `{{URL}}` | absolute download URL of `ctrlz-x86_64-pc-windows-msvc.zip` for this version |
| `{{CHECKSUM}}` | lowercase sha256 of that Windows zip, 64 hex characters |
| `{{SHA_WIN_X64}}` | explicitly named sha256 of the Windows zip, equal to `{{CHECKSUM}}`; reserved for channels that prefer the per-platform name, unused by the templates above |
| `{{SHA_MAC_X64}}` | sha256 of `ctrlz-x86_64-apple-darwin.tar.gz` |
| `{{SHA_MAC_ARM64}}` | sha256 of `ctrlz-aarch64-apple-darwin.tar.gz` |
| `{{SHA_LINUX_X64}}` | sha256 of `ctrlz-x86_64-unknown-linux-gnu.tar.gz` |

`{{URL}}` always resolves to
`https://github.com/akshaytheflash/ctrlz/releases/download/v{{VERSION}}/ctrlz-x86_64-pc-windows-msvc.zip`.

## Local checks

`packaging/npm/bin/ctrlz.js` is plain CommonJS with no dependencies. It resolves
`@akshaytheflash/ctrlz-<slug>/package.json`, runs `bin/ctrlz` (or `ctrlz.exe`)
next to it with the caller's arguments and stdio, and exits with the child's
status. Supported slugs: `win32-x64`, `darwin-x64`, `darwin-arm64`, `linux-x64`.
Anything else prints the supported list and exits 1 — no download, no fallback
build.
