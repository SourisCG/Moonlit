# PROGRESS — MoonLit build log

Single source of truth for phase status. Updated at the end of every phase.
Details per phase live in `ROADMAP_PHASES.md`; technical specs in `01_*`–`08_*`.

| Phase | Scope | Status | Commit | Acceptance |
|---|---|---|---|---|
| 0 | Bare scaffold (Tauri v2 + React-TS + Tailwind v3) + `docs/` spec | ✅ done | `6965fac` (squashed in) | `pnpm build` + `cargo check` green |
| 1 | Tray + F9 hotkey + glass UI + starfield + i18n ES/EN | ✅ done | `6965fac` | F9 fires globally, tray hide/show works |
| 1-fixes | Frameless custom topbar + MoonLit CSS logo + smooth starfield + F9 dedupe | ✅ done | `8e4b5c1` | Single-count verified by user, no tray glitch |
| license | GPL-3.0-only (required by gpu-screen-recorder) | ✅ done | `2b99add` | Verbatim LICENSE + metadata + README |
| 2 | rusqlite persistence (relative paths) + keyring secrets + settings UI | ✅ done | `c72edba` | CRUD, vault OK, folder picker fixed |
| 3 | Capture engine Linux (GSR embedded, 3-track mix-first, gains, ladder, 30/60fps, monitor select) | ✅ done (Linux) | `5ffd70d`+ui | F9 → `.mp4` 3×aac, thumbs, durations, gains — user-verified |
| 3-win | Capture engine Windows trip (WGC + WASAPI + AMF/QSV/x264, same behaviors) | ✅ done (code, HW-verified; user in-game F9 pass pending) | `7078a13` | See `09_WINDOWS_HANDOFF.md`; e2e-tested on RTX 3060 |
| 3-ui | Transparent tray icon, i18n codec labels, opener perms, disk note | ✅ done | `7c5d733` (batch) | user-verified pending |

## Cross-platform gate (project rule)

Any phase with per-OS code is implemented and tested on Linux first, then
**tested on Windows immediately before the phase is closed** — and likewise
in reverse whenever needed. No phase closes with an untested platform stub.
Applies from Phase 3 on (capture, detection, editor/FFmpeg, packaging).
| 4 | Game detection + launchers + custom apps | ⬜ pending | — | Native + Wine/Proton + Minecraft detected |
| 5 | Lazy editor + FFmpeg pipeline | ⬜ pending | — | Lossless <1s, vertical HW, no leak |
| 6 | Drive + social sharing | ⬜ pending | — | Public link copied + notified |
| 7 | CI/CD packaging | ⬜ pending | — | Tag produces all installers |

## Log

- **Phase 3-win — Windows capture engine (2026-09-07, commits `681529d`→`7078a13`)**
  Native WGC replay engine behind the same `CaptureEngine` surface; Linux
  untouched except a mechanical `list_audio_devices` signature alignment
  (argless on both backends; Linux resolves its GSR binary internally).
  `commands.rs` IPC contracts unchanged (only a native-binary fallback path,
  still zero-`cfg` — the rule grep prints nothing).
  - `video.rs`: DXGI vendor (dGPU by VRAM, Basic Render skipped), WGC
    monitors + `resolve_monitor`, probed `offered_codecs` (live
    micro-encode per candidate, cached; AV1 only where it encodes).
  - `audio.rs`: cpal loopback + mic, 48 kHz stereo stems, live 0–200%
    gains via atomics, mix tap full-fidelity (Linux parity).
  - `wgc.rs`: WGC → ffmpeg (NVENC/AMF/QSV/libx264, CBR ladder, 2 s GOP,
    NVENC HQ only on nvidia+h264/hevc) → MPEG-TS RAM ring; save cuts the
    window and muxes 3×AAC (mix/game/mic) in GSR filename style.
  - `x264` always offered (user decision): ladder follows h264,
    `TranscodeEncoder::X264` → `libx264`, EN+ES locale keys,
    `THIRD_PARTY.md` corrected (shipped ffmpeg builds link libx264; the
    project was already GPL-3.0-only via GSR).
  - OS floor documented (Win10 1903+/Win11) in `09`/`02`/`08`.
  - Verified live on RTX 3060: 1080p60 NVENC buffer, save → h264 + 3×AAC;
    `cargo check` + `cargo test` (15 pass) green on
    `x86_64-pc-windows-msvc`. Full Linux `cargo check` from Windows is
    blocked by missing GTK system libs (environmental); the Linux-side
    diff is mechanical and Linux CI must confirm before release.
  - Still needs a user in-game pass: F9 → clip <1s, audible gain sliders,
    device/monitor/codec switching with restart notice.
- **Phase 3-win batch 2 (2026-09-07, `fbb4286`→`052e08c`): audio stock fix,
  embedded ffmpeg, AV-safe home, zero warnings**
  - Stock-install audio failure (root-caused): settings seed GSR magic ids
    (`default_input/output`) that never match cpal names → both streams
    failed. `find_output/input_device` now resolve them to OS defaults;
    regression live test included.
  - FFmpeg fully embedded: `build-aux/fetch-ffmpeg.ps1` pins BtbN
    `win64-gpl` monthly + sha256 + encoder-set assert; `resolve_ffmpeg`
    via `sidecar.rs` (sidecar-first, PATH dev-only); `CaptureConfig`
    carries the path (Linux ignores); probes use the shipped binary.
    Full e2e re-verified against the pinned binary. No shell plugin
    (line-events only — raw pipes need the bare path via resources).
  - AV-safe home: `%LOCALAPPDATA%\MoonLit\Clips` on Windows (outside
    Controlled Folder Access + OneDrive, no elevation), Linux unchanged;
    one-time boot migration of legacy files, DB rows untouched, custom
    folders never moved.
  - `scale_arg` moved to `os/linux` (GSR-only); NVENC HQ recipe completed
    (`spatial-aq 1`, `multipass disabled`, accepted by Gyan 9 + BtbN).
  - Static CRT (`crt-static`, target-scoped): dumpbin shows system DLLs
    only — no VC++ Redistributable needed.
  - Gate: 17 unit tests pass, 3 live HW tests pass, zero `cargo` warnings,
    zero-`cfg` grep empty, `pnpm build` green, full `cargo build` links.
- **Thumbnail fix (2026-09-07): `ffmpeg thumbnail failed` on every Windows
  save.** NVENC/swscale emit limited-range yuv420p, which ffmpeg 9's mjpeg
  encoder rejects (`THUMB_EXIT=-22`, reproduced). `make_thumbnail` now
  passes `-strict unofficial` (pixels untouched) and takes an adaptive seek
  (`min(1 s, half the probed duration)`) so sub-second clips also index.
  Hermetic regression test (`lavfi→libx264` limited-range fixture, CI-safe);
  verified against Gyan 9 and the pinned BtbN (exit 0, valid `.jpg`).
- **Timeline collapse fix (2026-09-07): ≤2 s clips on 10 s+ runs.** WGC
  delivers only on screen change; the unpaced pump let the encoded timeline
  collapse to the activity burst, and `-shortest` then cut the audio to it
  (telemetry proved it: `wgc_in=123 pump_out=259` over 6 s). Fixes: CFR
  pacer with deadline catch-up in the pump (telemetry now
  `wgc_in=81 pump_out=425`, duration tracks wall-clock), `apad` per stem +
  full-length silence placeholders (short stems can never truncate), MIX
  derived at save from aligned solo tails (live mix-ring interleaved chunks
  — removed), e2e asserts probed duration ∈ [4,8] s for a 6 s run, save
  logs pump telemetry, mic-privacy note in `09`.
- **Slow-save investigation (2026-09-07): F9→ding took 30 s+.** Found the
  dev disk (C:) at **477 MB free** with a 29 GB `target/` — disk pressure
  alone stalls every stage (80 MB TS + WAVs + 75 MB clip + AV scans).
  `cargo clean` reclaimed it (26.5 GB free). Shipped with the same commit:
  per-stage timing logs (`save total/engine/probe+thumb/db`, `mux wav/mux`,
  `wgc-save`) and parallel probe+thumbnail (`join!`, 1 s seek with near-head
  retry for sub-second clips). Next F9 prints exactly where seconds go.

- **Phase 0/1** — Scaffold, tray minimize-to-tray, F9 global shortcut with notification, MoonLit glass layout, pausable canvas starfield, ES/EN i18n. Manual test: F9 counted globally, tray restore OK.
- **Phase 1-fixes** — Frameless window + custom topbar (drag, minimize, maximize, close-to-tray), MoonLit moon+play CSS logo from moonlit.souriscg.dev, time-based soft starfield twinkle, Rust 400ms + frontend 300ms F9 dedupe. Manual test passed by user.
- **License** — Adopted `GPL-3.0-only` (gpu-screen-recorder is GPL-3.0-only per Arch/Alpine/Artix). Verbatim FSF text, metadata in `package.json`/`Cargo.toml`, README section.
- **Phase 2-fixes** — Single-source locale (`useLocale`: DB + i18next in sync, both directions), added missing `dialog:allow-open` capability (folder picker was silently rejected), visible errors on browse/save, logo glow contained (was `z-index:-1` leaking → corner artifact).
- **Maximize flicker (root-caused)** — Tauri natively toggles maximize on double-click over `data-tauri-drag-region` (internal-toggle-maximize, tauri#12006); our own JS dblclick handler double-toggled (flicker). Fix: removed JS dblclick, native owns the gesture; buttons keep atomic `toggleMaximize` + debounce + OS-synced state. Capability `allow-internal-toggle-maximize` added explicitly.
- **Dev env** — `src-tauri/.cargo/config.toml` sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` for all cargo runs (fixes Wayland `Error 71` crash at first paint on Fedora/GNOME); `pnpm tauri:dev` is the official launch command.
- **UI batch** — MoonLit icon set regenerated from `build-aux/moonlit-icon.svg`
  (taskbar + tray via `default_window_icon`); `lang.es/lang.en` translated in
  sidebar + select (last hardcoded Spanish gone); `opener:allow-open-path` +
  `opener:allow-reveal-item-in-dir` added (`opener:default` does not include
  those commands — root cause of the open-video error); disk-space note under
  the resolution selector (ES/EN).
- **Native scaler patch (scheduled, not implemented)** — After Flatpak + MS
  Store ship, before signing `.exe`/`.msi`: raise GSR's downscale filter from
  hardcoded `GL_LINEAR` (bilinear, no mipmaps — proven in pinned source
  `src/window_texture.c`) to Bicubic (Lanczos under test). Same RAM/bitrate/
  latency/save path. Acceptance: stock vs patched 720p on a 1:1 monitor.
- **Authorship rewrite (2026-09-06)** — All 26 commits re-signed from
  `SourisCG <souris@souriscg.dev>` (assumed by the agent at `git init`, never
  confirmed) to `Sebastián García <sebastian.garciab2004@gmail.com>` (user's
  global identity). Content, messages and dates byte-identical
  (`git diff` old vs new tip: empty). Original history preserved in branch
  `main-backup-20260906` (local + remote, keep until told otherwise).
  Old `build <hash>` seals below resolve via the backup branch + map.
- **Wayland taskbar association, dev flow (2026-09-06)** — Window showed the
  generic Wayland icon in the taskbar while the tray icon rendered fine.
  Root cause: on Wayland the taskbar matches by `appId ↔ .desktop`
  (`dev.souriscg.moonlit` via `app.enableGTKAppId`), and no dev `.desktop`
  existed in the repo; a stale `com.souriscg.MoonLit.desktop` (wrong
  identifier/WMClass, `Exec=MoonLit` pointing nowhere) also conflicted.
  Fix: `app.enableGTKAppId: true` in `tauri.conf.json`,
  `build-aux/dev.souriscg.moonlit.desktop.template` +
  `build-aux/install-dev-desktop.sh` (`pnpm desktop:install`) installing a
  validated dev entry + removing the stale one. Tray is unaffected (explicit
  `TrayIconBuilder` pixels). Verified by user on KDE Wayland.
- **Transparent icon set (2026-09-06)** — `src-tauri/icons/` regenerated from
  `build-aux/moonlit-icon.svg` as fully transparent (rounded artwork, no
  opaque backdrop). `tray-icon.png` currently mirrors the set; the dedicated
  tray asset separation is documented as future work in `07_UI_MOONLIT.md`.
- **HEVC save fix (2026-09-06)** — h265 clips failed to save: NVENC HQ opts
  are now per-codec (`nvenc_hq_opts`: `high` for h264, `main` for hevc) with
  unit tests in `video_quality.rs`.
- **Purge missing clips (2026-09-06)** — New `purge_missing_clips` command +
  gallery button: drops DB rows whose files no longer exist on disk.
- **Gain range 0–200% (2026-09-06)** — Track sliders extended from 150 to 200
  (clamps in `os/linux/audio.rs::set_volume`, `read_gains`, `set_track_gain` +
  `TrackMixer` slider). Defaults stay 100/100; 200% is a boost tool for quiet
  sources — safe ceilings documented in `02_CAPTURE_ENGINE.md` (game ≈100, it
  already peaks near 0 dB at unity; mic ≈120–150). PipeWire acceptance of 200%
  verified live; user-verified on a real clip.

## Hash map (old → new, same order/messages/dates)

| Old | New | Subject |
|---|---|---|
| `6965fac` | `ccf9451` | feat(phase1): tray minimize-to-tray + F9 global hotkey + MoonLit glass UI + pausable starfield + i18n es/en |
| `8e4b5c1` | `9a81b9a` | fix(phase1): custom frameless topbar + MoonLit CSS logo + smooth starfield + F9 dedupe |
| `2b99add` | `2f1234e` | chore(license): adopt GPL-3.0-only (required by gpu-screen-recorder sidecar) |
| `c72edba` | `b8f218e` | feat(phase2): rusqlite persistence with relative paths + keyring secrets + settings UI |
| `f8884c6` | `94536d3` | docs: mark phase 2 done in PROGRESS |
| `982890a` | `9f3cc75` | fix(phase2): synced locale source, dialog permission, logo glow containment |
| `1d14c82` | `943d024` | fix(dev): permanent Wayland workaround via .cargo/config env |
| `259dcdb` | `a885c28` | docs: log wayland dev fix in PROGRESS |
| `69fdd4a` | `f051f0c` | fix(phase2): stateless locale from i18n + hardened maximize toggle |
| `3c93467` | `4b14c1e` | debug(phase2): temp maximize flicker logging (to be removed with fix) |
| `4a9d44d` | `75052f0` | fix(phase2): atomic maximize toggle + delayed drag-area maximize |
| `2b2c5e2` | `71848fc` | fix(phase2): let Tauri own drag-area dblclick maximize |
| `502601e` | `bbddf69` | docs: log maximize root cause in PROGRESS |
| `00a70b3` | `ff9e586` | feat(phase3): GSR replay engine + dual audio + record UI (Linux) |
| `d0fec68` | `a44eb46` | feat(phase3): embedded GSR build script + live per-track volumes |
| `94d6d8a` | `f371bba` | feat(phase3): embedded GSR binary + aac tracks |
| `cbb29bd` | `a60d51a` | fix(phase3): stream matching, thumbs, duration restart, slider UX |
| `b0ab0a5` | `4ae01b3` | fix(phase3): exact stream match, thumbs, restart keys, devices, quick-open |
| `efec83a` | `ce54586` | refactor+fix(phase3): OBS-style os/ layout, asset protocol, real durations |
| `2e06904` | `2f075da` | fix(phase3): gallery state+errors, stream debug log, responsive pass |
| `0c6dd61` | `949eaae` | fix(phase3): icon-only gallery, self-clearing errors, build stamp |
| `a9e5377` | `7a2ca1b` | feat(phase3): 3-track layout, mix first (plays everywhere) |
| `e95189c` | `3229e91` | fix(phase3): reveal fallback to openPath(parent) |
| `4f30c08` | `1981222` | fix(phase3): camelCase revert, scroll, title, resize grips |
| `328e911` | `eae081d` | fix(phase3): clip filename as row title, probe cleanup |
| `f290be2` | `a098988` | feat(phase3): Medal bitrate ladder + NVENC HQ recipe + video settings |
