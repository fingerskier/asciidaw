//! Realtime audio via cpal.
//!
//! Output callback: owns a snapshot of the tracks (swapped in through a
//! lock-free ring; the old snapshot is handed back so it's freed on the UI
//! thread, never in the callback), mixes with `mix::render`, publishes the
//! playhead and meters through atomics.
//!
//! Input callback: pushes whole interleaved buffers into a ring that the UI
//! thread drains for meters and recording.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Host, SampleFormat, SizedSample, Stream, StreamConfig, SupportedStreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::mix::{RenderOpts, Snapshot, render};
use crate::project::Track;

pub const MAX_METER_TRACKS: usize = 64;

enum Cmd {
    Tracks(Snapshot),
    Play(u64),
    Stop,
}

/// State the callbacks publish for the UI.
pub struct Shared {
    pub playhead: AtomicU64,
    pub playing: AtomicBool,
    pub input_overruns: AtomicU64,
    out_peak: [AtomicU32; 2],
    track_peak: [AtomicU32; MAX_METER_TRACKS],
    errors: Mutex<Vec<String>>,
}

impl Shared {
    fn new() -> Self {
        Shared {
            playhead: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            input_overruns: AtomicU64::new(0),
            out_peak: std::array::from_fn(|_| AtomicU32::new(0)),
            track_peak: std::array::from_fn(|_| AtomicU32::new(0)),
            errors: Mutex::new(Vec::new()),
        }
    }

    /// Non-negative floats order the same as their bit patterns, so
    /// `fetch_max` on the bits is a lock-free float max.
    fn bump(slot: &AtomicU32, v: f32) {
        slot.fetch_max(v.max(0.0).to_bits(), Relaxed);
    }

    fn take(slot: &AtomicU32) -> f32 {
        f32::from_bits(slot.swap(0, Relaxed))
    }

    pub fn take_out_peaks(&self) -> [f32; 2] {
        [Self::take(&self.out_peak[0]), Self::take(&self.out_peak[1])]
    }

    pub fn take_track_peak(&self, i: usize) -> f32 {
        self.track_peak.get(i).map_or(0.0, Self::take)
    }

    pub fn take_errors(&self) -> Vec<String> {
        self.errors.lock().map(|mut e| std::mem::take(&mut *e)).unwrap_or_default()
    }

    fn push_error(&self, e: impl ToString) {
        if let Ok(mut errs) = self.errors.lock() {
            errs.push(e.to_string());
        }
    }
}

/// User device selection.
#[derive(Debug, Clone, Default)]
pub struct AudioOpts {
    pub host: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
}

impl std::fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({} ch, {} Hz, {})", self.name, self.channels, self.rate, self.format)
    }
}

pub struct Output {
    _stream: Stream,
    cmds: Producer<Cmd>,
    garbage: Consumer<Snapshot>,
    pub info: DeviceInfo,
}

pub struct Input {
    _stream: Stream,
    ring: Consumer<f32>,
    pub info: DeviceInfo,
}

impl Input {
    /// Move every complete interleaved frame captured so far into `buf`.
    pub fn drain(&mut self, buf: &mut Vec<f32>) {
        let ch = self.info.channels as usize;
        let n = self.ring.slots() / ch * ch;
        if n == 0 {
            return;
        }
        if let Ok(chunk) = self.ring.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            buf.extend_from_slice(a);
            buf.extend_from_slice(b);
            chunk.commit_all();
        }
    }
}

pub struct Engine {
    pub output: Option<Output>,
    pub input: Option<Input>,
    pub shared: Arc<Shared>,
    /// Human-readable problems from opening devices.
    pub notes: Vec<String>,
}

pub fn select_host(name: Option<&str>) -> Result<Host> {
    let Some(name) = name else { return Ok(cpal::default_host()) };
    let id = cpal::available_hosts().into_iter().find(|h| h.name().eq_ignore_ascii_case(name)).ok_or_else(|| {
        let names: Vec<_> = cpal::available_hosts().iter().map(|h| h.name()).collect();
        anyhow!("audio host {name:?} not available (have: {})", names.join(", "))
    })?;
    Ok(cpal::host_from_id(id)?)
}

pub fn device_label(d: &Device) -> String {
    d.description()
        .map(|desc| desc.name().to_string())
        .or_else(|_| d.id().map(|id| id.to_string()))
        .unwrap_or_else(|_| "unknown device".into())
}

/// Default device, or the first whose id or name contains `query`.
pub fn find_device(host: &Host, query: Option<&str>, input: bool) -> Result<Device> {
    let kind = if input { "input" } else { "output" };
    let Some(q) = query else {
        let d = if input { host.default_input_device() } else { host.default_output_device() };
        return d.with_context(|| format!("no default {kind} device"));
    };
    if let Ok(id) = q.parse::<cpal::DeviceId>()
        && let Some(d) = host.device_by_id(&id)
    {
        return Ok(d);
    }
    let ql = q.to_lowercase();
    let devices: Vec<Device> = if input { host.input_devices()?.collect() } else { host.output_devices()?.collect() };
    devices
        .into_iter()
        .find(|d| {
            let id = d.id().map(|i| i.to_string()).unwrap_or_default();
            device_label(d).to_lowercase().contains(&ql) || id.to_lowercase().contains(&ql)
        })
        .with_context(|| format!("no {kind} device matching {q:?} (try `asciidaw devices`)"))
}

/// The device's default config, moved to `rate` when the device can do it.
pub fn pick_config(device: &Device, input: bool, rate: Option<u32>) -> Result<SupportedStreamConfig> {
    let default = if input { device.default_input_config()? } else { device.default_output_config()? };
    let Some(rate) = rate.filter(|&r| r != default.sample_rate()) else { return Ok(default) };
    let ranges: Vec<_> =
        if input { device.supported_input_configs()?.collect() } else { device.supported_output_configs()?.collect() };
    Ok(ranges
        .into_iter()
        .filter(|r| r.contains_rate(rate))
        .max_by_key(|r| {
            (
                r.channels() == default.channels(),
                r.sample_format() == default.sample_format(),
                r.sample_format() == SampleFormat::F32,
            )
        })
        .map(|r| r.with_sample_rate(rate))
        .unwrap_or(default))
}

macro_rules! with_sample_type {
    ($fmt:expr, $func:ident ( $($arg:expr),* )) => {
        match $fmt {
            SampleFormat::F32 => $func::<f32>($($arg),*),
            SampleFormat::F64 => $func::<f64>($($arg),*),
            SampleFormat::I16 => $func::<i16>($($arg),*),
            SampleFormat::I24 => $func::<cpal::I24>($($arg),*),
            SampleFormat::I32 => $func::<i32>($($arg),*),
            SampleFormat::I8 => $func::<i8>($($arg),*),
            SampleFormat::U8 => $func::<u8>($($arg),*),
            SampleFormat::U16 => $func::<u16>($($arg),*),
            SampleFormat::U24 => $func::<cpal::U24>($($arg),*),
            SampleFormat::U32 => $func::<u32>($($arg),*),
            other => Err(anyhow!("unsupported sample format {other}")),
        }
    };
}

struct OutState {
    cmds: Consumer<Cmd>,
    garbage: Producer<Snapshot>,
    tracks: Snapshot,
    playing: bool,
    pos: u64,
    mix: Vec<f32>,
    peaks: Vec<f32>,
    channels: usize,
    shared: Arc<Shared>,
}

impl OutState {
    fn process<T: SizedSample + FromSample<f32>>(&mut self, data: &mut [T]) {
        while let Ok(cmd) = self.cmds.pop() {
            match cmd {
                Cmd::Tracks(t) => {
                    let old = std::mem::replace(&mut self.tracks, t);
                    // Full garbage ring (UI stalled) means we free here; rare and bounded.
                    let _ = self.garbage.push(old);
                }
                Cmd::Play(from) => {
                    self.pos = from;
                    self.playing = true;
                }
                Cmd::Stop => self.playing = false,
            }
        }
        self.shared.playing.store(self.playing, Relaxed);
        let nch = self.channels;
        let frames = data.len() / nch;
        if !self.playing || frames == 0 {
            data.fill(T::EQUILIBRIUM);
            return;
        }
        if self.mix.len() < frames * 2 {
            self.mix.resize(frames * 2, 0.0); // only if the device grows its buffer
        }
        let mix = &mut self.mix[..frames * 2];
        self.peaks.fill(0.0);
        render(&self.tracks, self.pos, mix, &RenderOpts::STEREO_MIX, &mut self.peaks);
        let (mut pl, mut pr) = (0f32, 0f32);
        for (f, frame) in data.chunks_exact_mut(nch).enumerate() {
            let (l, r) = (mix[2 * f].clamp(-1.0, 1.0), mix[2 * f + 1].clamp(-1.0, 1.0));
            pl = pl.max(l.abs());
            pr = pr.max(r.abs());
            if nch == 1 {
                frame[0] = T::from_sample(0.5 * (l + r));
            } else {
                frame[0] = T::from_sample(l);
                frame[1] = T::from_sample(r);
                frame[2..].fill(T::EQUILIBRIUM);
            }
        }
        self.pos += frames as u64;
        self.shared.playhead.store(self.pos, Relaxed);
        Shared::bump(&self.shared.out_peak[0], pl);
        Shared::bump(&self.shared.out_peak[1], pr);
        for (slot, &p) in self.shared.track_peak.iter().zip(&self.peaks) {
            Shared::bump(slot, p);
        }
    }
}

fn build_output<T: SizedSample + FromSample<f32>>(
    device: &Device,
    config: StreamConfig,
    mut state: OutState,
) -> Result<Stream> {
    let shared = state.shared.clone();
    Ok(device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _| state.process(data),
        move |e| shared.push_error(format!("output: {e}")),
        None,
    )?)
}

fn build_input<T>(device: &Device, config: StreamConfig, mut ring: Producer<f32>, shared: Arc<Shared>) -> Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let nch = config.channels.max(1) as u64;
    let err_shared = shared.clone();
    Ok(device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| match ring.write_chunk_uninit(data.len()) {
            // Whole buffers or nothing, so frames never tear across channels.
            Ok(chunk) => {
                chunk.fill_from_iter(data.iter().map(|s| s.to_sample::<f32>()));
            }
            Err(_) => {
                shared.input_overruns.fetch_add(data.len() as u64 / nch, Relaxed);
            }
        },
        move |e| err_shared.push_error(format!("input: {e}")),
        None,
    )?)
}

impl Engine {
    /// No devices: editing and export still work.
    pub fn offline() -> Engine {
        Engine { output: None, input: None, shared: Arc::new(Shared::new()), notes: vec![] }
    }

    /// Open output and/or input near `rate`. Failures become `notes`, not
    /// errors, so the app can run with whatever is available.
    pub fn open(opts: &AudioOpts, rate: Option<u32>, want_output: bool, want_input: bool) -> Engine {
        let mut engine = Engine::offline();
        let host = match select_host(opts.host.as_deref()) {
            Ok(h) => h,
            Err(e) => {
                engine.notes.push(format!("{e:#}"));
                return engine;
            }
        };
        if want_output {
            match engine.open_output(&host, opts.output.as_deref(), rate) {
                Ok(o) => engine.output = Some(o),
                Err(e) => engine.notes.push(format!("no playback: {e:#}")),
            }
        }
        if want_input {
            // Match the output's rate so takes line up without resampling.
            let rate = rate.or(engine.output.as_ref().map(|o| o.info.rate));
            match engine.open_input(&host, opts.input.as_deref(), rate) {
                Ok(i) => engine.input = Some(i),
                Err(e) => engine.notes.push(format!("no recording: {e:#}")),
            }
        }
        engine
    }

    fn open_output(&self, host: &Host, query: Option<&str>, rate: Option<u32>) -> Result<Output> {
        let device = find_device(host, query, false)?;
        let supported = pick_config(&device, false, rate)?;
        let config = supported.config();
        let info = DeviceInfo {
            name: device_label(&device),
            rate: config.sample_rate,
            channels: config.channels,
            format: supported.sample_format(),
        };
        if info.channels == 0 {
            bail!("{} reports zero output channels", info.name);
        }
        let (cmd_tx, cmd_rx) = RingBuffer::new(256);
        let (garbage_tx, garbage_rx) = RingBuffer::new(256);
        let state = OutState {
            cmds: cmd_rx,
            garbage: garbage_tx,
            tracks: Arc::new(Vec::new()),
            playing: false,
            pos: 0,
            mix: vec![0.0; 16384],
            peaks: vec![0.0; MAX_METER_TRACKS],
            channels: info.channels as usize,
            shared: self.shared.clone(),
        };
        let stream = with_sample_type!(info.format, build_output(&device, config, state))?;
        stream.play()?;
        Ok(Output { _stream: stream, cmds: cmd_tx, garbage: garbage_rx, info })
    }

    fn open_input(&self, host: &Host, query: Option<&str>, rate: Option<u32>) -> Result<Input> {
        let device = find_device(host, query, true)?;
        let supported = pick_config(&device, true, rate)?;
        let config = supported.config();
        let info = DeviceInfo {
            name: device_label(&device),
            rate: config.sample_rate,
            channels: config.channels,
            format: supported.sample_format(),
        };
        if info.channels == 0 {
            bail!("{} reports zero input channels", info.name);
        }
        // Four seconds of slack for UI hiccups.
        let (tx, rx) = RingBuffer::new(info.rate as usize * info.channels as usize * 4);
        let stream = with_sample_type!(info.format, build_input(&device, config, tx, self.shared.clone()))?;
        stream.play()?;
        Ok(Input { _stream: stream, ring: rx, info })
    }

    fn send(&mut self, cmd: Cmd) {
        self.collect_garbage();
        if let Some(out) = &mut self.output {
            // A full command ring means the callback is not running at all.
            let _ = out.cmds.push(cmd);
        }
    }

    /// Hand the audio thread a fresh copy of the tracks (cheap: audio is Arc'd).
    pub fn set_tracks(&mut self, tracks: &[Track]) {
        self.send(Cmd::Tracks(Arc::new(tracks.to_vec())));
    }

    pub fn play(&mut self, from: u64) {
        if self.output.is_some() {
            self.shared.playhead.store(from, Relaxed);
            self.shared.playing.store(true, Relaxed);
        }
        self.send(Cmd::Play(from));
    }

    pub fn stop(&mut self) {
        self.shared.playing.store(false, Relaxed);
        self.send(Cmd::Stop);
    }

    pub fn playhead(&self) -> u64 {
        self.shared.playhead.load(Relaxed)
    }

    /// Free snapshots the audio thread has finished with.
    pub fn collect_garbage(&mut self) {
        if let Some(out) = &mut self.output {
            while out.garbage.pop().is_ok() {}
        }
    }

    pub fn output_rate(&self) -> Option<u32> {
        self.output.as_ref().map(|o| o.info.rate)
    }
}

/// Redirect fd 2 while alive. ALSA prints configuration chatter straight to
/// stderr, which would scribble over the TUI.
pub struct StderrRedirect {
    saved: libc::c_int,
}

impl StderrRedirect {
    pub fn to(path: &std::path::Path) -> Option<StderrRedirect> {
        use std::os::fd::IntoRawFd;
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
        // SAFETY: plain fd juggling; `saved` is restored in Drop.
        unsafe {
            let saved = libc::dup(2);
            if saved < 0 {
                return None;
            }
            let fd = file.into_raw_fd();
            libc::dup2(fd, 2);
            libc::close(fd);
            Some(StderrRedirect { saved })
        }
    }
}

impl Drop for StderrRedirect {
    fn drop(&mut self) {
        // SAFETY: restores the descriptor saved in `to`.
        unsafe {
            libc::dup2(self.saved, 2);
            libc::close(self.saved);
        }
    }
}
