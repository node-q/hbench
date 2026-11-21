# Actuaria Studio Essentials

The Actuaria.io studio as a desktop app, Actuaria Studio Essentials: hbench's viewer in a native window with a VS Code-style layout. Built
with [Tauri 2](https://tauri.app).

## How it works

- The app bundles `hbench` (Tauri `externalBin`) and runs `hbench view <folder>` on a loopback port, preferring
  47810–47829 so the page's saved state (theme, side bar, tabs) persists across launches.
- The window shows that viewer. The viewer sees `window.__ACTUARIA_DESKTOP__` and swaps its web top bar for the
  desktop chrome in `viewer/src/desktop/`:
  - an activity bar;
  - an explorer of jobs and their trials;
  - editor tabs with breadcrumbs;
  - a status bar.

  The Actuaria theme comes from actuaria.io's tokens and the Geist fonts. Add `?desktop` to a viewer URL to
  preview the layout in a browser.
- There is no IPC. Native menus call `window.__actuaria.command(..)`. The page asks for native actions (open
  folder, reveal, theme, external links) by navigating to `/__actuaria/<action>`, which the app intercepts and
  never loads. Links to other sites open in the default browser.
- **Signing in to Actuaria.** The page asks with `/__actuaria/login`, `/__actuaria/logout` and
  `/__actuaria/account`; the app runs the bundled `hbench login --browser` or `hbench logout`, or opens
  `https://account.actuaria.io` in the default browser. Sign-in happens in the system browser, never in the app's
  window. The page gets only progress (`window.__ACTUARIA_SIGNIN__` and an `actuaria:signin` event, with the
  sign-in address only when no browser opened) and asks the viewer's `/api/account` who is signed in: **tokens
  never enter the window**. The refresh token is DPoP-bound to a key `hbench` keeps in the OS keychain next to it
  (docs/REFERENCE.md, "Signing in to Actuaria"); the app never sees the key. No Tauri command is added and the capabilities stay empty. The app takes
  `/__actuaria/` actions only from its own pages (the start screen and the viewer it runs).
- The viewer's launch key: the app makes a new one for each `hbench view` it starts, passes it in
  `HBENCH_VIEW_KEY`, and opens the window at `http://127.0.0.1:<port>/#k=<key>`. Writes and the account route
  need it (docs/REFERENCE.md, Viewer).
- Apps started from Finder don't get your shell profile. The app reads the login shell's environment once at
  startup, the way VS Code does, so hbench finds `docker` and the model API keys you export in `~/.zshrc`.
- On Unix, hbench runs under a small `sh` watchdog tied to a pipe the app holds. If the app quits, crashes or is
  killed, the server stops.
- Settings (`settings.json`) live in the app config dir and the server log (`hbench-view.log`) in the app log dir.
  On first launch the app opens `~/Actuaria`.
- When a run finishes, the app shows a toast if its window is focused, or a system notification if it is in
  the background.
- The window title names the open tab and the folder, e.g. `Compare — my-evals`.
- The app updates itself. A few seconds after launch, then every four hours, it reads the latest release's
  `latest.json` and, when there is a newer version, the page shows a prompt: **Update now** downloads the package,
  checks its signature against the public key in `tauri.conf.json`, swaps it in, and asks to restart. **Later**
  hides the prompt and leaves an **Update to X.Y.Z** action in the status bar; that version is not prompted for
  again at launch. **Check for Updates…** (app menu on macOS, Help elsewhere) checks on demand. On Windows the
  installer takes over at once and relaunches the app; on Linux the `.deb` or `.rpm` is installed through
  `pkexec`. Debug builds never check; `ACTUARIA_UPDATE_DEMO=9.9.9 npm run dev` fakes an update to try the flow,
  and `?desktop&update=9.9.9` does the same in the browser preview.

## Shortcuts

| Shortcut | Action |
| --- | --- |
| ⌘N | New run |
| ⌘O | Open folder |
| ⌘⇧R | Reveal the folder in Finder |
| ⌘W / ⌥⌘T | Close tab / close other tabs |
| ⌘⇧W | Close window |
| ⌘⇧] / ⌘⇧[ | Next / previous tab |
| ⌘[ / ⌘] | Back / forward |
| ⌘P, ⌘⇧P, ⌘K | Command palette |
| ⌘1 / ⌘2 / ⌘3 | Jobs / Compare / New run |
| ⌘B | Toggle the side bar |
| ⌘⇧L | Toggle light / dark |
| ⌘= / ⌘- / ⌘0 | Zoom in / out / reset |

In the explorer, arrow keys move between rows, Right and Left expand and collapse a job, and Enter opens the row.

## Build

You need Rust, Node 22+ and, on macOS, Xcode command line tools. The app runs on macOS 13 or later (the layout
uses CSS container queries).

```sh
cd desktop
npm install
npm run dev     # debug hbench + app with hot Rust rebuilds
npm run build   # release hbench + the .app and .dmg in src-tauri/target/release/bundle/
```

Debug builds (`npm run dev`) keep their own settings and log (`settings.dev.json`, `hbench-view.dev.log`) and
run beside an installed copy of the app without handing over to it. Quit the packaged app before `npm run build`
replaces it.

`npm run sidecar` builds hbench and copies it to `src-tauri/binaries/hbench-<target triple>`, where Tauri picks
it up. hbench embeds the viewer from `viewer/dist`. After changing `viewer/src`, run `npm run build` in `viewer/`
first.

Building the `.dmg` arranges its window through Finder. If that step times out (no automation permission, e.g.
over SSH), build with `CI=true npm run build`, which skips the cosmetic layout.

Icons come from `assets/icon.png`, actuaria.io's mark on the app's violet, a 1024px render of `assets/icon.svg`
(any SVG renderer will do, e.g. headless Chrome with `--default-background-color=00000000`). Regenerate every
size from it with `npm run icons`.

## Releases

The app's version is hbench's. `scripts/set-version.sh` (run by the Release workflow, see
[docs/RELEASING.md](../docs/RELEASING.md)) writes it into `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock` and
`src-tauri/tauri.conf.json` along with hbench's own files, and each release build sets it again from the tag.

Tagging an hbench release (`vX.Y.Z`) also builds the app. `.github/workflows/release.yml` builds the app
for Apple Silicon and Intel around that release's own `hbench` binary, gives it the release's version, and
attaches `Actuaria-Studio-Essentials-vX.Y.Z-<target>.dmg` to the GitHub release, listed in `SHA256SUMS` and attested. It does the
same for Linux, as a `.deb` and an `.rpm` per architecture.

Signing happens in a separate job that runs only `scripts/package-macos.sh` (Apple's tools, no npm or cargo
code). It reads these secrets of the `release` environment:

| Secret | What it is |
| --- | --- |
| `APPLE_CERTIFICATE` | A "Developer ID Application" certificate exported as `.p12`, base64-encoded |
| `APPLE_CERTIFICATE_PASSWORD` | That `.p12`'s password |
| `APPLE_API_KEY` | The contents of an App Store Connect API key (`.p8`), used to notarize |
| `APPLE_API_KEY_ID` | The key's ID |
| `APPLE_API_ISSUER` | The key's issuer ID |

Until they are set, the `.dmg` is ad-hoc signed and the job warns: the app runs, but Gatekeeper blocks a
downloaded copy until the user allows it in System Settings. With the certificate alone it is signed but not
notarized. A desktop failure does not hold back the hbench release.

To package a local build the same way: `scripts/package-macos.sh "<Actuaria Studio Essentials.app>" <out.dmg>`.

### Updates

Each release also carries what the in-app updater needs: `Actuaria-Studio-Essentials-vX.Y.Z-<target>.app.tar.gz` (the signed macOS
app, written by `package-macos.sh`), a `.sig` for it and for each Windows installer and Linux package, and
`latest.json`, which lists them. The `desktop-updater` job makes the signatures and the manifest with
`scripts/sign-updates.sh`, and is the only job that sees the updater key, two more secrets of the `release`
environment:

| Secret | What it is |
| --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | The private key from `npx tauri signer generate` (the file's contents) |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Its password |

Its public key is `plugins.updater.pubkey` in `src-tauri/tauri.conf.json`. Each signature names the version it
was made for and the app requires that, so an old package can't be passed off as a new one. Losing the private key
means installed apps can't be updated any more: rotating it takes a release whose app carries the new public key,
signed with the old one. Without the secrets the release ships without `latest.json` and the job warns.

Before this name the app was called Actuaria. Its executable is still `Actuaria` and its identifier and data are
unchanged, so an update carries settings over. The installers file an install under the app's name, though, so
an update from Actuaria takes care of the old install: on macOS it replaces the bundle in place (which keeps
its `Actuaria.app` file name); the `.deb` and `.rpm` replace the `actuaria` package; and the Windows installer
silently uninstalls the old app after installing (`src-tauri/windows/hooks.nsh`).

## Platforms

- **macOS** (13 or later): `.dmg` for Apple Silicon and Intel, signed and notarized by the release.
- **Linux**: `.deb` and `.rpm` for x86_64 and aarch64, built on Ubuntu 22.04, so they need its glibc (2.35) and
  WebKitGTK 4.1 or newer: Ubuntu 22.04+, Debian 12+, Fedora 38+. They are not signed; verify them with
  `SHA256SUMS` or `gh attestation verify`. The package installs `Actuaria` and the bundled `hbench` in
  `/usr/bin`. To build locally: `sh scripts/linux-deps.sh`, then `npm run sidecar && npx tauri build --bundles deb,rpm`.
- **Windows** (10 or later, x86_64): an installer, `Actuaria-Studio-Essentials-vX.Y.Z-x86_64-pc-windows-msvc-setup.exe`, that
  installs for the current user. It is not Authenticode-signed, so SmartScreen warns on first run; verify it
  with `SHA256SUMS` or `gh attestation verify`. The app has been built in CI but not yet launched on Windows.
  Unlike on macOS and Linux, a crash or a kill of the app leaves its `hbench view` running; a normal quit
  stops it.
