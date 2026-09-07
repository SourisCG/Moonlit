//! Windows engine: WGC frame acquisition → ffmpeg encode → RAM rings.
//!
//! Replay semantics mirror Linux GSR (`-r` ring + remux on hotkey): encoded
//! video packets (MPEG-TS) and PCM audio stems live in RAM only — zero disk
//! writes while idle. `save_clip` cuts the window, muxes 3×AAC and delivers
//! the `.mp4`. No DLL injection anywhere (WGC is the same OS API Xbox Game
//! Bar uses — anti-cheat safe). OS floor: Windows 10 1903+.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::Duration;

use tokio::process::{Child, Command};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use super::super::{CaptureConfig, CaptureEngine, SavePlan};
use super::audio::AudioCapture;
use super::video;

/// Frames in flight between the WGC callback and the ffmpeg writer.
/// Small: backpressure means the encoder is behind, and a replay buffer
/// prefers dropping a frame over growing latency.
const FRAME_QUEUE: usize = 4;
/// Extra seconds kept beyond the configured buffer (cut margin + headroom).
const RING_MARGIN_SECS: u64 = 8;
/// How long `start_buffer` waits for the encoder to prove it is alive.
const ENCODER_PROOF_MS: u64 = 800;

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested)
// ---------------------------------------------------------------------------

/// ffmpeg args for the live encoder **after** `-c:v <enc>`. CBR ladder +
/// 2 s GOP everywhere (Medal parity); HQ knobs only where valid
/// (NVIDIA + h264/hevc, same rule as GSR `nvenc_opts`).
pub fn live_encoder_args(
    enc_name: &str,
    codec: &str,
    bitrate_kbps: u32,
    fps: u32,
    nvenc_hq: bool,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut flag = |k: &str, v: &str| {
        out.push(k.to_string());
        out.push(v.to_string());
    };
    if enc_name.ends_with("_nvenc") {
        if nvenc_hq {
            // Full old-MoonLit HQ recipe (2.2): P7 + HQ + profile + BF2 +
            // Spatial AQ + single-pass. Same set GSR runs validated live.
            flag("-preset", "p7");
            flag("-tune", "hq");
            flag("-profile:v", if codec == "hevc" { "main" } else { "high" });
            flag("-bf", "2");
            flag("-spatial-aq", "1");
            flag("-multipass", "disabled");
        }
        flag("-rc", "cbr");
    } else if enc_name.ends_with("_amf") {
        flag("-rc", "cbr");
        flag("-quality", "quality");
    } else if enc_name.ends_with("_qsv") {
        // Live constraint (unlike offline save-scale): stay `fast` so a
        // 60 fps game never stalls the encoder.
        flag("-preset", "fast");
    } else if enc_name == "libx264" {
        flag("-preset", "veryfast");
        flag("-tune", "zerolatency");
        flag("-profile:v", "high");
        flag("-bf", "2");
    }
    let gop = (fps.max(1) * 2).to_string();
    flag("-b:v", &format!("{bitrate_kbps}k"));
    flag("-maxrate", &format!("{bitrate_kbps}k"));
    flag("-bufsize", &format!("{bitrate_kbps}k"));
    flag("-g", &gop);
    out
}

/// Output cadence for the frame pump: WGC only delivers on change, so a
/// quiet desktop would collapse the encoded timeline. The pump re-emits the
/// latest frame at exactly this cadence (classic CFR pacing) — wall-clock
/// duration always matches the session length. Pure (unit-tested).
fn pace_interval_ms(fps: u32) -> u64 {
    1000 / fps.max(1) as u64
}

/// Frame pump body: fresh frames win; on timeout the last frame is
/// re-emitted to hold CFR. A deadline catch-up covers slow pipe writes:
/// without it each 8 MB write stretches the cadence and the timeline slips
/// behind wall-clock. Bounded (8/iter) so a hopeless encoder can't spiral.
/// Generic over the writer for hermetic tests.
fn pump_frames<W: Write>(
    rx: mpsc::Receiver<Vec<u8>>,
    stdin: &mut W,
    interval_ms: u64,
    fps: u32,
    halt: &AtomicBool,
    dead: &AtomicBool,
    frames_in: &std::sync::atomic::AtomicU64,
    frames_out: &std::sync::atomic::AtomicU64,
) {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    let mut last: Option<Vec<u8>> = None;
    macro_rules! emit {
        ($frame:expr) => {
            if stdin.write_all($frame).is_err() {
                dead.store(true, Ordering::Relaxed);
                return;
            }
            frames_out.fetch_add(1, Ordering::Relaxed);
        };
    }
    loop {
        if halt.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(interval_ms)) {
            Ok(frame) => {
                frames_in.fetch_add(1, Ordering::Relaxed);
                emit!(&frame);
                last = Some(frame);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(prev) = &last {
                    emit!(prev);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // Catch-up: wall-clock frames owed vs frames written.
        if last.is_some() {
            let expected = t0.elapsed().as_millis() as u64 * fps.max(1) as u64 / 1000;
            let mut burst = 0;
            while frames_out.load(Ordering::Relaxed) < expected && burst < 8 {
                if halt.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(prev) = &last {
                    emit!(prev);
                }
                burst += 1;
            }
        }
    }
    let _ = stdin.flush();
}

/// Clip file name in the GSR style (`replay_YYYY-MM-DD_HH-MM-SS.mp4`) so
/// Windows clips sort and read exactly like Linux ones.
fn replay_filename() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    format!(
        "replay_{:04}-{:02}-{:02}_{:02}-{:02}-{:02}.mp4",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// Resync a TS window: first offset where `0x47` starts an aligned pair.
/// Returns the whole window when no sync is found (caller still delivers).
fn cut_ts_window(ring: &[u8], keep_bytes: usize) -> &[u8] {
    let start = ring.len().saturating_sub(keep_bytes);
    let win = &ring[start..];
    let scan = win.len().min(188 * 8);
    for i in 0..scan {
        if win[i] == 0x47 && (i + 188 >= win.len() || win[i + 188] == 0x47) {
            return &win[i..];
        }
    }
    win
}

/// dBFS for the save stats line (-inf when the stem is digital silence).
fn dbfs(v: f32) -> String {
    if v <= 0.0 {
        "-inf".to_string()
    } else {
        format!("{:.1}dB", 20.0 * v.log10())
    }
}
/// MIX track from the two solo tails. Both stems share the 48 kHz stereo
/// grid and both tails end at "now", so equal-length tails are sample
/// aligned and sum to a true mix (Linux parity: mix plays everywhere).
/// Shorter side decides the length; the WAV writer clamps the sum.
fn mix_stems(game: &[f32], mic: &[f32]) -> Vec<f32> {
    let n = game.len().min(mic.len());
    game[..n].iter().zip(&mic[..n]).map(|(g, m)| g + m).collect()
}

/// Minimal PCM-16 WAV writer (stereo 48 kHz). No extra crate needed.
/// Batched through a 1 MB BufWriter: per-sample syscalls made this stage
/// cost 7 s for 3×30 s stems (8.6M writes); now a handful of block writes.
fn write_wav(path: &Path, samples: &[f32]) -> std::io::Result<()> {
    use std::io::{BufWriter, Write};
    let mut pcm = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        pcm.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    let data_bytes = pcm.len() as u32;
    let mut hdr = [0u8; 44];
    hdr[0..4].copy_from_slice(b"RIFF");
    hdr[4..8].copy_from_slice(&(36 + data_bytes).to_le_bytes());
    hdr[8..12].copy_from_slice(b"WAVE");
    hdr[12..16].copy_from_slice(b"fmt ");
    hdr[16..20].copy_from_slice(&16u32.to_le_bytes());
    hdr[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    hdr[22..24].copy_from_slice(&2u16.to_le_bytes()); // stereo
    hdr[24..28].copy_from_slice(&48_000u32.to_le_bytes());
    hdr[28..32].copy_from_slice(&(48_000u32 * 2 * 2).to_le_bytes());
    hdr[32..34].copy_from_slice(&4u16.to_le_bytes());
    hdr[34..36].copy_from_slice(&16u16.to_le_bytes());
    hdr[36..40].copy_from_slice(b"data");
    hdr[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    let f = std::fs::File::create(path)?;
    let mut w = BufWriter::with_capacity(1024 * 1024, f);
    w.write_all(&hdr)?;
    w.write_all(&pcm)?;
    w.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// WGC plumbing
// ---------------------------------------------------------------------------

struct EngineShared {
    tx: Mutex<mpsc::SyncSender<Vec<u8>>>,
    halt: AtomicBool,
    width: u32,
    height: u32,
    size_warned: AtomicBool,
}

struct FrameHandler {
    shared: Arc<EngineShared>,
    scratch: Vec<u8>,
}

impl GraphicsCaptureApiHandler for FrameHandler {
    type Flags = Arc<EngineShared>;
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            shared: ctx.flags,
            scratch: Vec::new(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.shared.halt.load(Ordering::Relaxed) {
            return Ok(());
        }
        // A mid-run resize would corrupt the rawvideo stream (fixed `-s`);
        // drop those frames loudly instead of poisoning the ring.
        if frame.width() != self.shared.width || frame.height() != self.shared.height {
            if !self.shared.size_warned.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[moonlit] monitor size changed mid-buffer ({}x{}), dropping frames until restart",
                    frame.width(),
                    frame.height()
                );
            }
            return Ok(());
        }
        match frame.buffer() {
            Ok(buf) => {
                let bytes = buf.as_nopadding_buffer(&mut self.scratch);
                if let Ok(tx) = self.shared.tx.lock() {
                    // Full queue = encoder behind: drop, never block capture.
                    let _ = tx.try_send(bytes.to_vec());
                }
            }
            Err(e) => eprintln!("[moonlit] frame buffer failed: {e}"),
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        eprintln!("[moonlit] WGC session closed by the OS");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

pub struct WindowsCaptureEngine {
    control: Option<CaptureControl<FrameHandler, String>>,
    video_child: Option<Child>,
    video_ring: Arc<Mutex<Vec<u8>>>,
    video_dead: Arc<AtomicBool>,
    ring_cap: usize,
    audio: Option<AudioCapture>,
    output_dir: PathBuf,
    audio_args: Vec<String>,
    save_plan: Option<SavePlan>,
    duration_secs: u32,
    bitrate_kbps: u32,
    fps: u32,
    /// ffmpeg for encode/mux/probe (bundled sidecar first, passed in by
    /// shared startup code which owns the AppHandle).
    ffmpeg: PathBuf,
    /// Pump telemetry: WGC frames in vs bytes-written frames out.
    frames_in: Arc<std::sync::atomic::AtomicU64>,
    frames_out: Arc<std::sync::atomic::AtomicU64>,
}

impl WindowsCaptureEngine {
    pub fn new() -> Self {
        Self {
            control: None,
            video_child: None,
            video_ring: Arc::new(Mutex::new(Vec::new())),
            video_dead: Arc::new(AtomicBool::new(false)),
            ring_cap: 0,
            audio: None,
            output_dir: PathBuf::new(),
            audio_args: Vec::new(),
            save_plan: None,
            duration_secs: 0,
            bitrate_kbps: 0,
            fps: 60,
            ffmpeg: PathBuf::new(),
            frames_in: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            frames_out: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    fn spawn_video_encoder(
        &self,
        ffmpeg: &Path,
        enc_name: &str,
        codec: &str,
        mw: u32,
        mh: u32,
        out_height: u32,
        bitrate_kbps: u32,
        fps: u32,
        nvenc_hq: bool,
    ) -> Result<(Child, mpsc::SyncSender<Vec<u8>>), String> {
        let vf = if out_height > 0 && out_height != mh {
            format!("scale=-2:{out_height},fps={fps},format=yuv420p")
        } else {
            format!("fps={fps},format=yuv420p")
        };
        let mut cmd = Command::new(ffmpeg);
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-thread_queue_size",
            "32",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "bgra",
            "-s",
            &format!("{mw}x{mh}"),
            "-framerate",
            &fps.to_string(),
            "-i",
            "pipe:0",
            "-an",
            "-vf",
            &vf,
            "-c:v",
            enc_name,
        ]);
        cmd.args(live_encoder_args(enc_name, codec, bitrate_kbps, fps, nvenc_hq));
        cmd.args(["-f", "mpegts", "pipe:1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot launch video encoder ({}): {e}", ffmpeg.display()))?;
        // Hand the pipes to blocking std threads (the pump and the drain
        // must never stall the async runtime on 8 MB frame writes).
        let mut stdin: std::process::ChildStdin = child
            .stdin
            .take()
            .ok_or_else(|| "encoder stdin unavailable".to_string())?
            .into_owned_handle()
            .map_err(|e| format!("encoder stdin handoff failed: {e}"))?
            .into();
        let stdout: std::process::ChildStdout = child
            .stdout
            .take()
            .ok_or_else(|| "encoder stdout unavailable".to_string())?
            .into_owned_handle()
            .map_err(|e| format!("encoder stdout handoff failed: {e}"))?
            .into();
        // Frame pump (CFR-paced): WGC callback -> ffmpeg stdin.
        // WGC only delivers on change; the pacer re-emits the latest frame
        // so the encoded timeline always spans the full session.
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(FRAME_QUEUE);
        let dead = self.video_dead.clone();
        let pump_halt = dead.clone();
        let pump_in = self.frames_in.clone();
        let pump_out = self.frames_out.clone();
        let interval = pace_interval_ms(fps);
        std::thread::Builder::new()
            .name("moonlit-wgc-pump".into())
            .spawn(move || {
                pump_frames(rx, &mut stdin, interval, fps, &pump_halt, &dead, &pump_in, &pump_out);
            })
            .map_err(|e| format!("cannot spawn frame pump: {e}"))?;
        // TS drain: encoder stdout -> RAM ring.
        let ring = self.video_ring.clone();
        let cap = self.ring_cap;
        let dead2 = self.video_dead.clone();
        std::thread::Builder::new()
            .name("moonlit-ts-drain".into())
            .spawn(move || {
                let mut out = stdout;
                let mut buf = [0u8; 64 * 1024];
                loop {
                    match out.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(mut guard) = ring.lock() {
                                guard.extend_from_slice(&buf[..n]);
                                if guard.len() > cap + 1024 * 1024 {
                                    let excess = guard.len() - cap;
                                    guard.drain(..excess);
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                dead2.store(true, Ordering::Relaxed);
            })
            .map_err(|e| format!("cannot spawn TS drain: {e}"))?;
        Ok((child, tx))
    }

    /// Mux the cut TS window + 3 stems into the final clip. Track order =
    /// MIX, game, mic (Linux parity). Stems shorter than the video are
    /// padded (`apad`), never truncated: `-shortest` then always equals the
    /// video length. Missing/empty stems become full-length silence so the
    /// file always carries 3×AAC.
    async fn mux_clip(
        ffmpeg: &Path,
        cut_ts: &Path,
        mix: &[f32],
        game: &[f32],
        mic: &[f32],
        dest: &Path,
        full_secs: usize,
    ) -> Result<(), String> {
        let tmp = dest.with_extension("mux.tmp");
        let _ = tokio::fs::remove_file(&tmp).await;
        // Full-length placeholder: a missing stem must not shorten the clip
        // through -shortest (apad covers the partial case below).
        let t_wav = std::time::Instant::now();
        let silence = vec![0.0f32; full_secs.max(1) * 48_000 * 2];
        let stems = [
            ("mix", if mix.is_empty() { &silence } else { mix }),
            ("game", if game.is_empty() { &silence } else { game }),
            ("mic", if mic.is_empty() { &silence } else { mic }),
        ];
        let mut wav_paths = Vec::new();
        for (i, (name, samples)) in stems.iter().enumerate() {
            let p = tmp.with_extension(format!("{name}{i}.wav"));
            let owned = samples.to_vec();
            tokio::task::spawn_blocking({
                let p = p.clone();
                move || write_wav(&p, &owned)
            })
            .await
            .map_err(|e| format!("wav task failed: {e}"))?
            .map_err(|e| format!("cannot write {name} wav: {e}"))?;
            wav_paths.push(p);
        }
        let t_mux = std::time::Instant::now();
        let wav_elapsed = t_mux.duration_since(t_wav);
        let status = Command::new(ffmpeg)
            .args(["-y", "-hide_banner", "-loglevel", "error"])
            .arg("-i")
            .arg(cut_ts)
            .arg("-i")
            .arg(&wav_paths[0])
            .arg("-i")
            .arg(&wav_paths[1])
            .arg("-i")
            .arg(&wav_paths[2])
            .args([
                "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:a",
                "-c:v", "copy", "-c:a", "aac", "-b:a", "160k",
                // Pad short stems to the video length; -shortest then always
                // equals the video (a short/empty stem can never truncate).
                "-filter:a:0", "apad", "-filter:a:1", "apad", "-filter:a:2", "apad",
                "-metadata:s:a:0", "title=Mix",
                "-metadata:s:a:1", "title=Game",
                "-metadata:s:a:2", "title=Mic",
                "-shortest",
            ])
            .arg(dest)
            .status()
            .await
            .map_err(|e| format!("save mux failed: {e}"))?;
        for p in &wav_paths {
            let _ = tokio::fs::remove_file(p).await;
        }
        if !status.success() {
            return Err("save mux failed (ffmpeg)".into());
        }
        eprintln!("[moonlit] mux: wav={wav_elapsed:?} mux={:?}", t_mux.elapsed());
        Ok(())
    }
}

impl CaptureEngine for WindowsCaptureEngine {
    async fn start_buffer(&mut self, config: CaptureConfig) -> Result<(), String> {
        if self.control.is_some() {
            return Err("recorder already running".into());
        }
        std::fs::create_dir_all(&config.output_dir)
            .map_err(|e| format!("cannot create clips dir: {e}"))?;
        let vendor = video::vendor(Path::new("")).await;
        let enc_name = video::capture_encoder_name(&vendor, &config.codec).ok_or_else(|| {
            format!(
                "codec '{}' cannot encode on '{}' (stale setting?)",
                config.codec, vendor
            )
        })?;
        let monitor = video::resolve_monitor(&config.source)
            .ok_or_else(|| "no monitor available (Windows 10 1903+ required)".to_string())?;
        let (mw, mh) = monitor
            .width()
            .and_then(|w| monitor.height().map(|h| (w, h)))
            .map_err(|_| "cannot determine monitor size".to_string())?;
        if mw == 0 || mh == 0 {
            return Err("cannot determine monitor size".to_string());
        }
        let fps = if config.fps == 30 { 30 } else { 60 };
        self.ring_cap = (((config.bitrate_kbps as usize)
            * ((config.duration_seconds as usize) + RING_MARGIN_SECS as usize))
            / 8)
            * 1024;
        self.ring_cap = self.ring_cap.max(16 * 1024 * 1024);
        self.video_dead.store(false, Ordering::Relaxed);
        self.frames_in.store(0, Ordering::Relaxed);
        self.frames_out.store(0, Ordering::Relaxed);
        self.video_ring.lock().map(|mut r| r.clear()).ok();

        let ffmpeg = config
            .ffmpeg_bin
            .clone()
            .unwrap_or_else(super::video::capture_ffmpeg);
        let nvenc_hq = config.nvenc_opts.is_some();
        let (child, tx) = self.spawn_video_encoder(
            &ffmpeg,
            enc_name,
            &config.codec,
            mw,
            mh,
            config.out_height,
            config.bitrate_kbps,
            fps,
            nvenc_hq,
        )?;
        self.video_child = Some(child);
        // Prove the encoder is alive before committing to WGC: a bad combo
        // (stale codec, missing HW block) exits within milliseconds, and
        // its stderr names the cause.
        tokio::time::sleep(Duration::from_millis(ENCODER_PROOF_MS)).await;
        if let Some(child) = self.video_child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                let mut err = String::new();
                if let Some(stderr) = child.stderr.take() {
                    let mut out = stderr;
                    use tokio::io::AsyncReadExt as _;
                    let mut buf = Vec::new();
                    let _ = out.read_to_end(&mut buf).await;
                    err = String::from_utf8_lossy(&buf)
                        .lines()
                        .rev()
                        .take(5)
                        .collect::<Vec<_>>()
                        .join(" | ");
                }
                self.video_child = None;
                return Err(format!("video encoder exited ({status}): {err}"));
            }
        }
        // WGC session on its own thread (free-threaded control).
        let shared = Arc::new(EngineShared {
            tx: Mutex::new(tx),
            halt: AtomicBool::new(false),
            width: mw,
            height: mh,
            size_warned: AtomicBool::new(false),
        });
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            shared,
        );
        let control = FrameHandler::start_free_threaded(settings)
            .map_err(|e| format!("WGC start failed: {e:?}"))?;
        self.control = Some(control);
        // Audio: loopback + mic. Unity gains here; the settings task applies
        // persisted gains right after start (same as Linux).
        match AudioCapture::start(
            &config.desktop_device,
            &config.mic_device,
            config.duration_seconds + RING_MARGIN_SECS as u32,
            100,
            100,
            false,
            false,
        ) {
            Ok(a) => {
                if a.live_count() < 2 {
                    eprintln!("[moonlit] audio degraded: {}/2 streams live", a.live_count());
                }
                self.audio = Some(a);
            }
            Err(e) => {
                eprintln!("[moonlit] audio failed, aborting start: {e}");
                let _ = self.stop_buffer().await;
                return Err(e);
            }
        }
        self.output_dir = config.output_dir;
        // Same synthetic shape as GSR `-a` args: mix first, then solos.
        self.audio_args = vec![
            format!("{}|{}", config.desktop_device, config.mic_device),
            config.desktop_device.clone(),
            config.mic_device.clone(),
        ];
        self.save_plan = if config.save_height > 0 {
            Some(SavePlan {
                height: config.save_height,
                bitrate_kbps: config.save_bitrate_kbps,
                codec: config.codec.clone(),
                fps,
                encoder: config.save_encoder,
            })
        } else {
            None
        };
        self.duration_secs = config.duration_seconds;
        self.bitrate_kbps = config.bitrate_kbps;
        self.fps = fps;
        self.ffmpeg = ffmpeg.clone();
        eprintln!(
            "[moonlit] WGC buffer: {}x{}@{} {} ({}), audio {}/2",
            mw,
            mh,
            fps,
            enc_name,
            vendor,
            self.audio.as_ref().map(|a| a.live_count()).unwrap_or(0)
        );
        Ok(())
    }

    async fn save_clip(&mut self) -> Result<PathBuf, String> {
        if self.control.is_none() {
            return Err("recorder not running".into());
        }        if self.video_dead.load(Ordering::Relaxed) {
            let len = self.video_ring.lock().map(|r| r.len()).unwrap_or(0);
            if len == 0 {
                return Err("video encoder died and no footage is buffered".into());
            }
        }
        let ffmpeg = self.ffmpeg.clone();
        let t_save = std::time::Instant::now();
        // Telemetry: frames WGC delivered vs frames the pump wrote, plus
        // ring fill. A stalled source shows in>>out==0; a dead encoder
        // shows video_dead with a frozen ring.
        let (fin, fout, rlen) = (
            self.frames_in.load(Ordering::Relaxed),
            self.frames_out.load(Ordering::Relaxed),
            self.video_ring.lock().map(|r| r.len()).unwrap_or(0),
        );
        eprintln!("[moonlit] save: wgc_in={fin} pump_out={fout} ring={}MB dead={}",
            rlen / 1024 / 1024,
            self.video_dead.load(Ordering::Relaxed));
        // Audio levels: peaks seen since start, per stem. A -inf stem means
        // the device delivered digital silence (muted, wrong endpoint, or
        // no signal) — not a mux problem.
        if let Some(a) = self.audio.as_ref() {
            let (g, m) = a.peak_levels();
            eprintln!("[moonlit] audio stats: game peak={} mic peak={}", dbfs(g), dbfs(m));
        }
        // Video window: last (duration + 2 s) of TS, resynced.
        let keep = ((self.bitrate_kbps as usize * (self.duration_secs as usize + 2)) / 8) * 1024;
        let cut = {
            let ring = self.video_ring.lock().map_err(|_| "video ring poisoned".to_string())?;
            if ring.is_empty() {
                return Err("no video buffered yet".into());
            }
            cut_ts_window(&ring, keep.max(1024 * 1024)).to_vec()
        };
        let ts_path = std::env::temp_dir().join("moonlit-save-cut.ts");
        tokio::fs::write(&ts_path, &cut)
            .await
            .map_err(|e| format!("cannot stage video window: {e}"))?;
        // Audio tails: last `duration` seconds of each 48 kHz stereo stem.
        // The mix is derived here (sample sum on the shared grid), never
        // recorded live.
        let tail = self.duration_secs as usize * 48_000 * 2;
        let snap = self
            .audio
            .as_ref()
            .map(|a| a.snapshot())
            .unwrap_or_default();
        let tail_of = |v: Vec<f32>| {
            if v.len() > tail {
                v[v.len() - tail..].to_vec()
            } else {
                v
            }
        };
        let game = tail_of(snap.game);
        let mic = tail_of(snap.mic);
        let mix = mix_stems(&game, &mic);
        let dest = self.output_dir.join(replay_filename());
        let res = Self::mux_clip(
            &ffmpeg,
            &ts_path,
            &mix,
            &game,
            &mic,
            &dest,
            self.duration_secs as usize,
        )
        .await;
        let _ = tokio::fs::remove_file(&ts_path).await;
        res?;
        eprintln!("[moonlit] wgc-save done in {:?}", t_save.elapsed());
        Ok(dest)
    }

    async fn stop_buffer(&mut self) -> Result<(), String> {
        if let Some(control) = self.control.take() {
            // `stop` posts WM_QUIT to the capture thread and joins it.
            let _ = tokio::task::spawn_blocking(move || control.stop()).await;
        }
        if let Some(mut child) = self.video_child.take() {
            let _ = child.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        }
        // Dropping the capture unregisters shared audio (streams stop).
        self.audio = None;
        self.audio_args.clear();
        self.save_plan = None;
        self.video_ring.lock().map(|mut r| r.clear()).ok();
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "windows-capture"
    }

    fn audio_args(&self) -> Vec<String> {
        self.audio_args.clone()
    }

    fn save_plan(&self) -> Option<SavePlan> {
        self.save_plan.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{cut_ts_window, live_encoder_args, replay_filename};
    use std::path::PathBuf;

    #[test]
    fn nvenc_hq_flags() {
        let a = live_encoder_args("h264_nvenc", "h264", 20000, 60, true);
        let s = a.join(" ");
        assert!(s.contains("-preset p7"), "{s}");
        assert!(s.contains("-tune hq"), "{s}");
        assert!(s.contains("-profile:v high"), "{s}");
        assert!(s.contains("-spatial-aq 1"), "{s}");
        assert!(s.contains("-multipass disabled"), "{s}");
        assert!(s.contains("-rc cbr"), "{s}");
        assert!(s.contains("-g 120"), "{s}");
    }

    #[test]
    fn nvenc_plain_no_hq() {
        // e.g. AV1 on NVIDIA: ladder + CBR, backend defaults otherwise.
        let a = live_encoder_args("av1_nvenc", "av1", 8000, 60, false);
        let s = a.join(" ");
        assert!(!s.contains("-preset"), "{s}");
        assert!(s.contains("-rc cbr"), "{s}");
    }

    #[test]
    fn x264_and_qsv_shapes() {
        let x = live_encoder_args("libx264", "x264", 20000, 30, false).join(" ");
        assert!(x.contains("-preset veryfast"), "{x}");
        assert!(x.contains("zerolatency"), "{x}");
        assert!(x.contains("-g 60"), "{x}");
        let q = live_encoder_args("h264_qsv", "h264", 20000, 60, false).join(" ");
        assert!(q.contains("-preset fast"), "{q}");
    }

    #[test]
    fn ts_cut_resyncs() {
        // Garbage head, then aligned 188 B packets starting with 0x47.
        let mut ring = vec![0xAAu8; 100];
        for _ in 0..4 {
            ring.push(0x47);
            ring.extend(std::iter::repeat(0x00).take(187));
        }
        let cut = cut_ts_window(&ring, 600);
        assert_eq!(cut[0], 0x47);
        assert_eq!(cut[188], 0x47);
        assert!(cut.len() <= 600);
        // A window with no sync at all is delivered whole, never empty.
        let plain = vec![0x11u8; 500];
        assert_eq!(cut_ts_window(&plain, 600).len(), 500);
    }

    /// The pacer holds CFR on a stalled source: one frame in, the writer
    /// keeps receiving re-emissions at cadence until halted.
    #[test]
    fn pacer_holds_cfr_without_input() {
        use super::{pace_interval_ms, pump_frames};
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::{mpsc, Arc};

        assert_eq!(pace_interval_ms(60), 16);
        assert_eq!(pace_interval_ms(30), 33);
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(4);
        let halt = Arc::new(AtomicBool::new(false));
        let dead = Arc::new(AtomicBool::new(false));
        let fin = Arc::new(AtomicU64::new(0));
        let fout = Arc::new(AtomicU64::new(0));
        let (halt2, dead2, fin2, fout2) =
            (halt.clone(), dead.clone(), fin.clone(), fout.clone());
        let handle = std::thread::spawn(move || {
            let mut sink = std::io::Cursor::new(Vec::<u8>::new());
            pump_frames(rx, &mut sink, 10, 100, &halt2, &dead2, &fin2, &fout2);
            sink.into_inner()
        });
        tx.send(vec![0xABu8; 64]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(75));
        halt.store(true, Ordering::Relaxed);
        // Unblock a potential in-flight recv_timeout early exit race by
        // dropping the sender after halt (pump breaks on halt regardless).
        drop(tx);
        let out = handle.join().expect("pump thread");
        // 1 fresh + paced re-emissions (+ bounded catch-up) at 10 ms cadence.
        assert_eq!(fin.load(Ordering::Relaxed), 1);
        let n = fout.load(Ordering::Relaxed);
        assert!(n >= 4, "pacer stalled: only {n} frames in ~75 ms");
        assert!(out.len() >= 64 * 4, "writer got {} bytes", out.len());
        assert!(!dead.load(Ordering::Relaxed));
    }

    #[test]
    fn mix_sums_aligned_tails() {
        use super::mix_stems;
        assert_eq!(mix_stems(&[0.5, -0.5], &[0.25, 0.25]), vec![0.75, -0.25]);
        // Shorter side decides; tails share the "now" edge.
        assert_eq!(mix_stems(&[1.0, 2.0, 3.0], &[10.0]), vec![11.0]);
        assert!(mix_stems(&[], &[1.0]).is_empty());
    }

    #[test]
    fn wav_exact_size_and_clamp() {
        use super::write_wav;
        let dir = std::env::temp_dir().join(format!("moonlit-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.wav");
        // 1 s stereo + out-of-range clamp check.
        let mut samples = vec![0.0f32; 48_000 * 2];
        samples[0] = 2.0;
        samples[1] = -2.0;
        write_wav(&p, &samples).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(bytes.len(), 44 + 48_000 * 2 * 2);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(i16::from_le_bytes([bytes[44], bytes[45]]), 32767);
        assert_eq!(i16::from_le_bytes([bytes[46], bytes[47]]), -32767);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn filename_shape() {
        let n = replay_filename();
        assert!(n.starts_with("replay_"), "{n}");
        assert!(n.ends_with(".mp4"), "{n}");
        assert_eq!(n.len(), "replay_YYYY-MM-DD_HH-MM-SS.mp4".len(), "{n}");
        assert!(PathBuf::from(&n).extension().unwrap() == "mp4");
    }

    /// Live end-to-end on real hardware (ignored by default — needs WGC +
    /// NVENC + WASAPI, never runs on headless CI). Run explicitly:
    /// `cargo test --target x86_64-pc-windows-msvc live_buffer -- --ignored`.
    /// Buffers 6 s, saves, and asserts h264 video + 3×AAC.
    #[tokio::test]
    #[ignore]
    async fn live_buffer_and_save() {
        use super::super::super::{CaptureConfig, CaptureEngine, TranscodeEncoder};
        use std::time::Duration as StdDuration;

        let dir = std::env::temp_dir().join("moonlit-e2e");
        std::fs::create_dir_all(&dir).unwrap();
        let mut eng = super::WindowsCaptureEngine::new();
        eng.start_buffer(CaptureConfig {
            duration_seconds: 10,
            fps: 60,
            output_dir: dir,
            gsr_bin: None,
            desktop_device: String::new(),
            mic_device: String::new(),
            source: String::new(),
            codec: "h264".into(),
            out_height: 0,
            bitrate_kbps: 20000,
            save_height: 0,
            save_bitrate_kbps: 20000,
            save_encoder: Some(TranscodeEncoder::Nvenc),
            nvenc_opts: Some(crate::video_quality::nvenc_hq_opts("h264")),
            ffmpeg_bin: None,
        })
        .await
        .expect("start_buffer");
        tokio::time::sleep(StdDuration::from_secs(6)).await;
        let path = eng.save_clip().await.expect("save_clip");
        assert!(path.exists(), "clip missing");
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size > 50_000, "clip suspiciously small: {size} B");
        // Stream census via ffmpeg (no ffprobe in the static sidecar).
        let probe = tokio::process::Command::new(super::super::video::capture_ffmpeg())
            .args(["-hide_banner", "-i", &path.to_string_lossy()])
            .output()
            .await
            .expect("probe");
        let err = String::from_utf8_lossy(&probe.stderr);
        assert!(err.contains("Video: h264"), "no h264 video:\n{err}");
        assert_eq!(err.matches("Audio: aac").count(), 3, "want 3xAAC:\n{err}");
        // Duration must track wall-clock (6 s buffered): the CFR pacer keeps
        // the timeline alive even on a static desktop. This assert is the
        // regression net for timeline collapse (short clips on long runs).
        let dur_ms = crate::editor::ffmpeg::probe_duration_ms(
            &super::super::video::capture_ffmpeg(),
            &path,
        )
        .await
        .expect("duration probe");
        assert!(
            (4000..=9000).contains(&dur_ms),
            "timeline collapsed: {dur_ms} ms for a 6 s run"
        );
        eng.stop_buffer().await.expect("stop");
        let _ = std::fs::remove_file(&path);
    }
}
