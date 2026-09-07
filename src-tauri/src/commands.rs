//! Tauri IPC handlers (Phase 2: persistence; Phase 3: capture).

use std::collections::HashMap;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::os::{
    audio, backend_name, binary, caps, devices, open, video, AudioDevice, CaptureConfig,
    CaptureEngine,
};
use crate::video_quality;
use crate::state::{AppState, Engine};
use crate::storage::models::{ClipRecord, CustomApp, RegisterAppInput};
use crate::storage::{secrets, DbState};

#[tauri::command]
pub fn list_clips(db: State<'_, DbState>) -> Result<Vec<ClipRecord>, String> {
    db.list_clips()
}

#[tauri::command]
pub fn toggle_favorite(db: State<'_, DbState>, id: String) -> Result<bool, String> {
    db.toggle_favorite(&id)
}

#[tauri::command]
pub fn delete_clip(db: State<'_, DbState>, id: String) -> Result<(), String> {
    db.delete_clip(&id)
}

/// Drop DB rows whose files are gone from disk. Returns rows removed.
#[tauri::command]
pub fn purge_missing_clips(db: State<'_, DbState>) -> Result<u32, String> {
    let base = db.clips_dir()?;
    let missing: Vec<String> = db
        .list_clips()?
        .into_iter()
        .filter(|c| {
            !crate::storage::paths::resolve_clip_path(&base, &c.file_name).exists()
        })
        .map(|c| c.id)
        .collect();
    let n = missing.len() as u32;
    for id in &missing {
        db.delete_row(id)?;
    }
    Ok(n)
}

/// Absolute filesystem path for a clip file name (for <video> / convertFileSrc).
#[tauri::command]
pub fn resolve_clip_src(db: State<'_, DbState>, file_name: String) -> Result<String, String> {
    if file_name.contains("..") || file_name.starts_with('/') || file_name.starts_with('\\') {
        return Err("invalid file name".into());
    }
    let base = db.clips_dir()?;
    Ok(base.join(&file_name).to_string_lossy().to_string())
}

#[tauri::command]
pub fn get_settings(db: State<'_, DbState>) -> Result<HashMap<String, String>, String> {
    db.get_settings()
}

async fn start_engine(app: &AppHandle) -> Result<EngineStatus, String> {
    let db = app.state::<DbState>();
    let dir = db.clips_dir()?;
    let secs = buffer_seconds(&db) as u32;
    let mic_device = setting_str(&db, "mic_device", "default_input");
    let desktop_device = setting_str(&db, "desktop_device", "default_output");
    let (gsr_bin, source) = match binary::backend_binary(app) {
        Ok(v) => v,
        // Native backend (Windows WGC): no sidecar binary. Per-OS probes
        // ignore this path, so shared code stays free of OS branches.
        Err(_) => (PathBuf::new(), "native"),
    };
    eprintln!("[moonlit] capture backend: {} ({})", gsr_bin.display(), source);

    // Video quality: Medal ladder + old-MoonLit NVENC HQ recipe.
    let mut codec = setting_str(&db, "video_codec", "h264");
    if !["h264", "hevc", "av1", "x264"].contains(&codec.as_str()) {
        codec = "h264".to_string();
    }
    let out_height: u32 = setting_str(&db, "out_height", "0").parse().unwrap_or(0);
    // Capture framerate: 30 or 60 only (MVP). Anything else falls back to 60.
    let fps: u32 = match setting_str(&db, "fps", "60").parse().unwrap_or(60) {
        30 => 30,
        _ => 60,
    };
    let vendor = video::vendor(&gsr_bin).await;
    let monitors = video::list_monitors(&gsr_bin).await;
    let monitor = setting_str(&db, "monitor", "");
    // Source height: selected monitor, else tallest known monitor.
    let source_height = if monitor.trim().is_empty() {
        monitors.iter().map(|m| m.height).max().unwrap_or(0)
    } else {
        monitors
            .iter()
            .find(|m| m.name == monitor.trim())
            .map(|m| m.height)
            .unwrap_or(0)
    };
    let ladder_height = if out_height == 0 {
        if source_height > 0 { source_height } else { 1080 }
    } else {
        out_height
    };
    let bitrate = video_quality::bitrate_kbps(ladder_height, &codec);
    // Capture plan: the backend's live scaler proved soft on text at
    // non-integer ratios (1080p->720p), so when the target sits below the
    // source we buffer at source resolution and downscale with lanczos on
    // save. Otherwise capture directly at the requested height.
    let (capture_height, buffer_bitrate, save_height, save_bitrate) =
        if out_height != 0 && source_height > 0 && out_height < source_height {
            (0, video_quality::bitrate_kbps(source_height, &codec), out_height, bitrate)
        } else {
            (out_height, bitrate, 0, bitrate)
        };
    // NVENC HQ opts only where valid (NVIDIA + h264/hevc); elsewhere backend defaults.
    let nvenc_opts = if vendor == "nvidia" && (codec == "h264" || codec == "hevc") {
        Some(video_quality::nvenc_hq_opts(&codec))
    } else {
        None
    };
    // Save-time encoder per GPU vendor (None = keep source file on save).
    let save_encoder = video::transcode_encoder(&vendor, &codec);
    eprintln!("[moonlit] video: codec={codec} height={} fps={fps} vendor={vendor} cbr={bitrate}kbps nvenc_hq={} monitor={} capture={} save={} save_enc={:?}",
        if out_height == 0 { "source".to_string() } else { out_height.to_string() },
        nvenc_opts.is_some(),
        if monitor.trim().is_empty() { "auto".to_string() } else { monitor.clone() },
        if capture_height == 0 { "source".to_string() } else { capture_height.to_string() },
        if save_height == 0 { "-".to_string() } else { format!("lanczos->{save_height}p") },
        save_encoder);

    let mut engine = Engine::new();
    // ffmpeg for encode/mux/probe: bundled sidecar first (embedded, ships
    // with the installer), dev PATH fallback. Never optional in practice —
    // resolve_ffmpeg always returns at least the PATH fallback.
    let ffmpeg_bin = crate::editor::ffmpeg::resolve_ffmpeg(app).ok();
    engine
        .start_buffer(CaptureConfig {
            duration_seconds: secs,
            fps,
            output_dir: dir,
            gsr_bin: Some(gsr_bin),
            source: monitor,
            desktop_device,
            mic_device,
            codec,
            out_height: capture_height,
            bitrate_kbps: buffer_bitrate,
            save_height,
            save_bitrate_kbps: save_bitrate,
            save_encoder,
            nvenc_opts,
            ffmpeg_bin,
        })
        .await?;
    let tracks = audio::linked_count(&engine.audio_args()).await;
    let status = EngineStatus {
        running: true,
        backend: engine.backend_name().to_string(),
        tracks_linked: tracks,
        audio_error: read_audio_error(app).await,
    };
    {
        let st = app.state::<AppState>();
        *st.recorder.lock().await = Some(engine);
    }
    // Apply persisted per-track gains to the fresh GSR streams (best effort).
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        match apply_saved_gains(&app2).await {
            Ok(n) => {
                eprintln!("[moonlit] gains applied to {n} tracks");
                set_audio_error(&app2, None).await;
            }
            Err(e) => {
                eprintln!("[moonlit] gain apply failed: {e}");
                set_audio_error(&app2, Some(e)).await;
            }
        }
    });
    Ok(status)
}

fn setting_str(db: &DbState, key: &str, default: &str) -> String {
    db.get_settings()
        .ok()
        .and_then(|s| s.get(key).cloned())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

async fn stop_engine(app: &AppHandle) -> Result<EngineStatus, String> {
    let st = app.state::<AppState>();
    let mut guard = st.recorder.lock().await;
    if let Some(mut engine) = guard.take() {
        engine.stop_buffer().await?;
    }
    drop(guard);
    set_audio_error(app, None).await;
    Ok(EngineStatus {
        running: false,
        backend: backend_name().to_string(),
        tracks_linked: 0,
        audio_error: None,
    })
}

#[tauri::command]
pub async fn set_setting(app: AppHandle, key: String, value: String) -> Result<(), String> {
    {
        let db = app.state::<DbState>();
        db.set_setting(&key, &value)?;
    }
    // Changing the buffer length or capture devices with the engine running
    // restarts it so length, devices and stored durations match the recorder.
    const RESTART_KEYS: &[&str] = &[
        "buffer_seconds",
        "mic_device",
        "desktop_device",
        "video_codec",
        "out_height",
        "fps",
        "monitor",
    ];
    if RESTART_KEYS.contains(&key.as_str()) {
        let running = {
            let st = app.state::<AppState>();
            let guard = st.recorder.lock().await;
            let r = guard.is_some();
            drop(guard);
            r
        };
        if running {
            stop_engine(&app).await?;
            start_engine(&app).await?;
            notify(&app, "Búfer reiniciado con la nueva configuración", "Buffer restarted with new configuration");
        }
    }
    Ok(())
}

#[tauri::command]
pub fn list_custom_apps(db: State<'_, DbState>) -> Result<Vec<CustomApp>, String> {
    db.list_custom_apps()
}

#[tauri::command]
pub fn register_app(
    db: State<'_, DbState>,
    input: RegisterAppInput,
) -> Result<CustomApp, String> {
    db.register_app(input)
}

#[tauri::command]
pub fn delete_app(db: State<'_, DbState>, id: String) -> Result<(), String> {
    db.delete_app(&id)
}

#[tauri::command]
pub fn secret_store(alias: String, value: String) -> Result<(), String> {
    secrets::store_secret(&alias, &value)
}

#[tauri::command]
pub fn secret_get(alias: String) -> Result<String, String> {
    secrets::get_secret(&alias)
}

#[tauri::command]
pub fn secret_delete(alias: String) -> Result<(), String> {
    secrets::delete_secret(&alias)
}

// ---------------------------------------------------------------------------
// Capture (Phase 3)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct EngineStatus {
    pub running: bool,
    pub backend: String,
    /// GSR audio tracks currently linked (0, 1 or 2). UI-visible, no silent fails.
    pub tracks_linked: usize,
    /// Last audio-gain apply error, if any.
    pub audio_error: Option<String>,
}

async fn read_audio_error(app: &AppHandle) -> Option<String> {
    let st = app.try_state::<AppState>()?;
    let guard = st.audio_error.lock().await;
    guard.clone()
}

async fn set_audio_error(app: &AppHandle, err: Option<String>) {
    if let Some(st) = app.try_state::<AppState>() {
        *st.audio_error.lock().await = err;
    }
}

fn buffer_seconds(db: &DbState) -> i64 {
    db.get_settings()
        .ok()
        .and_then(|s| s.get("buffer_seconds").and_then(|v| v.parse().ok()))
        .unwrap_or(30)
        .clamp(5, 300)
}

fn is_spanish(app: &AppHandle) -> bool {
    app.try_state::<DbState>()
        .and_then(|db| db.get_settings().ok())
        .and_then(|s| s.get("locale").cloned())
        .map(|l| l.starts_with("es"))
        .unwrap_or(true)
}

pub fn notify(app: &AppHandle, body_es: &str, body_en: &str) {
    use tauri_plugin_notification::NotificationExt;
    let body = if is_spanish(app) { body_es } else { body_en };
    let _ = app.notification().builder().title("MoonLit").body(body).show();
}

#[tauri::command]
pub async fn start_buffer(app: AppHandle) -> Result<EngineStatus, String> {
    {
        let st = app.state::<AppState>();
        if st.recorder.lock().await.is_some() {
            return Err("buffer already running".into());
        }
    }
    start_engine(&app).await
}

#[tauri::command]
pub async fn stop_buffer(app: AppHandle) -> Result<EngineStatus, String> {
    stop_engine(&app).await
}

#[tauri::command]
pub async fn engine_status(app: AppHandle) -> Result<EngineStatus, String> {
    let st = app.state::<AppState>();
    let guard = st.recorder.lock().await;
    let (running, args) = (guard.is_some(), guard.as_ref().map(|e| e.audio_args()).unwrap_or_default());
    drop(guard);
    let tracks = if running {
        audio::linked_count(&args).await
    } else {
        0
    };
    Ok(EngineStatus {
        running,
        backend: backend_name().to_string(),
        tracks_linked: tracks,
        audio_error: read_audio_error(&app).await,
    })
}

/// Full save pipeline: flush ring -> thumbnail -> DB index -> ding -> event.
/// Stage timings go to the backend log (`[moonlit] save ...`) so slow saves
/// can be attributed instead of guessed.
pub(crate) async fn do_save_clip(app: &AppHandle) -> Result<ClipRecord, String> {
    let t_total = std::time::Instant::now();
    let path = {
        let st = app.state::<AppState>();
        let mut guard = st.recorder.lock().await;
        let eng = guard
            .as_mut()
            .ok_or_else(|| "buffer not running".to_string())?;
        eng.save_clip().await?
    };
    let t_engine = t_total.elapsed();
    let db = app.state::<DbState>();
    // Same-second double saves collide: GSR names files by timestamp, so the
    // second file overwrites the first on disk and the DB rejects the duplicate.
    // Rename to stem_2.mp4, stem_3.mp4… instead of failing and losing the clip.
    let mut path = path;
    {
        let taken: std::collections::HashSet<String> = db
            .list_clips()
            .map(|clips| clips.into_iter().map(|c| c.file_name).collect())
            .unwrap_or_default();
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if taken.contains(name) {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .ok_or("bad clip file name")?
                    .to_string();
                let ext = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("mp4");
                let mut n = 2u32;
                loop {
                    let cand = path.with_file_name(format!("{stem}_{n}.{ext}"));
                    let cand_name = cand
                        .file_name()
                        .and_then(|s| s.to_str())
                        .ok_or("bad clip file name")?;
                    if !taken.contains(cand_name) && !cand.exists() {
                        tokio::fs::rename(&path, &cand)
                            .await
                            .map_err(|e| format!("cannot dedupe clip name: {e}"))?;
                        path = cand;
                        break;
                    }
                    n += 1;
                    if n > 999 {
                        return Err("cannot find a free clip name".into());
                    }
                }
            }
        }
    }
    let base = db.clips_dir()?;
    let ffmpeg = crate::editor::ffmpeg::resolve_ffmpeg(app)?;
    // Deliver the requested height: the buffer ran at source resolution, so
    // downscale now with lanczos (NVENC). On any failure keep the source
    // file — a source-res clip beats no clip.
    {
        let st = app.state::<AppState>();
        let guard = st.recorder.lock().await;
        let plan = guard.as_ref().and_then(|e| e.save_plan());
        drop(guard);
        if let Some(p) = plan {
            let t0 = std::time::Instant::now();
            let tmp = path.with_extension("scaled.mp4");
            let ok = match p.encoder {
                Some(enc) => {
                    crate::editor::ffmpeg::scale_to_height(
                        &ffmpeg, &path, &tmp, p.height, p.bitrate_kbps, enc, &p.codec, p.fps,
                    )
                    .await
                }
                // No save-time encoder on this GPU (e.g. AMD/VAAPI without
                // validated render-node plumbing): keep the source file.
                None => {
                    eprintln!("[moonlit] no save encoder for this GPU, keeping source resolution");
                    false
                }
            };
            if ok {
                if let Err(e) = tokio::fs::rename(&tmp, &path).await {
                    eprintln!("[moonlit] scaled replace failed: {e}");
                    let _ = tokio::fs::remove_file(&tmp).await;
                } else {
                    eprintln!("[moonlit] lanczos save-scale to {}p in {:?}", p.height, t0.elapsed());
                }
            } else {
                eprintln!("[moonlit] save-scale failed, keeping source resolution");
                let _ = tokio::fs::remove_file(&tmp).await;
            }
        }
    }
    let size = tokio::fs::metadata(&path)
        .await
        .map_err(|e| format!("cannot stat clip: {e}"))?
        .len() as i64;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("bad clip file name")?;
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("bad clip file name")?
        .to_string();
    let thumb_name = format!("thumb_{stem}.jpg");
    let thumb_path = base.join(&thumb_name);
    // Duration probe + thumbnail are independent (same input): run them
    // together instead of paying two cold ffmpeg spawns in series.
    // Thumbnail tries 1 s first; a sub-second clip retries near the head.
    let t_tail = std::time::Instant::now();
    let (probe_ms, thumb_res) = tokio::join!(
        crate::editor::ffmpeg::probe_duration_ms(&ffmpeg, &path),
        crate::editor::ffmpeg::make_thumbnail(&ffmpeg, &path, &thumb_path, 1.0),
    );
    // Real measured duration (the buffer is rarely full at save time).
    // Falls back to the configured length only if probing fails.
    let secs_ms = probe_ms.unwrap_or_else(|| buffer_seconds(&db) * 1000);
    if let Err(e) = thumb_res {
        if secs_ms < 1500 {
            crate::editor::ffmpeg::make_thumbnail(&ffmpeg, &path, &thumb_path, 0.05).await?;
        } else {
            return Err(e);
        }
    }
    let t_tail_elapsed = t_tail.elapsed();
    let t_db = std::time::Instant::now();
    let clip = db.insert_clip(&file_name, &thumb_name, "Unknown", secs_ms, size)?;
    eprintln!("[moonlit] save total={:?} engine={t_engine:?} probe+thumb={t_tail_elapsed:?} db={:?} size={}MB",
        t_total.elapsed(), t_db.elapsed(), size / 1024 / 1024);
    crate::cue::play_ding();
    let _ = app.emit("moonlit://clip-saved", &clip);
    Ok(clip)
}

#[tauri::command]
pub async fn save_clip_now(app: AppHandle) -> Result<ClipRecord, String> {
    do_save_clip(&app).await
}

/// One-time correction: measure real durations for rows saved before probing
/// existed (the settings value was stored instead). Skips missing files and
/// rows already within 1.5 s of measured. Runs once at boot in background.
pub(crate) async fn backfill_durations(app: &AppHandle) {
    let Some(db) = app.try_state::<DbState>() else {
        return;
    };
    let Ok(clips) = db.list_clips() else { return };
    let Ok(base) = db.clips_dir() else { return };
    let Ok(ffmpeg) = crate::editor::ffmpeg::resolve_ffmpeg(app) else {
        return;
    };
    for clip in clips {
        let path = base.join(&clip.file_name);
        if !path.exists() {
            continue;
        }
        if let Some(ms) = crate::editor::ffmpeg::probe_duration_ms(&ffmpeg, &path).await {
            if (ms - clip.duration_ms).abs() > 1500 {
                let _ = db.update_duration(&clip.id, ms);
            }
        }
    }
}

/// F9 entry point: counter event always fires; clip saves only when running.
pub(crate) async fn handle_hotkey(app: AppHandle, shortcut: String, pressed_at: String) {
    let _ = app.emit(
        "moonlit://clip-hotkey",
        serde_json::json!({ "shortcut": shortcut, "pressed_at": pressed_at }),
    );
    let st = app.state::<AppState>();
    let guard = st.recorder.lock().await;
    let running = guard.is_some();
    drop(guard);
    if !running {
        notify(&app, "Búfer detenido — pulsa Start para grabar", "Buffer stopped — press Start to record");
        return;
    }
    match do_save_clip(&app).await {
        Ok(clip) => notify(
            &app,
            &format!("Clip guardado: {}", clip.file_name),
            &format!("Clip saved: {}", clip.file_name),
        ),
        Err(e) => notify(&app, &format!("Error al guardar: {e}"), &format!("Save failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Live capture gain + backend info (Phase 3)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct TrackGains {
    pub game: u32,
    pub mic: u32,
    pub mute_game: bool,
    pub mute_mic: bool,
}

fn read_gains(app: &AppHandle) -> TrackGains {
    let map = app
        .try_state::<DbState>()
        .and_then(|db| db.get_settings().ok())
        .unwrap_or_default();
    let num = |k: &str, d: u32| {
        map.get(k)
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
            .clamp(0, 200)
    };
    let flag = |k: &str| map.get(k).map(|v| v == "1" || v == "true").unwrap_or(false);
    TrackGains {
        game: num("gain_game", 100),
        mic: num("gain_mic", 100),
        mute_game: flag("mute_game"),
        mute_mic: flag("mute_mic"),
    }
}

async fn engine_audio_args(app: &AppHandle) -> Vec<String> {
    let st = app.state::<AppState>();
    let guard = st.recorder.lock().await;
    let args = guard.as_ref().map(|e| e.audio_args()).unwrap_or_default();
    drop(guard);
    args
}

async fn apply_saved_gains(app: &AppHandle) -> Result<usize, String> {
    let g = read_gains(app);
    let args = engine_audio_args(app).await;
    audio::apply_gains(&args, g.game, g.mic, g.mute_game, g.mute_mic).await
}

#[tauri::command]
pub async fn audio_levels(app: AppHandle) -> Result<TrackGains, String> {
    Ok(read_gains(&app))
}

fn check_track(track: &str) -> Result<(), String> {
    if track == "game" || track == "mic" {
        Ok(())
    } else {
        Err("track must be 'game' or 'mic'".into())
    }
}

#[tauri::command]
pub async fn set_track_gain(app: AppHandle, track: String, percent: u32) -> Result<TrackGains, String> {
    check_track(&track)?;
    let pct = percent.clamp(0, 200);
    {
        let db = app.state::<DbState>();
        db.set_setting(if track == "game" { "gain_game" } else { "gain_mic" }, &pct.to_string())?;
    }
    // Live-apply if the buffer is running; persisting alone is fine otherwise.
    let running = {
        let st = app.state::<AppState>();
        let guard = st.recorder.lock().await;
        let r = guard.is_some();
        drop(guard);
        r
    };
    if running {
        apply_saved_gains(&app).await?;
    }
    Ok(read_gains(&app))
}

#[tauri::command]
pub async fn set_track_mute(app: AppHandle, track: String, muted: bool) -> Result<TrackGains, String> {
    check_track(&track)?;
    {
        let db = app.state::<DbState>();
        db.set_setting(
            if track == "game" { "mute_game" } else { "mute_mic" },
            if muted { "1" } else { "0" },
        )?;
    }
    let running = {
        let st = app.state::<AppState>();
        let guard = st.recorder.lock().await;
        let r = guard.is_some();
        drop(guard);
        r
    };
    if running {
        apply_saved_gains(&app).await?;
    }
    Ok(read_gains(&app))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GsrInfo {
    pub path: String,
    pub source: String,
    pub caps_ok: bool,
    pub present: bool,
}

#[tauri::command]
pub async fn gsr_info(app: AppHandle) -> Result<GsrInfo, String> {
    match binary::backend_binary(&app) {
        Ok((path, source)) => {
            let caps_ok = caps::caps_ok(&path);
            Ok(GsrInfo {
                path: path.to_string_lossy().to_string(),
                source: source.to_string(),
                caps_ok,
                present: true,
            })
        }
        Err(_) => Ok(GsrInfo {
            path: String::new(),
            source: "missing".to_string(),
            caps_ok: false,
            present: false,
        }),
    }
}

/// One-click KMS permission fix (polkit dialog) for OUR bundled binary.
#[tauri::command]
pub async fn fix_gsr_caps(app: AppHandle) -> Result<(), String> {
    let (path, source) = binary::backend_binary(&app)?;
    if source != "bundled" {
        return Err("one-click fix applies to the bundled binary only".into());
    }
    caps::fix_caps(&path).await
}

/// Capture devices (Linux: bundled GSR query; Windows: cpal enumeration).
#[tauri::command]
pub async fn list_audio_devices() -> Result<Vec<AudioDevice>, String> {
    devices::list_audio_devices().await
}

/// Video options for the Settings UI: codec ids from the backend, ladder
/// heights, Medal bitrates and exact RAM estimates (CBR => exact, not ranges).
/// NOTE: no human text crosses IPC — labels/notes live in frontend locales.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CodecOpt {
    pub id: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HeightOpt {
    pub height: u32,
    pub label: String,
    /// CBR kbps per codec at this height, in codec order.
    pub bitrates: Vec<u32>,
    /// Exact 60 s ring megabytes per codec, in codec order.
    pub ring_mb_60s: Vec<u32>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MonitorOpt {
    pub name: String,
    pub label: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VideoOptions {
    pub codecs: Vec<CodecOpt>,
    pub heights: Vec<HeightOpt>,
    pub monitors: Vec<MonitorOpt>,
    pub current_codec: String,
    pub current_height: u32,
    pub current_fps: u32,
    pub current_monitor: String,
    /// Height the ring buffer actually runs at (source when transcoding).
    pub buffer_height: u32,
    /// Whether saves downscale with lanczos (buffer at source).
    pub transcoding: bool,
    pub max_source_height: u32,
    pub vendor: String,
}

#[tauri::command]
pub async fn video_options(app: AppHandle) -> Result<VideoOptions, String> {
    use crate::video_quality as q;
    // Native backends (Windows WGC) have no sidecar binary; their probes
    // ignore this path.
    let gsr_bin: PathBuf = binary::backend_binary(&app)
        .map(|(p, _)| p)
        .unwrap_or_default();
    let vendor = video::vendor(&gsr_bin).await;

    // Codec ids the backend reports, filtered to known-good entries.
    // Probes run against the SHIPPED ffmpeg (bundled sidecar first), never
    // an incidental PATH binary — production machines may not have one.
    // Labels/notes are frontend-owned (locales) — never hardcode UI text here.
    let ffmpeg = crate::editor::ffmpeg::resolve_ffmpeg(&app)
        .unwrap_or_else(|_| PathBuf::from("ffmpeg"));
    let mut codecs: Vec<CodecOpt> = Vec::new();
    for id in video::offered_codecs(&gsr_bin, &ffmpeg).await {
        if matches!(id.as_str(), "h264" | "hevc" | "av1" | "x264")
            && !codecs.iter().any(|c: &CodecOpt| c.id == id)
        {
            codecs.push(CodecOpt { id });
        }
    }
    if codecs.is_empty() {
        // Fallback (Windows stub / unknown backend): H.264 always exists.
        codecs.push(CodecOpt { id: "h264".into() });
    }

    let db = app.state::<DbState>();
    let current_codec = setting_str(&db, "video_codec", "h264");
    let current_height: u32 = setting_str(&db, "out_height", "0").parse().unwrap_or(0);
    let current_fps: u32 = match setting_str(&db, "fps", "60").parse().unwrap_or(60) {
        30 => 30,
        _ => 60,
    };
    let listed = video::list_monitors(&gsr_bin).await;
    let max_source_height = listed.iter().map(|m| m.height).max().unwrap_or(0);
    let current_monitor = setting_str(&db, "monitor", "");
    let monitors = listed
        .iter()
        .map(|m| MonitorOpt {
            name: m.name.clone(),
            label: format!("{} ({}×{})", m.name, m.width, m.height),
        })
        .collect::<Vec<_>>();
    let source_height = if current_monitor.trim().is_empty() {
        max_source_height
    } else {
        listed
            .iter()
            .find(|m| m.name == current_monitor.trim())
            .map(|m| m.height)
            .unwrap_or(max_source_height)
    };
    // Same capture plan as start_engine: buffer at source when downscaling.
    let transcoding =
        current_height != 0 && source_height > 0 && current_height < source_height;
    let buffer_height = if transcoding { source_height } else { current_height };
    let heights = q::HEIGHTS
        .iter()
        .map(|&h| {
            let bitrates = codecs.iter().map(|c| q::bitrate_kbps(h, &c.id)).collect::<Vec<_>>();
            let ring = bitrates.iter().map(|&b| q::ring_mb(b, 60)).collect::<Vec<_>>();
            HeightOpt {
                height: h,
                label: format!("{h}p"),
                bitrates,
                ring_mb_60s: ring,
            }
        })
        .collect();

    Ok(VideoOptions {
        codecs,
        heights,
        monitors,
        current_codec,
        current_height,
        current_fps,
        current_monitor,
        buffer_height,
        transcoding,
        max_source_height,
        vendor,
    })
}
/// Open a clip with the system default player, entirely from the backend.
///
/// Rationale: frontend `openPath` goes through IPC capability checks
/// (`opener:allow-open-path`); doing it here bypasses that layer, so one
/// fewer thing can silently break. Every step is logged (backend log IS
/// visible to developers) and failures name their layer. Fallback chain:
/// opener crate -> OS launcher (`xdg-open` / `cmd /C start`).
/// NOTE: Tauri camelCases Rust params on the wire: frontend sends `clipId`,
/// never `clip_id` (see docs/01_ARCHITECTURE.md IPC rule).
#[tauri::command]
pub async fn open_clip_external(app: AppHandle, clip_id: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let db = app.state::<DbState>();
    let base = db.clips_dir()?;
    let clips = db.list_clips()?;
    let clip = clips
        .into_iter()
        .find(|c| c.id == clip_id)
        .ok_or("clip not found")?;
    let abs = base.join(&clip.file_name);
    eprintln!("[moonlit] open_clip_external: {}", abs.display());
    if !abs.exists() {
        return Err(format!("file gone from disk: {}", clip.file_name));
    }
    match app.opener().open_path(abs.to_string_lossy(), None::<&str>) {
        Ok(()) => {
            eprintln!("[moonlit] open_clip_external: opener ok");
            return Ok(());
        }
        Err(e) => eprintln!("[moonlit] open_clip_external: opener failed ({e}), trying OS launcher"),
    }
    // OS launcher lives in os::open — no cfg here (zero-cfg rule).
    match open::open_external(&abs) {
        Ok(()) => {
            eprintln!("[moonlit] open_clip_external: OS launcher ok");
            Ok(())
        }
        Err(e) => Err(format!("opener + OS launcher both failed ({e})")),
    }
}

/// NOTE: Tauri camelCases Rust params on the wire: frontend sends `clipId`,
/// never `clip_id` (see docs/01_ARCHITECTURE.md IPC rule).
#[tauri::command]
pub async fn preview_track(app: AppHandle, clip_id: String, track: u32) -> Result<String, String> {    if track < 1 || track > 3 {
        return Err("track must be 1 (mix), 2 (game) or 3 (mic)".into());
    }
    let db = app.state::<DbState>();
    let clips = db.list_clips()?;
    let clip = clips
        .into_iter()
        .find(|c| c.id == clip_id)
        .ok_or("clip not found")?;
    let base = db.clips_dir()?;
    let input = base.join(&clip.file_name);
    let preview = std::env::temp_dir().join("moonlit-track-preview.m4a");
    let ffmpeg = crate::editor::ffmpeg::resolve_ffmpeg(&app)?;
    let status = tokio::process::Command::new(&ffmpeg)
        .args([
            "-y", "-hide_banner", "-loglevel", "error",
            "-i", &input.to_string_lossy(),
            "-map", &format!("0:{track}"),
            "-c:a", "aac",
        ])
        .arg(&preview)
        .status()
        .await
        .map_err(|e| format!("preview extract failed: {e}"))?;
    if !status.success() {
        return Err("preview extract failed".into());
    }
    Ok(preview.to_string_lossy().to_string())
}
