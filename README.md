# MoonLit

> Open-source, lightweight, local-first game clip recorder for Linux and Windows.
> A Medal.tv-style alternative with zero cloud: press a hotkey, save the last seconds, edit lightly, share from your own storage.

[![License: GPL-3.0-only](https://img.shields.io/badge/License-GPL--3.0--only-blue.svg)](./LICENSE)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20Windows-lightgrey.svg)](https://github.com/SourisCG/Moonlit)
[![Built with Tauri v2](https://img.shields.io/badge/Tauri-v2-purple.svg)](https://tauri.app/)
[![Stack](https://img.shields.io/badge/stack-React_19_%2B_Rust-blue.svg)](./SPEC.md)
[![Status](https://img.shields.io/badge/status-Linux_Alpha_%7C_Windows_Next-yellow.svg)](./docs/ROADMAP_PHASES.md)

Website: [moonlit.souriscg.dev](https://moonlit.souriscg.dev/) · Full spec: [`SPEC.md`](./SPEC.md) · Docs: [`docs/`](./docs)

---

## Why MoonLit?

Clipping epic moments shouldn't require a heavy client, a cloud account, or Windows-only software.

Medal.tv is great, but it's closed, cloud-locked, heavy (300-800 MB Electron), and has no real Linux story. **MoonLit does one thing well:** capture what just happened while you game, with almost zero cost, and leave the files with you.

- **Press `F9` while playing** — get an `.mp4` of the last seconds in <1s.
- **Keep playing** — capture lives in GPU/VRAM + RAM ring, React UI stays hidden in tray.
- **Own your clips** — local files + SQLite + OS keyring. No central server. Share via *your* Google Drive or webhooks.

## Medal.tv vs MoonLit

|  | Medal.tv | MoonLit |
|---|---|---|
| Cloud required | Yes, account + upload | **No. Zero-cloud, local-first** |
| Linux support | No | **Yes (X11 + Wayland via gpu-screen-recorder)** |
| Windows support | Yes | Yes (Windows 10 1903+ / 11, WGC, in progress) |
| Idle footprint while gaming | ~300-800 MB (Electron/Chromium) | **<80 MB RAM, ~0% CPU, window hidden to tray** |
| Replay buffer | Yes | **Yes, RAM ring, no disk writes until save** |
| Audio tracks | Mixed | **3-track: MIX (game+mic) + game solo + mic solo** |
| Editor | Cloud / heavy | **Lightweight local: lossless trim, vertical 9:16, remix** |
| Sharing | Medal cloud link | **Your Drive (public link + clipboard) + Discord / YouTube / etc.** |
| Open source | No | **Yes, GPL-3.0-only** |
| Languages | EN | **ES + EN from day one** |

## Features

### Working now (Linux Alpha, user-verified)

- **Replay buffer with global hotkey** — `F9` saves last N seconds (default 30s) via embedded `gpu-screen-recorder`, `SIGUSR1` flush, no re-encode.
- **Mix-first 3-track audio** — Track 1 = game+mic mix (plays everywhere), Track 2 = game only, Track 3 = mic only for editing. Live per-track gain/mute (0-200%) without touching what you hear.
- **Medal-grade quality** — Medal CBR ladder (360p/720p/1080p/1440p) + old-MoonLit NVENC HQ recipe (`p7/hq/high/bf=2`), 30/60fps selector, monitor selector.
- **Smart delivery** — records at source when needed, downscales on save with `lanczos` for crisp text at non-integer ratios.
- **Gallery** — thumbnails + real durations, favorites, ghost-clip reconcile (auto-purge missing files), LRU prune.
- **Safe persistence** — SQLite with relative paths only (`base_dir + file_name`), secrets in OS keyring, never absolute paths in DB.
- **MoonLit UI** — frameless glass layout, pausable starfield (0 cost while gaming), bilingual ES/EN, tray with status + audio ding on save (audible in fullscreen).

### Coming soon

- **Windows backend** — same surface via WGC + WASAPI + NVENC/AMF/QuickSync. Start point: `docs/09_WINDOWS_HANDOFF.md`.
- **Game detection** — Steam (`SteamAppId` + `.acf`), Wine/Proton cmdline + blacklist, Minecraft/Prism/Bedrock, Heroic/Epic/Battle.net/Xbox, custom apps + process picker.
- **Lazy editor** — `React.lazy` ClipEditor + Wavesurfer Regions + dual waveforms, FFmpeg sidecar (lossless trim <1s, vertical HW, remix). Fully destroyed on close.
- **Sharing** — Drive PKCE + resumable upload + public link + clipboard, Discord webhook, Twitter/YouTube/TikTok flows.
- **Distribution** — `.exe/.msi/.AppImage/.deb/.rpm` from GitHub Releases on `v*` tags. Flathub / MS Store / WinGet later.

See [`docs/ROADMAP_PHASES.md`](./docs/ROADMAP_PHASES.md) and [`docs/PROGRESS.md`](./docs/PROGRESS.md) for acceptance checklists and build log.

## How it works

```
1. Play (MoonLit hidden to tray, RAM ring recording, ~100-150 MB for 60s 1080p)
       ↓ press F9
2. Save (remux ring → .mp4 in Videos/MoonLit, indexed + thumb, ding plays)
       ↓
3. Browse (Gallery → favorite / preview / open externally)
       ↓
4. Edit & Share (trim lossless → vertical/remix if needed → upload to your Drive)
```

No pixels ever touch JS. React only sends `{start, end}` to Rust; Rust runs FFmpeg CLI.

## Quick Start

### Prerequisites

- Node 20 + `pnpm` (`corepack enable pnpm` or `npm i -g pnpm`)
- Rust stable + system webview deps:
  ```bash
  # Fedora / RHEL
  sudo dnf install webkit2gtk4.1-devel libappindicator-gtk3-devel librsvg2-devel

  # Ubuntu / Debian
  sudo apt-get install libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
  ```
- Linux capture (no portal dialogs):
  ```bash
  sudo setcap cap_sys_admin+ep $(which gpu-screen-recorder)
  ```
  Fallback is XDG Portal ScreenCast with saved token if you skip this.
- Windows: 10 version 1903 (build 18362)+ or 11. MSVC toolchain.

### Develop

```bash
pnpm install
pnpm tauri dev
```

### Build

```bash
pnpm build
# Tauri bundle -> src-tauri/target/release/bundle/
```

> Windows SmartScreen (early builds): app is unsigned OSS yet. Click `More info → Run anyway`. Builds are auditable via GitHub Actions. Signing via Store/MSIX comes after Phase 7.

## Usage

- `F9` — save clip (global, works in fullscreen exclusive).
- Tray icon — buffer status (active/idle), show/hide, quit. Main window minimizes to tray while gaming.
- Settings — buffer length, fps (30/60), quality ladder, monitor, `gain_game/gain_mic` + mutes, base folder, locale ES/EN, hotkey (F9 default).
- Clips live in `~/Videos/MoonLit` by default. DB stores only `file_name`, resolved at runtime as `base_dir.join(file_name)` — move the folder freely.

## Privacy: local-first, zero-cloud

- No central server, no telemetry, no account.
- Clips + SQLite stay on disk. Tokens (e.g. `google_drive_refresh_token`) stay in OS keyring (libsecret / Credential Manager / Keychain).
- Uploads go client → service directly (Drive API, Discord webhook). You revoke them where you created them.

## Tech stack (brief)

> Gamers can skip this. Contributors: start at [`SPEC.md`](./SPEC.md) + [`docs/01_ARCHITECTURE.md`](./docs/01_ARCHITECTURE.md).

- **App:** Tauri v2 + React 19 + TypeScript + Vite + Tailwind v3 + `react-i18next` + Wavesurfer v7 + Lucide
- **Backend:** Rust (tokio, serde, rusqlite, keyring, rodio, uuid, dirs, nix/image on Linux)
- **Capture Linux:** `gpu-screen-recorder` sidecar (KMS/DRM, NVENC/AMF/QSV/VA-API). **Windows:** WGC + WASAPI + `windows-capture`/`cpal` (trip in progress)
- **Sidecars:** static `ffmpeg` + `gpu-screen-recorder` in `src-tauri/binaries/`
- **Rules:** HW-encode first, lossless-cut by default, lazy editor, relative paths, zero `cfg(target_os)` outside `os/`, IPC wire keys always camelCase

```
moonlit/
├── docs/          # EN technical spec (01-09 + ROADMAP + PROGRESS + THIRD_PARTY)
├── SPEC.md        # Index + acceptance map
├── src-tauri/src/os/  # ALL platform code (linux/ + windows/ behind traits)
├── src-tauri/src/storage/ editor/ uploader/ detector/
└── src/components/ (starfield / gallery / editor(lazy) / settings)
```

## License

Copyright (C) 2026 SourisCG

This program is free software: you can redistribute it and/or modify it under the terms of the **GNU General Public License version 3 only**, as published by the Free Software Foundation. See [LICENSE](./LICENSE).
