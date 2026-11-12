# Actuaria desktop

The Actuaria.io studio as a desktop app: hbench's viewer in a native window with a VS Code-style layout. Built
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
- Apps started from Finder don't get your shell profile. The app reads the login shell's environment once at
  startup, the way VS Code does, so hbench finds `docker` and the model API keys you export in `~/.zshrc`.
- On Unix, hbench runs under a small `sh` watchdog tied to a pipe the app holds. If the app quits, crashes or is
  killed, the server stops.
- Settings (`settings.json`) live in the app config dir and the server log (`hbench-view.log`) in the app log dir.
  On first launch the app opens `~/Actuaria`.

## Build

You need Rust, Node 22+ and, on macOS, Xcode command line tools.

```sh
cd desktop
npm install
npm run dev     # debug hbench + app with hot Rust rebuilds
npm run build   # release hbench + Actuaria.app and .dmg in src-tauri/target/release/bundle/
```

`npm run sidecar` builds hbench and copies it to `src-tauri/binaries/hbench-<target triple>`, where Tauri picks
it up. hbench embeds the viewer from `viewer/dist`. After changing `viewer/src`, run `npm run build` in `viewer/`
first.

Building the `.dmg` arranges its window through Finder. If that step times out (no automation permission, e.g.
over SSH), build with `CI=true npm run build`, which skips the cosmetic layout.

Icons come from `assets/icon.png`, a 1024px white mark on `#2f45ff` like actuaria.io's favicon. Regenerate them
with `npm run icons`.

## Platforms

macOS is built and tested. hbench's own releases cover macOS and Linux, so a Linux build should work the same
way (untested). Windows needs a Windows build of hbench first.
