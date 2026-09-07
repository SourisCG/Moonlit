//! Windows live audio: WASAPI loopback (game) + mic via `cpal`.
//!
//! Mirrors `os/linux/audio` semantics: per-track gain/mute 0–200% applied to
//! what WE capture (sample multiplication in our callback — never the OS
//! mixer), mix tap stays a full-fidelity safety copy, gains persist via
//! settings and are applied live through atomics.
//!
//! Ownership: the engine owns [`AudioCapture`] (the `cpal` streams live and
//! die with it). The free `apply_gains`/`linked_count` fns (same signatures
//! as Linux) reach the shared rings through a small process registry holding
//! only `Send + Sync` state — never the streams themselves.

use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc, Mutex, OnceLock,
};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Sample, SampleFormat};

/// Common stem format: 48 kHz stereo f32 (WASAPI is usually 48 kHz already;
/// anything else is linearly resampled in the callback).
pub const STEM_RATE: u32 = 48_000;
pub const STEM_CHANNELS: usize = 2;

/// Snapshot of the two solo stems for the save mux. The MIX track is
/// derived at save time (sample sum on the shared 48 kHz grid), never
/// recorded live — appending two independent callbacks into one ring would
/// interleave chunks instead of mixing them.
#[derive(Debug, Clone, Default)]
pub struct AudioSnapshot {
    pub game: Vec<f32>,
    pub mic: Vec<f32>,
}

/// Shared, `Send + Sync` capture state. Rings hold interleaved stereo f32.
pub struct SharedAudio {
    game_ring: Mutex<Vec<f32>>,
    mic_ring: Mutex<Vec<f32>>,
    /// Lock-free peak meters (f32 bits of max |sample| seen). Updated in the
    /// hot callbacks; read at save for the dBFS stats line. Answers "is the
    /// device delivering signal?" without locks or extra threads.
    game_peak: AtomicU32,
    mic_peak: AtomicU32,
    /// Max stem samples kept (set from the buffer length at start).
    capacity: usize,
    game_pct: AtomicU32,
    mic_pct: AtomicU32,
    mute_game: AtomicBool,
    mute_mic: AtomicBool,
    game_live: AtomicBool,
    mic_live: AtomicBool,
}

impl SharedAudio {
    fn new(capacity_secs: u32, game_pct: u32, mic_pct: u32, mute_game: bool, mute_mic: bool) -> Self {
        Self {
            game_ring: Mutex::new(Vec::new()),
            mic_ring: Mutex::new(Vec::new()),
            game_peak: AtomicU32::new(0.0f32.to_bits()),
            mic_peak: AtomicU32::new(0.0f32.to_bits()),
            capacity: ((capacity_secs as usize) * STEM_RATE as usize) * STEM_CHANNELS,
            game_pct: AtomicU32::new(game_pct.min(200)),
            mic_pct: AtomicU32::new(mic_pct.min(200)),
            mute_game: AtomicBool::new(mute_game),
            mute_mic: AtomicBool::new(mute_mic),
            game_live: AtomicBool::new(false),
            mic_live: AtomicBool::new(false),
        }
    }

    fn push_stem(ring: &Mutex<Vec<f32>>, capacity: usize, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        if let Ok(mut guard) = ring.lock() {
            guard.extend_from_slice(samples);
            // Amortized trim: only drain when 10% over capacity.
            if guard.len() > capacity + capacity / 10 {
                let excess = guard.len() - capacity;
                guard.drain(..excess);
            }
        }
    }

    /// Live stream count (0–2). The mix tap is derived, not an OS stream.
    pub fn live_count(&self) -> usize {
        [self.game_live.load(Ordering::Relaxed), self.mic_live.load(Ordering::Relaxed)]
            .into_iter()
            .filter(|l| *l)
            .count()
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        AudioSnapshot {
            game: self.game_ring.lock().map(|g| g.clone()).unwrap_or_default(),
            mic: self.mic_ring.lock().map(|g| g.clone()).unwrap_or_default(),
        }
    }

    /// Peak levels (linear 0.0–1.0+) seen since start: (game, mic).
    /// 0.0 = digital silence from the device (nothing captured, or muted).
    pub fn peak_levels(&self) -> (f32, f32) {
        (
            f32::from_bits(self.game_peak.load(Ordering::Relaxed)),
            f32::from_bits(self.mic_peak.load(Ordering::Relaxed)),
        )
    }

    /// Fold one quantum's peak into the lock-free meter. f32 bits of
    /// non-negative values order like u32, so fetch_max is exact.
    fn note_peak(meter: &AtomicU32, samples: &[f32]) {
        let peak = samples.iter().fold(0.0f32, |a, &s| a.max(s.abs()));
        if peak > 0.0 {
            meter.fetch_max(peak.to_bits(), Ordering::Relaxed);
        }
    }
}

fn registry() -> &'static Mutex<Option<Arc<SharedAudio>>> {
    static REG: OnceLock<Mutex<Option<Arc<SharedAudio>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(None))
}

/// Convert a callback slice to gained 48 kHz stereo f32.
fn convert(samples: &[f32], in_channels: u16, in_rate: u32, pct: u32, muted: bool) -> Vec<f32> {
    let stereo = to_stereo_48k(samples, in_channels, in_rate);
    if muted {
        return vec![0.0; stereo.len()];
    }
    if pct == 100 {
        return stereo;
    }
    let k = pct as f32 / 100.0;
    stereo.into_iter().map(|s| s * k).collect()
}

/// Mixdown + linear resample to 48 kHz stereo. Pure (unit-tested).
fn to_stereo_48k(samples: &[f32], in_channels: u16, in_rate: u32) -> Vec<f32> {
    let ch = in_channels.max(1) as usize;
    let frames_in = samples.len() / ch;
    if frames_in == 0 {
        return Vec::new();
    }
    // Mono-ize each input frame (average; stereo passthrough stays exact
    // because (l+r)/2 per channel would collapse — so special-case it).
    let mono: Vec<f32> = if ch == 1 {
        samples[..frames_in].to_vec()
    } else if ch == 2 {
        // Keep channels separate until after resampling for fidelity.
        vec![]
    } else {
        (0..frames_in)
            .map(|f| samples[f * ch..(f + 1) * ch].iter().sum::<f32>() / ch as f32)
            .collect()
    };
    if in_rate == STEM_RATE {
        return if ch == 2 {
            samples[..frames_in * 2].to_vec()
        } else {
            mono.iter().flat_map(|&m| [m, m]).collect()
        };
    }
    // Linear resample on mono (or per-channel for stereo).
    let frames_out = ((frames_in as u64 * STEM_RATE as u64) / in_rate as u64) as usize;
    if ch == 2 {
        let mut out = Vec::with_capacity(frames_out * 2);
        for i in 0..frames_out {
            let pos = i as f64 * frames_in as f64 / frames_out.max(1) as f64;
            let i0 = (pos as usize).min(frames_in.saturating_sub(1));
            let i1 = (i0 + 1).min(frames_in.saturating_sub(1));
            let t = (pos - i0 as f64) as f32;
            for c in 0..2 {
                let a = samples[i0 * 2 + c];
                let b = samples[i1 * 2 + c];
                out.push(a + (b - a) * t);
            }
        }
        out
    } else {
        let mut out = Vec::with_capacity(frames_out * 2);
        for i in 0..frames_out {
            let pos = i as f64 * mono.len() as f64 / frames_out.max(1) as f64;
            let i0 = (pos as usize).min(mono.len().saturating_sub(1));
            let i1 = (i0 + 1).min(mono.len().saturating_sub(1));
            let t = (pos - i0 as f64) as f32;
            let m = mono[i0] + (mono[i1] - mono[i0]) * t;
            out.push(m);
            out.push(m);
        }
        out
    }
}

/// Push one game quantum (gained stem).
/// Push one game quantum (gained stem). Peaks meter the CONVERTED stem —
/// what actually lands in the ring and the file.
fn push_game_quantum(shared: &Arc<SharedAudio>, f: &[f32], ch: u16, rate: u32) {
    let pct = shared.game_pct.load(Ordering::Relaxed);
    let muted = shared.mute_game.load(Ordering::Relaxed);
    let stem = convert(f, ch, rate, pct, muted);
    SharedAudio::note_peak(&shared.game_peak, &stem);
    SharedAudio::push_stem(&shared.game_ring, shared.capacity, &stem);
}

/// Push one mic quantum (gained stem). Peaks meter the CONVERTED stem —
/// what actually lands in the ring and the file.
fn push_mic_quantum(shared: &Arc<SharedAudio>, f: &[f32], ch: u16, rate: u32) {
    let pct = shared.mic_pct.load(Ordering::Relaxed);
    let muted = shared.mute_mic.load(Ordering::Relaxed);
    let stem = convert(f, ch, rate, pct, muted);
    SharedAudio::note_peak(&shared.mic_peak, &stem);
    SharedAudio::push_stem(&shared.mic_ring, shared.capacity, &stem);
}
/// Friendly name for logs (which physical device backs each stream).
fn device_label(d: &Option<cpal::Device>) -> String {
    d.as_ref()
        .and_then(|dev| dev.name().ok())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "<none>".into())
}

/// Owned capture session. Dropping it stops both streams (cpal `Stream`
/// stops on drop) and unregisters the shared state.
pub struct AudioCapture {
    _game_stream: Option<cpal::Stream>,
    _mic_stream: Option<cpal::Stream>,
    shared: Arc<SharedAudio>,
}

impl AudioCapture {
    /// Start loopback + mic capture. Empty ids and the GSR magic defaults
    /// (`default_output`/`default_input`, seeded into settings) fall back to
    /// the OS default render / input devices. At least one stream must come up.
    pub fn start(
        desktop_id: &str,
        mic_id: &str,
        capacity_secs: u32,
        game_pct: u32,
        mic_pct: u32,
        mute_game: bool,
        mute_mic: bool,
    ) -> Result<Self, String> {
        let shared = Arc::new(SharedAudio::new(
            capacity_secs.max(10),
            game_pct,
            mic_pct,
            mute_game,
            mute_mic,
        ));
        // Game: render endpoint opened as INPUT = WASAPI loopback.
        let game_device = super::devices::find_output_device(desktop_id);
        // Mic: capture endpoint.
        let mic_device = super::devices::find_input_device(mic_id);
        eprintln!("[moonlit] audio devices: game='{}' mic='{}'",
            device_label(&game_device), device_label(&mic_device));
        let game_stream = match game_device {
            Some(d) => match start_loopback_stream(&d, shared.clone()) {
                Ok(s) => {
                    shared.game_live.store(true, Ordering::Relaxed);
                    Some(s)
                }
                Err(e) => {
                    eprintln!("[moonlit] game loopback failed ({e}), continuing mic-only");
                    None
                }
            },
            None => {
                eprintln!("[moonlit] game device '{desktop_id}' not found, continuing mic-only");
                None
            }
        };
        // Mic: capture endpoint.
        let mic_stream = match mic_device {
            Some(d) => match start_mic_stream(&d, shared.clone()) {
                Ok(s) => {
                    shared.mic_live.store(true, Ordering::Relaxed);
                    Some(s)
                }
                Err(e) => {
                    eprintln!("[moonlit] mic capture failed ({e}), continuing without mic");
                    None
                }
            },
            None => {
                eprintln!("[moonlit] mic device '{mic_id}' not found, continuing without mic");
                None
            }
        };
        if game_stream.is_none() && mic_stream.is_none() {
            return Err("no audio stream could be started (game and mic both failed)".into());
        }
        if let Ok(mut reg) = registry().lock() {
            *reg = Some(shared.clone());
        }
        Ok(Self {
            _game_stream: game_stream,
            _mic_stream: mic_stream,
            shared,
        })
    }

    pub fn snapshot(&self) -> AudioSnapshot {
        self.shared.snapshot()
    }

    pub fn live_count(&self) -> usize {
        self.shared.live_count()
    }

    /// Peak levels (linear) seen since start: (game, mic).
    pub fn peak_levels(&self) -> (f32, f32) {
        self.shared.peak_levels()
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        if let Ok(mut reg) = registry().lock() {
            if let Some(cur) = reg.as_ref() {
                if Arc::ptr_eq(cur, &self.shared) {
                    *reg = None;
                }
            }
        }
    }
}

/// WASAPI loopback: an input stream on a RENDER device. cpal sets
/// `AUDCLNT_STREAMFLAGS_LOOPBACK` transparently for this combination.
fn start_loopback_stream(device: &cpal::Device, shared: Arc<SharedAudio>) -> Result<cpal::Stream, String> {
    let config = device
        .default_output_config()
        .map_err(|e| format!("loopback config: {e}"))?;
    eprintln!("[moonlit] game format: {} Hz, {} ch, {:?}",
        config.sample_rate().0, config.channels(), config.sample_format());
    let stream_config = cpal::StreamConfig {
        channels: config.channels(),
        sample_rate: config.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    let ch = config.channels();
    let rate = config.sample_rate().0;
    let err_fn = |err| eprintln!("[moonlit] game stream error: {err}");
    let make_cb = |shared: Arc<SharedAudio>| {
        move |data: &[f32], _: &cpal::InputCallbackInfo| push_game_quantum(&shared, data, ch, rate)
    };
    let stream = match config.sample_format() {
        SampleFormat::F32 => device.build_input_stream(&stream_config, make_cb(shared), err_fn, None),
        SampleFormat::I16 => {
            let shared2 = shared.clone();
            let cb = move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let f: Vec<f32> = data.iter().map(|s| s.to_sample::<f32>()).collect();
                push_game_quantum(&shared2, &f, ch, rate);
            };
            device.build_input_stream(&stream_config, cb, err_fn, None)
        }
        SampleFormat::U16 => {
            let shared2 = shared.clone();
            let cb = move |data: &[u16], _: &cpal::InputCallbackInfo| {
                let f: Vec<f32> = data.iter().map(|s| s.to_sample::<f32>()).collect();
                push_game_quantum(&shared2, &f, ch, rate);
            };
            device.build_input_stream(&stream_config, cb, err_fn, None)
        }
        other => return Err(format!("unsupported loopback sample format: {other:?}")),
    }
    .map_err(|e| format!("loopback stream: {e}"))?;
    stream.play().map_err(|e| format!("loopback play: {e}"))?;
    Ok(stream)
}

fn start_mic_stream(device: &cpal::Device, shared: Arc<SharedAudio>) -> Result<cpal::Stream, String> {
    let config = device
        .default_input_config()
        .map_err(|e| format!("mic config: {e}"))?;
    eprintln!("[moonlit] mic format: {} Hz, {} ch, {:?}",
        config.sample_rate().0, config.channels(), config.sample_format());
    let stream_config = cpal::StreamConfig {
        channels: config.channels(),
        sample_rate: config.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    let ch = config.channels();
    let rate = config.sample_rate().0;
    let err_fn = |err| eprintln!("[moonlit] mic stream error: {err}");
    // NOTE: the mix ring is game-appended by the loopback callback and
    // mic-appended here. Both sides convert to the same 48 kHz stereo grid,
    // so the mix stays aligned within one callback quantum (~10 ms).
    // When only one side is live, the mix carries that side alone.
    let make_cb = |shared: Arc<SharedAudio>| {
        move |data: &[f32], _: &cpal::InputCallbackInfo| push_mic_quantum(&shared, data, ch, rate)
    };
    let stream = match config.sample_format() {
        SampleFormat::F32 => device.build_input_stream(&stream_config, make_cb(shared), err_fn, None),
        SampleFormat::I16 => {
            let shared2 = shared.clone();
            let cb = move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let f: Vec<f32> = data.iter().map(|s| s.to_sample::<f32>()).collect();
                push_mic_quantum(&shared2, &f, ch, rate);
            };
            device.build_input_stream(&stream_config, cb, err_fn, None)
        }
        SampleFormat::U16 => {
            let shared2 = shared.clone();
            let cb = move |data: &[u16], _: &cpal::InputCallbackInfo| {
                let f: Vec<f32> = data.iter().map(|s| s.to_sample::<f32>()).collect();
                push_mic_quantum(&shared2, &f, ch, rate);
            };
            device.build_input_stream(&stream_config, cb, err_fn, None)
        }
        other => return Err(format!("unsupported mic sample format: {other:?}")),
    }
    .map_err(|e| format!("mic stream: {e}"))?;
    stream.play().map_err(|e| format!("mic play: {e}"))?;
    Ok(stream)
}

/// Mirrors `os/linux/audio::apply_gains`. Updates the live atomics (0–200%,
/// mix tap untouched) and returns linked streams. Errors when no capture is
/// registered instead of failing silently.
pub async fn apply_gains(
    _known_args: &[String],
    game_pct: u32,
    mic_pct: u32,
    mute_game: bool,
    mute_mic: bool,
) -> Result<usize, String> {
    let shared = registry()
        .lock()
        .ok()
        .and_then(|r| r.clone())
        .ok_or_else(|| "no Windows audio streams linked (buffer not running)".to_string())?;
    shared.game_pct.store(game_pct.min(200), Ordering::Relaxed);
    shared.mic_pct.store(mic_pct.min(200), Ordering::Relaxed);
    shared.mute_game.store(mute_game, Ordering::Relaxed);
    shared.mute_mic.store(mute_mic, Ordering::Relaxed);
    Ok(shared.live_count())
}

/// Mirrors `os/linux/audio::linked_count`. Single-shot, never waits.
pub async fn linked_count(_known_args: &[String]) -> usize {
    registry()
        .lock()
        .ok()
        .and_then(|r| r.clone())
        .map(|s| s.live_count())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{convert, to_stereo_48k, SharedAudio, STEM_CHANNELS, STEM_RATE};

    #[test]
    fn passthrough_48k_stereo() {
        let v = vec![0.1, 0.2, 0.3, 0.4];
        assert_eq!(to_stereo_48k(&v, 2, 48_000), v);
    }

    #[test]
    fn mono_dup() {
        assert_eq!(to_stereo_48k(&[0.5], 1, 48_000), vec![0.5, 0.5]);
    }

    #[test]
    fn resample_441k_length() {
        // 1 s of 44.1 kHz mono -> ~1 s of 48 kHz stereo.
        let v = vec![0.25; 44_100];
        let out = to_stereo_48k(&v, 1, 44_100);
        assert_eq!(out.len(), 48_000 * STEM_CHANNELS);
        assert!(out.iter().all(|&s| (s - 0.25).abs() < 1e-5));
    }

    #[test]
    fn gain_and_mute() {
        let v = vec![0.5, -0.5];
        assert_eq!(convert(&v, 2, STEM_RATE, 200, false), vec![1.0, -1.0]);
        assert_eq!(convert(&v, 2, STEM_RATE, 50, false), vec![0.25, -0.25]);
        assert_eq!(convert(&v, 2, STEM_RATE, 200, true), vec![0.0, 0.0]);
    }

    #[test]
    fn peak_meter_tracks_max() {
        use super::{push_game_quantum, push_mic_quantum, SharedAudio};
        use std::sync::Arc;
        let s = Arc::new(SharedAudio::new(10, 100, 100, false, false));
        assert_eq!(s.peak_levels(), (0.0, 0.0));
        push_game_quantum(&s, &[0.1, -0.2, 0.05, 0.0], 2, STEM_RATE);
        push_mic_quantum(&s, &[0.0; 4], 2, STEM_RATE);
        let (g, m) = s.peak_levels();
        assert!((g - 0.2).abs() < 1e-6, "{g}");
        assert_eq!(m, 0.0);
        // Meters never go down within a session.
        push_game_quantum(&s, &[0.01; 2], 2, STEM_RATE);
        assert!((s.peak_levels().0 - 0.2).abs() < 1e-6);
    }

    #[test]
    fn ring_trims() {
        let s = SharedAudio::new(1, 100, 100, false, false);
        let cap = s.capacity;
        assert_eq!(cap, STEM_RATE as usize * STEM_CHANNELS);
        SharedAudio::push_stem(&s.game_ring, cap, &vec![1.0; cap + 5000]);
        let len = s.game_ring.lock().unwrap().len();
        assert!(len <= cap + cap / 10 + 5000 && len >= cap);
    }

    /// Live hardware test (ignored by default — needs real WASAPI devices,
    /// so it never runs on headless CI). Run explicitly:
    /// `cargo test --target x86_64-pc-windows-msvc live_ -- --ignored`.
    /// Make noise while it runs: it logs both stems' peaks.
    #[tokio::test]
    #[ignore]
    async fn live_streams_link() {
        use super::AudioCapture;
        let cap = AudioCapture::start("", "", 12, 100, 100, false, false)
            .expect("at least one stream must start");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(cap.live_count() >= 1, "no live streams");
        let snap = cap.snapshot();
        let total = snap.game.len() + snap.mic.len();
        assert!(total > 0, "rings stayed empty after 2 s");
        let (g, m) = cap.peak_levels();
        eprintln!("[moonlit-test] live peaks: game={g:.4} mic={m:.4}");
    }

    /// Regression test for the stock-install failure: the GSR magic ids
    /// seeded into settings must resolve like empty ids (OS defaults).
    #[tokio::test]
    #[ignore]
    async fn live_magic_ids_link() {
        use super::AudioCapture;
        let cap = AudioCapture::start("default_output", "default_input", 12, 100, 100, false, false)
            .expect("magic ids must resolve to defaults");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(cap.live_count() >= 1, "no live streams via magic ids");
    }
}
