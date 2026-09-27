//! Application state and every user action. Drawing lives in `ui.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::audio::{AudioData, Peaks, gain_to_db, resample};
use crate::engine::{Engine, MAX_METER_TRACKS};
use crate::mix;
use crate::project::{Clip, Input, Project, Track};
use crate::wav::{self, Bits, WavOut};

const UNDO_LIMIT: usize = 200;
/// Meter fall-off per UI tick (~30 Hz): about 20 dB/s.
const METER_DECAY: f32 = 0.86;

pub fn fmt_time(frames: u64, rate: u32) -> String {
    let ms_total = frames * 1000 / rate.max(1) as u64;
    let (h, m, s, ms) = (ms_total / 3_600_000, ms_total / 60_000 % 60, ms_total / 1000 % 60, ms_total % 1000);
    if h > 0 { format!("{h}:{m:02}:{s:02}.{ms:03}") } else { format!("{m}:{s:02}.{ms:03}") }
}

/// "90", "90s", "1:30", "1:02:03.5" -> seconds.
pub fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim().trim_end_matches('s');
    if s.is_empty() {
        return None;
    }
    let mut secs = 0f64;
    for part in s.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    (secs >= 0.0 && secs.is_finite()).then_some(secs)
}

/// "C", "L30", "R100", "-0.5", "40" -> -1..=1.
pub fn parse_pan(s: &str) -> Option<f32> {
    let s = s.trim().to_ascii_uppercase();
    let v = if s == "C" || s == "CENTER" || s == "CENTRE" {
        0.0
    } else if let Some(n) = s.strip_prefix('L') {
        -n.parse::<f32>().ok()? / 100.0
    } else if let Some(n) = s.strip_prefix('R') {
        n.parse::<f32>().ok()? / 100.0
    } else {
        let v: f32 = s.parse().ok()?;
        if v.abs() > 1.0 { v / 100.0 } else { v }
    };
    (v.is_finite() && v.abs() <= 1.0).then_some(v)
}

pub fn fmt_pan(pan: f32) -> String {
    let p = (pan * 100.0).round() as i32;
    match p {
        0 => "C".into(),
        p if p < 0 => format!("L{}", -p),
        p => format!("R{p}"),
    }
}

fn expand_tilde(s: &str) -> PathBuf {
    match (s.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(s),
    }
}

/// First `<dir>/<stem>-NNN<ext>` that doesn't exist yet.
fn numbered(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    (1..).map(|n| dir.join(format!("{stem}-{n:03}{ext}"))).find(|p| !p.exists()).expect("unbounded")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Warn,
    Error,
}

pub struct Status {
    pub text: String,
    pub kind: StatusKind,
    at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Command(String),
    Help,
    ConfirmQuit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditTag {
    Nudge(usize, usize),
    Gain(usize, usize),
}

/// One armed track's in-progress recording.
pub struct Take {
    pub track: usize,
    pub input: Input,
    pub file: String,
    writer: Option<WavOut>,
    /// The streaming writer failed; the take is rewritten from memory on stop.
    failed: bool,
    pub data: Vec<Vec<f32>>,
    pub peaks: Peaks,
    scratch: Vec<f32>,
}

pub struct Recording {
    /// Timeline frame the take starts at (before latency compensation).
    pub start: u64,
    /// Input device rate; takes are resampled to the project rate on stop.
    pub rate: u32,
    pub takes: Vec<Take>,
    last_flush: Instant,
    /// Input-overrun counter already accounted for with silence.
    overruns_seen: u64,
}

impl Recording {
    /// Frames captured so far, in input-device frames.
    pub fn frames(&self) -> u64 {
        self.takes.first().map_or(0, |t| t.data[0].len() as u64)
    }

    /// Append captured frames, then `gap` frames of silence standing in for
    /// buffers the input ring had to drop, so later audio stays in time.
    /// A failing file doesn't stop capture: every take keeps recording into
    /// memory and a broken file is rewritten on stop.
    fn append(&mut self, buf: &[f32], dev_channels: usize, gap: u64) -> Result<(), String> {
        let mut first_err = None;
        for take in &mut self.takes {
            let c0 = take.input.channel as usize;
            let n = take.input.channels() as usize;
            take.scratch.clear();
            for frame in buf.chunks_exact(dev_channels) {
                for k in 0..n {
                    let s = frame.get(c0 + k).copied().unwrap_or(0.0);
                    take.data[k].push(s);
                    take.scratch.push(s);
                }
            }
            let silence = gap as usize * n;
            take.scratch.resize(take.scratch.len() + silence, 0.0);
            for ch in &mut take.data {
                ch.resize(ch.len() + gap as usize, 0.0);
            }
            let flush = self.last_flush.elapsed() > Duration::from_secs(1);
            if let Some(w) = &mut take.writer {
                // Flushing keeps the WAV header current so a crash leaves a playable file.
                let r = w.write_interleaved(&take.scratch).and_then(|_| if flush { w.flush() } else { Ok(()) });
                if let Err(e) = r {
                    first_err.get_or_insert_with(|| format!("{}: {e:#}", take.file));
                    take.writer = None;
                    take.failed = true;
                }
            }
            take.peaks.extend(&take.data);
        }
        if self.last_flush.elapsed() > Duration::from_secs(1) {
            self.last_flush = Instant::now();
        }
        first_err.map_or(Ok(()), Err)
    }
}

/// A clip being dragged with the mouse.
pub struct Drag {
    pub from_track: usize,
    pub clip: usize,
    /// Identity of the grabbed clip, checked on release in case it changed.
    source: Arc<AudioData>,
    start: u64,
    grab: u64,
    pub to_track: usize,
    pub to_start: u64,
    pub moved: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TrackHit {
    pub track: usize,
    pub header: Rect,
    pub lane: Rect,
    pub mute: Rect,
    pub solo: Rect,
    pub arm: Rect,
    pub input: Rect,
    pub gain: Rect,
    pub pan: Rect,
}

/// Screen regions from the last draw, for mouse hit-testing.
#[derive(Debug, Clone, Default)]
pub struct Hits {
    pub ruler: Rect,
    pub tracks: Vec<TrackHit>,
    pub play: Rect,
    pub rec: Rect,
    pub stop: Rect,
    /// Timeline columns.
    pub lanes_x: u16,
    pub lanes_w: u16,
    pub visible_tracks: usize,
}

pub struct App {
    pub project: Project,
    pub engine: Engine,
    pub sel_track: usize,
    pub sel_clip: Option<usize>,
    pub cursor: u64,
    /// Timeline frame at the left edge of the lanes.
    pub view_start: u64,
    /// Frames per screen column.
    pub zoom: u64,
    pub track_scroll: usize,
    pub markers: (Option<u64>, Option<u64>),
    pub dirty: bool,
    pub mode: Mode,
    pub status: Option<Status>,
    /// `Some(where playback started)` while the transport runs.
    pub play_origin: Option<u64>,
    pub recording: Option<Recording>,
    pub in_levels: Vec<f32>,
    pub out_levels: [f32; 2],
    pub track_levels: Vec<f32>,
    pub drag: Option<Drag>,
    pub hits: Hits,
    pub bits: Bits,
    pub quit: bool,
    pub fit_pending: bool,
    clipboard: Option<Clip>,
    undo: Vec<Vec<Track>>,
    redo: Vec<Vec<Track>>,
    last_edit: Option<EditTag>,
    in_buf: Vec<f32>,
    overruns_seen: u64,
    overrun_warned: Option<Instant>,
    ruler_anchor: Option<u64>,
}

impl App {
    pub fn new(project: Project, engine: Engine) -> App {
        let has_audio = project.end() > 0;
        let mut app = App {
            zoom: (project.rate as u64 / 10).max(1),
            project,
            engine,
            sel_track: 0,
            sel_clip: None,
            cursor: 0,
            view_start: 0,
            track_scroll: 0,
            markers: (None, None),
            dirty: false,
            mode: Mode::Normal,
            status: None,
            play_origin: None,
            recording: None,
            in_levels: Vec::new(),
            out_levels: [0.0; 2],
            track_levels: Vec::new(),
            drag: None,
            hits: Hits::default(),
            bits: Bits::I24,
            quit: false,
            fit_pending: has_audio,
            clipboard: None,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: None,
            in_buf: Vec::new(),
            overruns_seen: 0,
            overrun_warned: None,
            ruler_anchor: None,
        };
        app.sync();
        app
    }

    // ---- status -------------------------------------------------------

    pub fn info(&mut self, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), kind: StatusKind::Info, at: Instant::now() });
    }

    pub fn warn(&mut self, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), kind: StatusKind::Warn, at: Instant::now() });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), kind: StatusKind::Error, at: Instant::now() });
    }

    fn report<T>(&mut self, r: Result<T>) -> Option<T> {
        r.map_err(|e| self.error(format!("{e:#}"))).ok()
    }

    // ---- derived state ------------------------------------------------

    pub fn rate(&self) -> u32 {
        self.project.rate
    }

    pub fn is_playing(&self) -> bool {
        self.play_origin.is_some()
    }

    /// Where "now" is: the take's end while recording, the playhead while
    /// playing, else the edit cursor.
    pub fn position(&self) -> u64 {
        if let Some(rec) = &self.recording {
            rec.start + rec.frames() * self.rate() as u64 / rec.rate.max(1) as u64
        } else if self.is_playing() {
            self.engine.playhead()
        } else {
            self.cursor
        }
    }

    pub fn input_channels(&self) -> Option<u16> {
        self.engine.input.as_ref().map(|i| i.info.channels)
    }

    pub fn selected_clip(&self) -> Option<&Clip> {
        self.sel_clip.and_then(|i| self.project.tracks[self.sel_track].clips.get(i))
    }

    fn lane_frames(&self) -> u64 {
        self.hits.lanes_w.max(1) as u64 * self.zoom
    }

    // ---- sync, undo -----------------------------------------------------

    /// Push the current tracks to the audio thread and fix up selection.
    fn sync(&mut self) {
        if self.project.tracks.is_empty() {
            self.project.tracks.push(Track::new("Track 1", Input { channel: 0, stereo: false }));
        }
        self.sel_track = self.sel_track.min(self.project.tracks.len() - 1);
        let n = self.project.tracks[self.sel_track].clips.len();
        self.sel_clip = self.sel_clip.filter(|&i| i < n);
        self.engine.set_tracks(&self.project.tracks);
    }

    /// Record an undo point before a clip/track edit. Repeated edits with the
    /// same tag (e.g. holding the nudge key) collapse into one step.
    fn checkpoint(&mut self, tag: Option<EditTag>) {
        self.dirty = true;
        if tag.is_some() && tag == self.last_edit {
            return;
        }
        self.last_edit = tag;
        self.undo.push(self.project.tracks.clone());
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Swap in a snapshot but keep live mixer settings (arm, mute, gain...)
    /// when the track layout matches: undo is for edits, not faders.
    fn restore(&mut self, mut snap: Vec<Track>) -> Vec<Track> {
        if snap.len() == self.project.tracks.len() {
            for (s, cur) in snap.iter_mut().zip(&self.project.tracks) {
                s.copy_mixer_from(cur);
            }
        }
        std::mem::replace(&mut self.project.tracks, snap)
    }

    pub fn undo(&mut self) {
        if self.recording.is_some() {
            return self.warn("stop recording before undoing");
        }
        match self.undo.pop() {
            Some(snap) => {
                let cur = self.restore(snap);
                self.redo.push(cur);
                self.after_history("undo");
            }
            None => self.info("nothing to undo"),
        }
    }

    pub fn redo(&mut self) {
        if self.recording.is_some() {
            return self.warn("stop recording before redoing");
        }
        match self.redo.pop() {
            Some(snap) => {
                let cur = self.restore(snap);
                self.undo.push(cur);
                self.after_history("redo");
            }
            None => self.info("nothing to redo"),
        }
    }

    fn after_history(&mut self, what: &str) {
        self.last_edit = None;
        self.dirty = true;
        self.sync();
        self.info(format!("{what} ({} more)", if what == "undo" { self.undo.len() } else { self.redo.len() }));
    }

    // ---- per-frame work ---------------------------------------------------

    pub fn tick(&mut self) {
        let mut buf = std::mem::take(&mut self.in_buf);
        buf.clear();
        // Read before draining: anything dropped so far happened after the
        // ring's current contents (the ring stays full until we drain it).
        let overruns = self.engine.shared.input_overruns.load(Relaxed);
        if let Some(input) = &mut self.engine.input {
            input.drain(&mut buf);
            let ch = input.info.channels as usize;
            self.in_levels.resize(ch, 0.0);
            for (c, level) in self.in_levels.iter_mut().enumerate() {
                let peak = buf.iter().skip(c).step_by(ch).fold(0f32, |m, s| m.max(s.abs()));
                *level = peak.max(*level * METER_DECAY);
            }
            if let Some(rec) = &mut self.recording {
                let gap = overruns.saturating_sub(rec.overruns_seen).min(rec.rate as u64 * 60);
                rec.overruns_seen = overruns;
                if let Err(e) = rec.append(&buf, ch, gap) {
                    let text = format!("writing {e} — still recording to memory");
                    self.status = Some(Status { text, kind: StatusKind::Error, at: Instant::now() });
                }
            }
        }
        self.in_buf = buf;

        let out = self.engine.shared.take_out_peaks();
        for (level, p) in self.out_levels.iter_mut().zip(out) {
            *level = p.max(*level * METER_DECAY);
        }
        let n = self.project.tracks.len().min(MAX_METER_TRACKS);
        self.track_levels.resize(n, 0.0);
        for (i, level) in self.track_levels.iter_mut().enumerate() {
            *level = self.engine.shared.take_track_peak(i).max(*level * METER_DECAY);
        }

        for e in self.engine.shared.take_errors() {
            self.warn(e);
        }
        let overruns = self.engine.shared.input_overruns.load(Relaxed);
        if overruns != self.overruns_seen && self.overrun_warned.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
            // Once per 10 s at most, so it can't drown out everything else.
            self.overruns_seen = overruns;
            self.overrun_warned = Some(Instant::now());
            self.warn(format!("input overrun: {overruns} frames dropped so far (system too busy?)"));
        }

        if self.is_playing() && self.recording.is_none() {
            let end = self.project.end();
            if self.engine.playhead() > end + self.rate() as u64 / 2 {
                self.stop();
            }
        }
        if self.is_playing() || self.recording.is_some() {
            self.follow(self.position());
        }
        if let Some(s) = &self.status {
            let ttl = if s.kind == StatusKind::Info { 6 } else { 15 };
            if s.at.elapsed() > Duration::from_secs(ttl) {
                self.status = None;
            }
        }
        self.engine.collect_garbage();
    }

    /// Page the view so `pos` stays on screen while the transport runs.
    fn follow(&mut self, pos: u64) {
        let span = self.lane_frames();
        if pos < self.view_start || pos >= self.view_start + span * 19 / 20 {
            self.view_start = pos.saturating_sub(span / 20);
        }
    }

    fn ensure_visible(&mut self, pos: u64) {
        let span = self.lane_frames();
        if pos < self.view_start || pos >= self.view_start + span {
            self.view_start = pos.saturating_sub(span / 4);
        }
    }

    // ---- transport ----------------------------------------------------------

    pub fn toggle_play(&mut self) {
        if self.recording.is_some() {
            self.stop_record();
        } else if self.is_playing() {
            self.stop();
        } else {
            self.play_from(self.cursor);
        }
    }

    fn play_from(&mut self, from: u64) {
        if self.engine.output.is_none() {
            return self.error("no output device (see `asciidaw devices`)");
        }
        self.engine.play(from);
        self.play_origin = Some(from);
    }

    /// Stop and return the cursor to where playback started.
    pub fn stop(&mut self) {
        if self.recording.is_some() {
            return self.stop_record();
        }
        self.engine.stop();
        if let Some(origin) = self.play_origin.take() {
            self.cursor = origin;
            self.ensure_visible(origin);
        }
    }

    /// Stop and leave the cursor at the playhead.
    pub fn pause(&mut self) {
        if self.recording.is_some() {
            return self.stop_record();
        }
        if self.is_playing() {
            let pos = self.engine.playhead();
            self.engine.stop();
            self.play_origin = None;
            self.cursor = pos;
        }
    }

    pub fn toggle_record(&mut self) {
        if self.recording.is_some() {
            self.stop_record();
        } else {
            let r = self.start_record();
            self.report(r);
        }
    }

    fn start_record(&mut self) -> Result<()> {
        let Some(input) = &self.engine.input else {
            bail!("no input device (see `asciidaw devices`)");
        };
        let (dev_ch, in_rate) = (input.info.channels, input.info.rate);
        if self.is_playing() {
            self.pause();
        }
        if !self.project.tracks.iter().any(|t| t.armed) {
            self.project.tracks[self.sel_track].armed = true;
        }
        let armed: Vec<usize> = (0..self.project.tracks.len()).filter(|&i| self.project.tracks[i].armed).collect();
        for &i in &armed {
            let t = &self.project.tracks[i];
            if !t.input.fits(dev_ch) {
                bail!("{} records input {} but the device has {dev_ch} channel(s); try :input 1", t.name, t.input);
            }
        }
        std::fs::create_dir_all(self.project.audio_dir())
            .with_context(|| format!("creating {}", self.project.audio_dir().display()))?;
        let mut takes = Vec::new();
        for &i in &armed {
            let t = &self.project.tracks[i];
            let file = self.project.fresh_audio_name(&format!("{}-take", t.name));
            let path = self.project.audio_dir().join(&file);
            // Takes are kept as 32-bit float: no clipping, no re-quantisation.
            let writer = WavOut::create(&path, in_rate, t.input.channels(), Bits::F32)?;
            takes.push(Take {
                track: i,
                input: t.input,
                file,
                writer: Some(writer),
                failed: false,
                data: vec![Vec::new(); t.input.channels() as usize],
                peaks: Peaks::default(),
                scratch: Vec::new(),
            });
        }
        // Throw away whatever was captured before the button press.
        let mut stale = std::mem::take(&mut self.in_buf);
        if let Some(input) = &mut self.engine.input {
            input.drain(&mut stale);
        }
        stale.clear();
        self.in_buf = stale;

        let start = self.cursor;
        let overruns_seen = self.engine.shared.input_overruns.load(Relaxed);
        self.recording = Some(Recording { start, rate: in_rate, takes, last_flush: Instant::now(), overruns_seen });
        if self.engine.output.is_some() {
            self.engine.play(start);
            self.play_origin = Some(start);
        }
        let names: Vec<_> = armed.iter().map(|&i| self.project.tracks[i].name.clone()).collect();
        self.info(format!("recording {} (space/r to stop)", names.join(", ")));
        Ok(())
    }

    fn stop_record(&mut self) {
        self.tick(); // pick up the last buffers
        let Some(rec) = self.recording.take() else { return };
        self.engine.stop();
        self.play_origin = None;
        let rate = self.rate();
        let before = self.project.tracks.clone();
        let latency = self.project.latency_frames();
        let mut placed = Vec::new();
        let mut problems = Vec::new();
        for take in rec.takes {
            let Take { track, file, writer, failed, data, .. } = take;
            let path = self.project.audio_dir().join(&file);
            if let Some(w) = writer
                && let Err(e) = w.finalize()
            {
                problems.push(format!("{file}: {e:#}"));
            }
            if data[0].is_empty() {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let data = if rec.rate != rate { data.iter().map(|c| resample(c, rec.rate, rate)).collect() } else { data };
            if (failed || rec.rate != rate)
                && let Err(e) = wav::write_wav(&path, rate, &data, Bits::F32)
            {
                problems.push(format!("{file}: {e:#} (take kept in memory only — save elsewhere with :clip)"));
            }
            let stem = file.trim_end_matches(".wav").to_string();
            let mut clip = Clip::new(stem, Arc::new(AudioData::new(file.clone(), data)), rec.start);
            // Shift earlier by the round-trip latency so the take lines up.
            if latency > 0 {
                let cut = latency.saturating_sub(rec.start).min(clip.len.saturating_sub(1));
                clip.start = rec.start.saturating_sub(latency);
                clip.offset = cut;
                clip.len -= cut;
            }
            let secs = self.project.frames_to_secs(clip.len);
            let idx = self.project.tracks[track].insert(clip);
            placed.push((track, idx, file, secs));
        }
        self.cursor = rec.start;
        self.ensure_visible(rec.start);
        if placed.is_empty() {
            self.sync();
            return self.warn("nothing was recorded");
        }
        self.undo.push(before);
        self.redo.clear();
        self.last_edit = None;
        self.sel_track = placed[0].0;
        self.sel_clip = Some(placed[0].1);
        self.sync();
        // Recording is the one thing you can't redo, so persist immediately.
        let saved = match self.project.save() {
            Ok(()) => {
                self.dirty = false;
                "saved"
            }
            Err(e) => {
                problems.push(format!("save failed: {e:#}"));
                self.dirty = true;
                "NOT saved"
            }
        };
        let files: Vec<_> = placed.iter().map(|p| format!("audio/{}", p.2)).collect();
        let msg = format!("recorded {:.1}s → {} ({saved})", placed[0].3, files.join(", "));
        if problems.is_empty() { self.info(msg) } else { self.error(format!("{msg}; {}", problems.join("; "))) }
    }

    // ---- navigation -----------------------------------------------------------

    fn set_cursor(&mut self, pos: u64) {
        self.cursor = pos;
        if self.is_playing() && self.recording.is_none() {
            self.engine.play(pos);
            self.play_origin = Some(pos);
        }
        self.ensure_visible(pos);
    }

    fn move_cursor(&mut self, cols: i64) {
        let delta = cols.unsigned_abs() * self.zoom;
        let pos = if cols < 0 { self.cursor.saturating_sub(delta) } else { self.cursor + delta };
        self.set_cursor(pos);
    }

    fn select_track(&mut self, delta: i64) {
        let n = self.project.tracks.len() as i64;
        self.sel_track = (self.sel_track as i64 + delta).clamp(0, n - 1) as usize;
        self.sel_clip = self.project.tracks[self.sel_track].clip_at(self.cursor);
        let visible = self.hits.visible_tracks.max(1);
        if self.sel_track < self.track_scroll {
            self.track_scroll = self.sel_track;
        } else if self.sel_track >= self.track_scroll + visible {
            self.track_scroll = self.sel_track + 1 - visible;
        }
    }

    fn jump_clip(&mut self, forward: bool) {
        let clips = &self.project.tracks[self.sel_track].clips;
        let pick = if forward {
            clips.iter().position(|c| c.start > self.cursor)
        } else {
            clips.iter().rposition(|c| c.start < self.cursor)
        };
        match pick {
            Some(i) => {
                let start = clips[i].start;
                self.sel_clip = Some(i);
                self.set_cursor(start);
            }
            None => self.info(if forward { "no clip after the cursor" } else { "no clip before the cursor" }),
        }
    }

    pub fn zoom_by(&mut self, zoom_in: bool, anchor: u64) {
        let old = self.zoom;
        let max = self.rate() as u64 * 30;
        self.zoom = if zoom_in { (old / 2).max(4) } else { (old * 2).min(max) };
        let col = anchor.saturating_sub(self.view_start) / old;
        self.view_start = anchor.saturating_sub(col * self.zoom);
    }

    pub fn zoom_fit(&mut self) {
        let end = self.project.end().max(self.cursor).max(self.rate() as u64 * 5);
        let w = self.hits.lanes_w.max(10) as u64;
        self.zoom = (end * 21 / 20).div_ceil(w).max(4);
        self.view_start = 0;
    }

    fn scroll(&mut self, cols: i64) {
        let delta = cols.unsigned_abs() * self.zoom;
        self.view_start = if cols < 0 { self.view_start.saturating_sub(delta) } else { self.view_start + delta };
    }

    // ---- track ops --------------------------------------------------------------

    fn toggle_arm(&mut self, i: usize) {
        if self.recording.is_some() {
            return self.warn("can't change arming while recording");
        }
        let t = &mut self.project.tracks[i];
        t.armed = !t.armed;
    }

    fn toggle_mute(&mut self, i: usize) {
        let t = &mut self.project.tracks[i];
        t.mute = !t.mute;
        self.dirty = true;
        self.sync();
    }

    fn toggle_solo(&mut self, i: usize) {
        let t = &mut self.project.tracks[i];
        t.solo = !t.solo;
        self.dirty = true;
        self.sync();
    }

    fn nudge_gain(&mut self, i: usize, db: f32) {
        let t = &mut self.project.tracks[i];
        t.gain_db = ((t.gain_db + db) * 10.0).round() / 10.0;
        t.gain_db = t.gain_db.clamp(-60.0, 12.0);
        let g = t.gain_db;
        self.dirty = true;
        self.sync();
        self.info(format!("{}: {g:+.1} dB", self.project.tracks[i].name));
    }

    fn nudge_pan(&mut self, i: usize, delta: f32) {
        let t = &mut self.project.tracks[i];
        t.pan = ((t.pan + delta) * 100.0).round().clamp(-100.0, 100.0) / 100.0;
        self.dirty = true;
        self.sync();
    }

    fn cycle_input(&mut self, i: usize) {
        if self.recording.is_some() {
            return self.warn("can't change inputs while recording");
        }
        let dev = self.input_channels().unwrap_or(2).max(1);
        let cur = self.project.tracks[i].input;
        // 1, 2, ..., n, then stereo pairs 1-2, 2-3, ...
        let mut options: Vec<Input> = (0..dev).map(|c| Input { channel: c, stereo: false }).collect();
        options.extend((0..dev.saturating_sub(1)).map(|c| Input { channel: c, stereo: true }));
        let next = options.iter().position(|o| *o == cur).map_or(0, |p| (p + 1) % options.len());
        self.project.tracks[i].input = options[next];
        self.dirty = true;
    }

    fn add_track(&mut self, name: Option<String>) {
        self.checkpoint(None);
        let dev = self.input_channels().unwrap_or(2);
        let next_in = self.project.tracks.last().map_or(0, |t| t.input.channel as u32 + t.input.channels() as u32);
        let input = Input { channel: if next_in < dev as u32 { next_in as u16 } else { 0 }, stereo: false };
        let n = self.project.tracks.len() + 1;
        self.project.tracks.push(Track::new(name.unwrap_or_else(|| format!("Track {n}")), input));
        self.sel_track = n - 1;
        self.sel_clip = None;
        self.sync();
    }

    fn delete_track(&mut self) {
        if self.recording.is_some() {
            return self.warn("can't delete tracks while recording");
        }
        if self.project.tracks.len() == 1 {
            return self.warn("can't delete the last track");
        }
        self.checkpoint(None);
        let t = self.project.tracks.remove(self.sel_track);
        self.sel_clip = None;
        self.sync();
        self.info(format!("deleted {} (u to undo)", t.name));
    }

    // ---- clip ops -------------------------------------------------------------

    /// Selected clip, or the one under the cursor on the selected track.
    fn target_clip(&mut self) -> Option<usize> {
        let n = self.project.tracks[self.sel_track].clips.len();
        self.sel_clip = self.sel_clip.filter(|&i| i < n);
        if self.sel_clip.is_none() {
            self.sel_clip = self.project.tracks[self.sel_track].clip_at(self.cursor);
        }
        if self.sel_clip.is_none() {
            self.info("no clip selected");
        }
        self.sel_clip
    }

    fn split(&mut self) {
        let t = self.sel_track;
        let Some(i) = self.project.tracks[t].clip_at(self.cursor) else {
            return self.info("cursor is not inside a clip on this track");
        };
        let Some((a, b)) = self.project.tracks[t].clips[i].split(self.cursor) else {
            return self.info("cursor is on the clip edge");
        };
        self.checkpoint(None);
        let clips = &mut self.project.tracks[t].clips;
        clips[i] = a;
        clips.insert(i + 1, b);
        self.sel_clip = Some(i + 1);
        self.sync();
    }

    fn delete_clip(&mut self) {
        let Some(i) = self.target_clip() else { return };
        self.checkpoint(None);
        self.project.tracks[self.sel_track].clips.remove(i);
        self.sel_clip = None;
        self.sync();
    }

    fn copy_clip(&mut self) -> bool {
        let Some(i) = self.target_clip() else { return false };
        let clip = self.project.tracks[self.sel_track].clips[i].clone();
        self.info(format!("copied {}", clip.name));
        self.clipboard = Some(clip);
        true
    }

    fn cut_clip(&mut self) {
        if self.copy_clip() {
            self.delete_clip();
        }
    }

    fn paste(&mut self) {
        let Some(mut clip) = self.clipboard.clone() else { return self.info("clipboard is empty") };
        self.checkpoint(None);
        clip.start = self.cursor;
        let end = clip.end();
        let idx = self.project.tracks[self.sel_track].insert(clip);
        self.sel_clip = Some(idx);
        self.cursor = end; // paste again to append
        self.ensure_visible(end);
        self.sync();
    }

    fn duplicate(&mut self) {
        let Some(i) = self.target_clip() else { return };
        self.checkpoint(None);
        let track = &mut self.project.tracks[self.sel_track];
        let mut copy = track.clips[i].clone();
        copy.start = copy.end();
        let idx = track.insert(copy);
        self.sel_clip = Some(idx);
        self.sync();
    }

    /// Move the selected clip by `cols` screen columns, stopping at neighbours.
    fn nudge_clip(&mut self, cols: i64) {
        let Some(i) = self.target_clip() else { return };
        let t = self.sel_track;
        let (lo, hi) = self.project.tracks[t].free_span(i);
        let c = &self.project.tracks[t].clips[i];
        let delta = cols.unsigned_abs() * self.zoom;
        let new_start = if cols < 0 {
            c.start.saturating_sub(delta).max(lo)
        } else {
            (c.start + delta).min(hi.saturating_sub(c.len))
        };
        if new_start == c.start {
            return;
        }
        self.checkpoint(Some(EditTag::Nudge(t, i)));
        self.project.tracks[t].clips[i].start = new_start;
        self.sync();
    }

    fn fade_to_cursor(&mut self, fade_in: bool) {
        let t = self.sel_track;
        let Some(i) = self.project.tracks[t].clip_at(self.cursor).or(self.sel_clip) else {
            return self.info("put the cursor inside a clip");
        };
        let c = &self.project.tracks[t].clips[i];
        let (fi, fo) = if fade_in {
            (self.cursor.clamp(c.start, c.end()) - c.start, c.fade_out)
        } else {
            (c.fade_in, c.end() - self.cursor.clamp(c.start, c.end()))
        };
        if fi + fo > c.len {
            return self.warn("fades would overlap");
        }
        self.checkpoint(None);
        let c = &mut self.project.tracks[t].clips[i];
        c.fade_in = fi;
        c.fade_out = fo;
        let secs = self.project.frames_to_secs(if fade_in { fi } else { fo });
        self.sel_clip = Some(i);
        self.sync();
        self.info(format!("fade {} {secs:.2}s", if fade_in { "in" } else { "out" }));
    }

    fn set_fade_secs(&mut self, fade_in: bool, secs: f64) {
        let Some(i) = self.target_clip() else { return };
        let frames = self.project.secs_to_frames(secs);
        let c = &self.project.tracks[self.sel_track].clips[i];
        let other = if fade_in { c.fade_out } else { c.fade_in };
        if frames + other > c.len {
            return self.warn("fades would overlap");
        }
        self.checkpoint(None);
        let c = &mut self.project.tracks[self.sel_track].clips[i];
        if fade_in {
            c.fade_in = frames
        } else {
            c.fade_out = frames
        }
        self.sync();
    }

    fn set_clip_gain(&mut self, db: f32) {
        let Some(i) = self.target_clip() else { return };
        self.checkpoint(Some(EditTag::Gain(self.sel_track, i)));
        self.project.tracks[self.sel_track].clips[i].gain_db = db.clamp(-60.0, 40.0);
        self.sync();
        self.info(format!("clip gain {db:+.1} dB"));
    }

    fn normalize(&mut self, target_db: f32) {
        let Some(i) = self.target_clip() else { return };
        let c = &self.project.tracks[self.sel_track].clips[i];
        let raw = c.source.peak(c.offset as usize, (c.offset + c.len) as usize);
        if raw <= 0.0 {
            return self.warn("clip is silent");
        }
        self.set_clip_gain(target_db - gain_to_db(raw));
    }

    // ---- files ------------------------------------------------------------------

    pub fn save(&mut self) -> bool {
        match self.project.save() {
            Ok(()) => {
                self.dirty = false;
                self.info(format!("saved {}", self.project.dir.join(crate::project::PROJECT_FILE).display()));
                true
            }
            Err(e) => {
                self.error(format!("save failed: {e:#}"));
                false
            }
        }
    }

    fn exports_dir(&self) -> PathBuf {
        self.project.dir.join("exports")
    }

    fn parse_export_args(&self, rest: &str) -> Result<(Option<PathBuf>, bool, Bits)> {
        let (mut path, mut mono, mut bits) = (Vec::new(), false, self.bits);
        for tok in rest.split_whitespace() {
            if tok == "mono" {
                mono = true;
            } else if let Ok(b) = tok.parse::<Bits>() {
                bits = b;
            } else {
                path.push(tok);
            }
        }
        let path = (!path.is_empty()).then(|| expand_tilde(&path.join(" ")));
        Ok((path, mono, bits))
    }

    fn describe(&self, r: &mix::Rendered, bits: Bits) -> String {
        let clip = if r.clipped(bits) { " — CLIPPED, lower the gain" } else { "" };
        format!(
            "{} ({}, {bits}, peak {:.1} dBFS){clip}",
            r.path.display(),
            fmt_time(r.frames, self.rate()),
            gain_to_db(r.peak)
        )
    }

    pub fn export_mix(&mut self, rest: &str) {
        if self.recording.is_some() {
            return self.warn("stop recording first");
        }
        let r = (|| {
            let (path, mono, bits) = self.parse_export_args(rest)?;
            let stem = format!("{}-mix", self.project.name());
            let path = path.unwrap_or_else(|| numbered(&self.exports_dir(), &stem, ".wav"));
            let range = mix::export_range(&self.project, self.markers);
            let r = mix::export_mix(&self.project, &path, range, mono, bits)?;
            Ok::<_, anyhow::Error>((r, bits))
        })();
        if let Some((r, bits)) = self.report(r) {
            let msg = format!("exported {}", self.describe(&r, bits));
            if r.clipped(bits) { self.warn(msg) } else { self.info(msg) }
        }
    }

    fn export_stems(&mut self, rest: &str) {
        if self.recording.is_some() {
            return self.warn("stop recording first");
        }
        let r = (|| {
            let (dir, _, bits) = self.parse_export_args(rest)?;
            let stem = format!("{}-stems", self.project.name());
            let dir = dir.unwrap_or_else(|| numbered(&self.exports_dir(), &stem, ""));
            let range = mix::export_range(&self.project, self.markers);
            let files = mix::export_stems(&self.project, &dir, range, bits)?;
            Ok::<_, anyhow::Error>((dir, files, bits))
        })();
        if let Some((dir, files, bits)) = self.report(r) {
            let clipped = files.iter().any(|f| f.clipped(bits));
            let msg =
                format!("{} stems → {}{}", files.len(), dir.display(), if clipped { " — some CLIPPED" } else { "" });
            if clipped { self.warn(msg) } else { self.info(msg) }
        }
    }

    fn export_clip(&mut self, rest: &str) {
        if self.recording.is_some() {
            return self.warn("stop recording first");
        }
        let Some(i) = self.target_clip() else { return };
        let clip = self.project.tracks[self.sel_track].clips[i].clone();
        let r = (|| {
            let (path, _, bits) = self.parse_export_args(rest)?;
            let path = path.unwrap_or_else(|| numbered(&self.exports_dir(), &clip.name, ".wav"));
            Ok::<_, anyhow::Error>((mix::export_clip(&clip, self.rate(), &path, bits)?, bits))
        })();
        if let Some((r, bits)) = self.report(r) {
            self.info(format!("exported clip {}", self.describe(&r, bits)));
        }
    }

    fn import(&mut self, rest: &str) {
        if self.recording.is_some() {
            return self.warn("stop recording first");
        }
        if rest.is_empty() {
            return self.error("usage: :import <file>");
        }
        let path = expand_tilde(rest);
        let r = self.project.import(&path);
        let Some(source) = self.report(r) else { return };
        self.checkpoint(None);
        let name = source.file.trim_end_matches(".wav").to_string();
        let clip = Clip::new(name, source, self.cursor);
        let secs = self.project.frames_to_secs(clip.len);
        let idx = self.project.tracks[self.sel_track].insert(clip);
        self.sel_clip = Some(idx);
        self.sync();
        self.info(format!("imported {} ({secs:.1}s)", path.display()));
    }

    // ---- quitting ---------------------------------------------------------------

    pub fn request_quit(&mut self) {
        if self.recording.is_some() {
            self.stop_record();
        }
        if self.dirty {
            self.mode = Mode::ConfirmQuit;
        } else {
            self.quit = true;
        }
    }

    /// Called on the way out no matter how we leave.
    pub fn shutdown(&mut self) {
        if self.recording.is_some() {
            self.stop_record();
        }
        self.engine.stop();
    }

    // ---- input dispatch ---------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        // Editing mid-drag would invalidate the dragged clip's index.
        self.drag = None;
        match &mut self.mode {
            Mode::Command(buf) => {
                match key.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Enter => {
                        let line = std::mem::take(buf);
                        self.mode = Mode::Normal;
                        self.run_command(&line);
                    }
                    KeyCode::Backspace => {
                        if buf.pop().is_none() {
                            self.mode = Mode::Normal;
                        }
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => buf.clear(),
                    KeyCode::Char(c) => buf.push(c),
                    _ => {}
                }
                return;
            }
            Mode::Help => {
                self.mode = Mode::Normal;
                return;
            }
            Mode::ConfirmQuit => {
                match key.code {
                    KeyCode::Char('s' | 'w') => {
                        self.mode = Mode::Normal;
                        if self.save() {
                            self.quit = true;
                        }
                    }
                    KeyCode::Char('q' | 'y' | 'd') => self.quit = true,
                    _ => {
                        self.mode = Mode::Normal;
                        self.info("quit cancelled");
                    }
                }
                return;
            }
            Mode::Normal => {}
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let step = if shift { 10 } else { 1 };
        let t = self.sel_track;
        match key.code {
            KeyCode::Char('c') if ctrl => self.request_quit(),
            KeyCode::Char('s') if ctrl => {
                self.save();
            }
            KeyCode::Char('z') if ctrl => self.undo(),
            KeyCode::Char('y' | 'r') if ctrl => self.redo(),
            KeyCode::Char(' ') => self.toggle_play(),
            KeyCode::Enter => self.pause(),
            KeyCode::Char('r') => self.toggle_record(),
            KeyCode::Left if alt => self.nudge_clip(-step),
            KeyCode::Right if alt => self.nudge_clip(step),
            KeyCode::Left | KeyCode::Char('h') => self.move_cursor(-step),
            KeyCode::Right | KeyCode::Char('l') => self.move_cursor(step),
            KeyCode::Char('H') => self.move_cursor(-10),
            KeyCode::Char('L') => self.move_cursor(10),
            KeyCode::Up | KeyCode::Char('k') => self.select_track(-1),
            KeyCode::Down | KeyCode::Char('j') => self.select_track(1),
            KeyCode::Home | KeyCode::Char('g') => self.set_cursor(0),
            KeyCode::End | KeyCode::Char('G') => self.set_cursor(self.project.end()),
            KeyCode::Tab => self.jump_clip(true),
            KeyCode::BackTab => self.jump_clip(false),
            KeyCode::Char('=' | '+') => self.zoom_by(true, self.position()),
            KeyCode::Char('-' | '_') => self.zoom_by(false, self.position()),
            KeyCode::Char('0') => self.zoom_fit(),
            KeyCode::Char('a') => self.toggle_arm(t),
            KeyCode::Char('m') => self.toggle_mute(t),
            KeyCode::Char('s') => self.toggle_solo(t),
            KeyCode::Char('[') => self.nudge_gain(t, -1.0),
            KeyCode::Char(']') => self.nudge_gain(t, 1.0),
            KeyCode::Char('n') => self.add_track(None),
            KeyCode::Char('b') => self.split(),
            KeyCode::Char('x') => self.cut_clip(),
            KeyCode::Char('c') => {
                self.copy_clip();
            }
            KeyCode::Char('v') => self.paste(),
            KeyCode::Char('d') => self.duplicate(),
            KeyCode::Delete | KeyCode::Backspace => self.delete_clip(),
            KeyCode::Char('{') => self.fade_to_cursor(true),
            KeyCode::Char('}') => self.fade_to_cursor(false),
            KeyCode::Char('i') => {
                self.markers.0 = Some(self.position());
                self.info(format!("in {}", fmt_time(self.position(), self.rate())));
            }
            KeyCode::Char('o') => {
                self.markers.1 = Some(self.position());
                self.info(format!("out {}", fmt_time(self.position(), self.rate())));
            }
            KeyCode::Esc => {
                self.markers = (None, None);
                self.sel_clip = None;
            }
            KeyCode::Char('u') => self.undo(),
            KeyCode::Char('U') => self.redo(),
            KeyCode::Char('e') => self.export_mix(""),
            KeyCode::Char(':') => self.mode = Mode::Command(String::new()),
            KeyCode::Char('?') | KeyCode::F(1) => self.mode = Mode::Help,
            KeyCode::Char('q') => self.request_quit(),
            _ => {}
        }
    }

    pub fn run_command(&mut self, line: &str) {
        let line = line.trim();
        let (cmd, rest) = line.split_once(char::is_whitespace).map_or((line, ""), |(a, b)| (a, b.trim()));
        let t = self.sel_track;
        match cmd {
            "" => {}
            "w" | "write" | "save" => {
                self.save();
            }
            "q" | "quit" => {
                if self.dirty {
                    self.error("unsaved changes: :w to save, :q! to discard");
                } else {
                    self.request_quit();
                }
            }
            "q!" | "quit!" => {
                if self.recording.is_some() {
                    self.stop_record();
                }
                self.quit = true;
            }
            "wq" | "x" => {
                if self.save() {
                    self.request_quit();
                }
            }
            "export" | "mix" => self.export_mix(rest),
            "stems" => self.export_stems(rest),
            "clip" | "bounce" => self.export_clip(rest),
            "import" | "load" => self.import(rest),
            "rename" | "name" if !rest.is_empty() => {
                self.project.tracks[t].name = rest.to_string();
                self.dirty = true;
            }
            "gain" | "vol" => match rest.trim_end_matches("dB").trim().parse::<f32>() {
                Ok(db) if db.is_finite() => {
                    self.project.tracks[t].gain_db = db.clamp(-60.0, 12.0);
                    self.dirty = true;
                    self.sync();
                }
                _ => self.error("usage: :gain <dB>"),
            },
            "clipgain" | "cgain" => match rest.trim_end_matches("dB").trim().parse::<f32>() {
                Ok(db) if db.is_finite() => self.set_clip_gain(db),
                _ => self.error("usage: :clipgain <dB>"),
            },
            "normalize" | "norm" => {
                let target = if rest.is_empty() { Some(-1.0) } else { rest.trim_end_matches("dB").trim().parse().ok() };
                match target {
                    Some(db) => self.normalize(db),
                    None => self.error("usage: :normalize [peak dBFS, default -1]"),
                }
            }
            "pan" => match parse_pan(rest) {
                Some(p) => {
                    self.project.tracks[t].pan = p;
                    self.dirty = true;
                    self.sync();
                }
                None => self.error("usage: :pan <L100..C..R100>"),
            },
            "input" | "in" => match Input::parse(rest) {
                Some(i) if self.recording.is_none() => {
                    self.project.tracks[t].input = i;
                    self.dirty = true;
                    if let Some(ch) = self.input_channels()
                        && !i.fits(ch)
                    {
                        self.warn(format!("input device only has {ch} channel(s)"));
                    }
                }
                Some(_) => self.warn("can't change inputs while recording"),
                None => self.error("usage: :input <n> (mono) or <n-m> (stereo pair), 1-based"),
            },
            "latency" => match rest.trim_end_matches("ms").trim().parse::<f32>() {
                Ok(ms) if (0.0..=2000.0).contains(&ms) => {
                    self.project.latency_ms = ms;
                    self.dirty = true;
                    self.info(format!("new takes shift {ms} ms earlier"));
                }
                _ => self.error("usage: :latency <ms>"),
            },
            "track" | "newtrack" => self.add_track((!rest.is_empty()).then(|| rest.to_string())),
            "deltrack" => self.delete_track(),
            "goto" | "g" => match parse_time(rest) {
                Some(s) => self.set_cursor(self.project.secs_to_frames(s)),
                None => self.error("usage: :goto <m:ss.sss | seconds>"),
            },
            "fadein" | "fadeout" => match parse_time(rest) {
                Some(s) => self.set_fade_secs(cmd == "fadein", s),
                None => self.error(format!("usage: :{cmd} <seconds>")),
            },
            "bits" => match rest.parse::<Bits>() {
                Ok(b) => {
                    self.bits = b;
                    self.info(format!("exports will be {b}"));
                }
                Err(e) => self.error(e),
            },
            "devices" | "dev" => {
                let out = self.engine.output.as_ref().map_or("none".into(), |o| o.info.to_string());
                let inp = self.engine.input.as_ref().map_or("none".into(), |i| i.info.to_string());
                self.info(format!("out: {out} · in: {inp}"));
            }
            "help" | "h" => self.mode = Mode::Help,
            other => self.error(format!("unknown command :{other} (? for help)")),
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        let pos = Position { x: m.column, y: m.row };
        let frame_at = |app: &App| app.view_start + m.column.saturating_sub(app.hits.lanes_x) as u64 * app.zoom;
        let n_tracks = self.project.tracks.len();
        // Hits come from the last draw; a track may have been deleted since.
        let hit = self
            .hits
            .tracks
            .iter()
            .copied()
            .find(|h| h.header.contains(pos) || h.lane.contains(pos))
            .filter(|h| h.track < n_tracks);
        let ctrl = m.modifiers.contains(KeyModifiers::CONTROL);
        let shift = m.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag = None; // a release we never saw must not replay
                self.ruler_anchor = None;
                if self.mode != Mode::Normal {
                    self.mode = Mode::Normal;
                    return;
                }
                if self.hits.play.contains(pos) {
                    if !self.is_playing() {
                        self.toggle_play();
                    }
                } else if self.hits.rec.contains(pos) {
                    self.toggle_record();
                } else if self.hits.stop.contains(pos) {
                    self.stop();
                } else if self.hits.ruler.contains(pos) && m.column >= self.hits.lanes_x {
                    let f = frame_at(self);
                    self.ruler_anchor = Some(f);
                    self.set_cursor(f);
                } else if let Some(h) = hit {
                    if h.track != self.sel_track {
                        self.sel_clip = None;
                    }
                    self.sel_track = h.track;
                    if h.mute.contains(pos) {
                        self.toggle_mute(h.track);
                    } else if h.solo.contains(pos) {
                        self.toggle_solo(h.track);
                    } else if h.arm.contains(pos) {
                        self.toggle_arm(h.track);
                    } else if h.input.contains(pos) {
                        self.cycle_input(h.track);
                    } else if h.lane.contains(pos) {
                        let f = frame_at(self);
                        self.sel_clip = self.project.tracks[h.track].clip_at(f);
                        if let Some(i) = self.sel_clip {
                            let c = &self.project.tracks[h.track].clips[i];
                            let start = c.start;
                            self.drag = Some(Drag {
                                from_track: h.track,
                                clip: i,
                                source: c.source.clone(),
                                start,
                                grab: f - start,
                                to_track: h.track,
                                to_start: start,
                                moved: false,
                            });
                        }
                        if self.recording.is_none() {
                            self.set_cursor(f);
                        }
                    } else {
                        self.sel_clip = self.project.tracks[h.track].clip_at(self.cursor);
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let f = frame_at(self);
                if let Some(anchor) = self.ruler_anchor {
                    self.markers = (Some(anchor.min(f)), Some(anchor.max(f)));
                } else if let Some(d) = &mut self.drag {
                    if let Some(h) = hit {
                        d.to_track = h.track;
                    }
                    d.to_start = f.saturating_sub(d.grab);
                    d.moved = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.ruler_anchor = None;
                if let Some(d) = self.drag.take()
                    && d.moved
                {
                    let same = self
                        .project
                        .tracks
                        .get(d.from_track)
                        .and_then(|t| t.clips.get(d.clip))
                        .is_some_and(|c| Arc::ptr_eq(&c.source, &d.source) && c.start == d.start);
                    if !same || d.to_track >= self.project.tracks.len() {
                        return; // the clip was edited mid-drag; drop the gesture
                    }
                    if d.to_track == d.from_track && d.to_start == d.start {
                        return;
                    }
                    self.checkpoint(None);
                    let mut clip = self.project.tracks[d.from_track].clips.remove(d.clip);
                    clip.start = d.to_start;
                    let idx = self.project.tracks[d.to_track].insert(clip);
                    self.sel_track = d.to_track;
                    self.sel_clip = Some(idx);
                    self.sync();
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                if let Some(h) = hit.filter(|h| h.header.contains(pos)) {
                    if h.gain.contains(pos) {
                        self.nudge_gain(h.track, if up { 0.5 } else { -0.5 });
                    } else if h.pan.contains(pos) {
                        self.nudge_pan(h.track, if up { 0.05 } else { -0.05 });
                    } else {
                        self.scroll_tracks(if up { -1 } else { 1 });
                    }
                } else if ctrl {
                    let anchor = frame_at(self);
                    self.zoom_by(up, anchor);
                } else if shift {
                    self.scroll_tracks(if up { -1 } else { 1 });
                } else {
                    let cols = (self.hits.lanes_w / 8).max(1) as i64;
                    self.scroll(if up { -cols } else { cols });
                }
            }
            MouseEventKind::ScrollLeft => self.scroll(-((self.hits.lanes_w / 8).max(1) as i64)),
            MouseEventKind::ScrollRight => self.scroll((self.hits.lanes_w / 8).max(1) as i64),
            _ => {}
        }
    }

    fn scroll_tracks(&mut self, delta: i64) {
        let max = self.project.tracks.len().saturating_sub(1) as i64;
        self.track_scroll = (self.track_scroll as i64 + delta).clamp(0, max) as usize;
    }

    pub fn status_hint(&self) -> Option<String> {
        if !self.project.tracks.iter().any(|t| !t.clips.is_empty()) && self.recording.is_none() {
            let dir = self.project.dir.display();
            return Some(match (&self.engine.input, Project::exists(&self.project.dir)) {
                (None, _) => "no input device — :import a file, or check `asciidaw devices`".into(),
                (Some(_), false) => format!("new project in {dir} — press r to record, ? for help"),
                (Some(_), true) => "press r to record on the selected track, ? for help".into(),
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::tests::src;

    fn app() -> App {
        let mut p = Project::new(std::env::temp_dir().join("asciidaw-app-test"), 1000);
        p.tracks[0].insert(Clip::new("a", src(1000, 0.5), 0));
        let mut app = App::new(p, Engine::offline());
        app.zoom = 10;
        app.hits.lanes_w = 100;
        app
    }

    #[test]
    fn time_parsing_and_formatting() {
        assert_eq!(parse_time("90"), Some(90.0));
        assert_eq!(parse_time("1:30"), Some(90.0));
        assert_eq!(parse_time("1:02:03.5"), Some(3723.5));
        assert_eq!(parse_time("2.5s"), Some(2.5));
        assert_eq!(parse_time("x"), None);
        assert_eq!(parse_time("-3"), None);
        assert_eq!(fmt_time(48000 * 83 + 24000, 48000), "1:23.500");
        assert_eq!(fmt_time(48000 * 3723, 48000), "1:02:03.000");
    }

    #[test]
    fn pan_parsing() {
        assert_eq!(parse_pan("C"), Some(0.0));
        assert_eq!(parse_pan("l50"), Some(-0.5));
        assert_eq!(parse_pan("R100"), Some(1.0));
        assert_eq!(parse_pan("-0.25"), Some(-0.25));
        assert_eq!(parse_pan("40"), Some(0.4));
        assert_eq!(parse_pan("R150"), None);
        assert_eq!(fmt_pan(-0.3), "L30");
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn split_cut_paste_undo() {
        let mut app = app();
        app.cursor = 400;
        app.on_key(key('b'));
        assert_eq!(app.project.tracks[0].clips.len(), 2);
        assert_eq!(app.sel_clip, Some(1));
        app.on_key(key('x'));
        assert_eq!(app.project.tracks[0].clips.len(), 1);
        app.sel_track = 1;
        app.cursor = 100;
        app.on_key(key('v'));
        let c = &app.project.tracks[1].clips[0];
        assert_eq!((c.start, c.len, c.offset), (100, 600, 400));
        assert_eq!(app.cursor, 700);
        app.on_key(key('u'));
        assert!(app.project.tracks[1].clips.is_empty());
        app.on_key(key('u'));
        app.on_key(key('u'));
        assert_eq!(app.project.tracks[0].clips.len(), 1);
        assert_eq!(app.project.tracks[0].clips[0].len, 1000);
        app.on_key(key('U'));
        assert_eq!(app.project.tracks[0].clips.len(), 2);
    }

    #[test]
    fn undo_keeps_mixer_state() {
        let mut app = app();
        app.cursor = 500;
        app.on_key(key('b'));
        app.on_key(key('m'));
        app.on_key(key('u'));
        assert_eq!(app.project.tracks[0].clips.len(), 1);
        assert!(app.project.tracks[0].mute, "undo must not un-mute");
    }

    #[test]
    fn nudge_stops_at_neighbours_and_coalesces() {
        let mut app = app();
        app.project.tracks[0].insert(Clip::new("b", src(100, 0.5), 1200));
        app.sel_clip = Some(1);
        for _ in 0..50 {
            app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        }
        assert_eq!(app.project.tracks[0].clips[1].start, 1000);
        assert_eq!(app.undo.len(), 1);
    }

    #[test]
    fn commands() {
        let mut app = app();
        app.run_command("gain -6");
        app.run_command("pan L25");
        app.run_command("input 1-2");
        app.run_command("rename Vox");
        let t = &app.project.tracks[0];
        assert_eq!((t.gain_db, t.pan, t.input.stereo, t.name.as_str()), (-6.0, -0.25, true, "Vox"));
        app.sel_clip = Some(0);
        app.run_command("normalize -6");
        assert!((app.project.tracks[0].clips[0].peak() - 0.5012).abs() < 1e-3);
        app.run_command("fadein 0.1");
        assert_eq!(app.project.tracks[0].clips[0].fade_in, 100);
        app.run_command("goto 0:00.5");
        assert_eq!(app.cursor, 500);
        app.run_command("bogus");
        assert_eq!(app.status.as_ref().unwrap().kind, StatusKind::Error);
    }

    fn click(app: &mut App, kind: MouseEventKind, x: u16, y: u16) {
        app.on_mouse(MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE });
    }

    /// Two tracks laid out like the UI would: headers 0..26, lanes from 26.
    fn with_hits(app: &mut App) {
        app.hits.lanes_x = 26;
        app.hits.tracks = (0..app.project.tracks.len())
            .map(|i| {
                let y = i as u16 * 4;
                TrackHit {
                    track: i,
                    header: Rect::new(0, y, 26, 4),
                    lane: Rect::new(26, y, 100, 4),
                    mute: Rect::new(1, y + 1, 3, 1),
                    solo: Rect::new(5, y + 1, 3, 1),
                    arm: Rect::new(9, y + 1, 3, 1),
                    input: Rect::new(14, y + 1, 6, 1),
                    gain: Rect::new(1, y + 2, 8, 1),
                    pan: Rect::new(12, y + 2, 7, 1),
                }
            })
            .collect();
    }

    #[test]
    fn clicking_another_tracks_button_drops_the_clip_selection() {
        let mut app = app();
        with_hits(&mut app);
        app.sel_clip = Some(0); // clip on track 1
        click(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 5); // arm on track 2
        assert!(app.project.tracks[1].armed);
        assert_eq!((app.sel_track, app.sel_clip), (1, None));
        app.on_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)); // used to panic
        app.sel_clip = Some(5); // stale by any other route
        app.on_key(key('x'));
        assert_eq!(app.project.tracks[0].clips.len(), 1);
    }

    #[test]
    fn editing_mid_drag_cancels_the_drag() {
        let mut app = app();
        with_hits(&mut app);
        click(&mut app, MouseEventKind::Down(MouseButton::Left), 30, 1);
        click(&mut app, MouseEventKind::Drag(MouseButton::Left), 50, 5);
        assert!(app.drag.as_ref().is_some_and(|d| d.moved && d.to_track == 1));
        app.on_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        click(&mut app, MouseEventKind::Up(MouseButton::Left), 50, 5); // used to panic
        assert!(app.project.tracks.iter().all(|t| t.clips.is_empty()));
    }

    #[test]
    fn a_missed_release_does_not_replay() {
        let mut app = app();
        with_hits(&mut app);
        click(&mut app, MouseEventKind::Down(MouseButton::Left), 30, 1);
        click(&mut app, MouseEventKind::Drag(MouseButton::Left), 60, 1);
        // Release happened outside the terminal; next gesture starts on empty track 2.
        click(&mut app, MouseEventKind::Down(MouseButton::Left), 40, 5);
        click(&mut app, MouseEventKind::Drag(MouseButton::Left), 70, 5);
        click(&mut app, MouseEventKind::Up(MouseButton::Left), 70, 5);
        assert_eq!(app.project.tracks[0].clips[0].start, 0);
        assert!(app.project.tracks[1].clips.is_empty());
    }

    #[test]
    fn drag_moves_clip_between_tracks() {
        let mut app = app();
        with_hits(&mut app);
        click(&mut app, MouseEventKind::Down(MouseButton::Left), 30, 1); // frame 40
        click(&mut app, MouseEventKind::Drag(MouseButton::Left), 40, 5); // frame 140
        click(&mut app, MouseEventKind::Up(MouseButton::Left), 40, 5);
        assert!(app.project.tracks[0].clips.is_empty());
        assert_eq!(app.project.tracks[1].clips[0].start, 100);
        assert_eq!((app.sel_track, app.sel_clip), (1, Some(0)));
    }

    #[test]
    fn stale_hits_for_a_deleted_track_are_ignored() {
        let mut app = app();
        with_hits(&mut app);
        app.sel_track = 1;
        app.run_command("deltrack");
        click(&mut app, MouseEventKind::Down(MouseButton::Left), 2, 5); // used to panic
        click(&mut app, MouseEventKind::ScrollUp, 2, 6);
        assert_eq!(app.project.tracks.len(), 1);
    }

    #[test]
    fn clip_gain_on_two_tracks_is_two_undo_steps() {
        let mut app = app();
        app.project.tracks[1].insert(Clip::new("b", src(100, 0.5), 0));
        app.sel_clip = Some(0);
        app.run_command("clipgain -3");
        app.sel_track = 1;
        app.sel_clip = Some(0);
        app.run_command("clipgain -6");
        app.undo();
        assert_eq!(app.project.tracks[0].clips[0].gain_db, -3.0);
        assert_eq!(app.project.tracks[1].clips[0].gain_db, 0.0);
    }

    fn take(track: usize, writer: Option<WavOut>) -> Take {
        Take {
            track,
            input: Input { channel: track as u16, stereo: false },
            file: format!("t{track}.wav"),
            writer,
            failed: false,
            data: vec![Vec::new()],
            peaks: Peaks::default(),
            scratch: Vec::new(),
        }
    }

    #[test]
    fn dropped_input_becomes_silence_in_the_take() {
        let mut rec = Recording {
            start: 0,
            rate: 48000,
            takes: vec![take(0, None)],
            last_flush: Instant::now(),
            overruns_seen: 0,
        };
        rec.append(&[0.5, 0.1, 0.5, 0.1], 2, 3).unwrap();
        rec.append(&[0.25, 0.1], 2, 0).unwrap();
        assert_eq!(rec.takes[0].data[0], vec![0.5, 0.5, 0.0, 0.0, 0.0, 0.25]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn one_failing_file_does_not_stop_the_other_take() {
        let full = WavOut::create(Path::new("/dev/full"), 48000, 1, Bits::F32).unwrap();
        let mut rec = Recording {
            start: 0,
            rate: 48000,
            takes: vec![take(0, Some(full)), take(1, None)],
            last_flush: Instant::now(),
            overruns_seen: 0,
        };
        let buf = vec![0.1f32; 2 * 20_000]; // enough to overflow the write buffer
        let err = rec.append(&buf, 2, 0).unwrap_err();
        assert!(err.contains("t0.wav"), "{err}");
        assert!(rec.takes[0].failed && rec.takes[0].writer.is_none());
        assert_eq!(rec.takes[0].data[0].len(), 20_000);
        assert_eq!(rec.takes[1].data[0].len(), 20_000);
        rec.append(&buf, 2, 0).unwrap(); // keeps going in memory
        assert_eq!(rec.takes[0].data[0].len(), 40_000);
    }

    #[test]
    fn quit_asks_when_dirty() {
        let mut app = app();
        app.on_key(key('q'));
        assert!(app.quit);
        let mut app = self::app();
        app.cursor = 300;
        app.on_key(key('b'));
        app.on_key(key('q'));
        assert!(!app.quit);
        assert_eq!(app.mode, Mode::ConfirmQuit);
        app.on_key(key('n'));
        assert_eq!(app.mode, Mode::Normal);
        app.on_key(key('q'));
        app.on_key(key('q'));
        assert!(app.quit);
    }
}
