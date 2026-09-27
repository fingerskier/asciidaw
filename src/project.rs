//! The session model: tracks of non-overlapping clips referencing shared
//! audio sources, persisted as `project.json` + `audio/*.wav` in a directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::audio::{AudioData, db_to_gain, resample};
use crate::wav::{self, Bits};

pub const PROJECT_FILE: &str = "project.json";
pub const AUDIO_DIR: &str = "audio";
const FORMAT_VERSION: u32 = 1;

/// A window onto a source, placed on the timeline. All positions in frames.
#[derive(Debug, Clone)]
pub struct Clip {
    pub name: String,
    pub source: Arc<AudioData>,
    /// Timeline position of the first frame.
    pub start: u64,
    /// First frame used from the source.
    pub offset: u64,
    pub len: u64,
    pub gain_db: f32,
    pub fade_in: u64,
    pub fade_out: u64,
}

impl Clip {
    pub fn new(name: impl Into<String>, source: Arc<AudioData>, start: u64) -> Clip {
        let len = source.frames() as u64;
        Clip { name: name.into(), source, start, offset: 0, len, gain_db: 0.0, fade_in: 0, fade_out: 0 }
    }

    pub fn end(&self) -> u64 {
        self.start + self.len
    }

    pub fn contains(&self, t: u64) -> bool {
        self.start <= t && t < self.end()
    }

    /// Fade envelope (0..=1) at frame `i` within the clip; excludes clip gain.
    pub fn fade(&self, i: u64) -> f32 {
        let mut g = 1.0;
        if i < self.fade_in {
            g *= fade_curve(i as f32 / self.fade_in as f32);
        }
        let from_end = self.len.saturating_sub(i + 1);
        if from_end < self.fade_out {
            g *= fade_curve(from_end as f32 / self.fade_out as f32);
        }
        g
    }

    pub fn gain(&self) -> f32 {
        db_to_gain(self.gain_db)
    }

    /// The part of this clip that falls inside timeline range `[from, to)`.
    /// Fades survive only on edges that weren't cut.
    pub fn trimmed(&self, from: u64, to: u64) -> Option<Clip> {
        let s = self.start.max(from);
        let e = self.end().min(to);
        if s >= e {
            return None;
        }
        let mut c = self.clone();
        c.offset += s - self.start;
        c.start = s;
        c.len = e - s;
        c.fade_in = if s == self.start { self.fade_in.min(c.len) } else { 0 };
        c.fade_out = if e == self.end() { self.fade_out.min(c.len) } else { 0 };
        Some(c)
    }

    /// Split at timeline frame `at` (strictly inside the clip).
    pub fn split(&self, at: u64) -> Option<(Clip, Clip)> {
        if at <= self.start || at >= self.end() {
            return None;
        }
        Some((self.trimmed(self.start, at)?, self.trimmed(at, self.end())?))
    }

    /// Peak level of the clip's audio (after clip gain, before fades).
    pub fn peak(&self) -> f32 {
        self.source.peak(self.offset as usize, (self.offset + self.len) as usize) * self.gain()
    }
}

/// Quarter-sine: smoother than linear, no click at either end.
fn fade_curve(x: f32) -> f32 {
    (x.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2).sin()
}

/// Which device input channel(s) a track records from (0-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    pub channel: u16,
    pub stereo: bool,
}

/// No real interface has more; keeps channel arithmetic far from overflow.
const MAX_INPUT_CHANNEL: u16 = 1024;

impl Input {
    pub fn channels(&self) -> u16 {
        if self.stereo { 2 } else { 1 }
    }

    /// Whether a device with `device_channels` inputs can feed this.
    pub fn fits(&self, device_channels: u16) -> bool {
        self.channel as u32 + self.channels() as u32 <= device_channels as u32
    }

    /// Parse user syntax: "1" (mono, 1-based) or "1-2" / "1+2" (stereo pair).
    pub fn parse(s: &str) -> Option<Input> {
        let s = s.trim();
        let num = |t: &str| t.trim().parse::<u16>().ok().filter(|n| (1..=MAX_INPUT_CHANNEL).contains(n));
        if let Some((a, b)) = s.split_once(['-', '+']) {
            let (a, b) = (num(a)?, num(b)?);
            (b == a + 1).then(|| Input { channel: a - 1, stereo: true })
        } else {
            num(s).map(|a| Input { channel: a - 1, stereo: false })
        }
    }
}

impl std::fmt::Display for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.stereo {
            write!(f, "{}-{}", self.channel + 1, self.channel + 2)
        } else {
            write!(f, "{}", self.channel + 1)
        }
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    pub name: String,
    /// Sorted by start; never overlapping.
    pub clips: Vec<Clip>,
    pub gain_db: f32,
    /// -1 (left) ..= 1 (right).
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub armed: bool,
    pub input: Input,
}

impl Track {
    pub fn new(name: impl Into<String>, input: Input) -> Track {
        Track {
            name: name.into(),
            clips: Vec::new(),
            gain_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
            armed: false,
            input,
        }
    }

    pub fn end(&self) -> u64 {
        self.clips.iter().map(Clip::end).max().unwrap_or(0)
    }

    pub fn clip_at(&self, t: u64) -> Option<usize> {
        self.clips.iter().position(|c| c.contains(t))
    }

    /// Remove all material in `[from, to)`, trimming or splitting clips.
    pub fn clear_range(&mut self, from: u64, to: u64) {
        let mut out = Vec::with_capacity(self.clips.len() + 1);
        for c in self.clips.drain(..) {
            if c.end() <= from || c.start >= to {
                out.push(c);
                continue;
            }
            out.extend(c.trimmed(c.start, from));
            out.extend(c.trimmed(to, c.end()));
        }
        self.clips = out;
    }

    /// Insert with overwrite semantics; returns the new clip's index.
    pub fn insert(&mut self, clip: Clip) -> usize {
        self.clear_range(clip.start, clip.end());
        let idx = self.clips.partition_point(|c| c.start < clip.start);
        self.clips.insert(idx, clip);
        idx
    }

    /// Free timeline span a clip at `idx` may move within without overlapping
    /// its neighbours: (earliest start, latest end).
    pub fn free_span(&self, idx: usize) -> (u64, u64) {
        let lo = if idx > 0 { self.clips[idx - 1].end() } else { 0 };
        let hi = self.clips.get(idx + 1).map_or(u64::MAX, |c| c.start);
        (lo, hi)
    }

    /// Mixer-only state (not covered by undo).
    pub fn copy_mixer_from(&mut self, other: &Track) {
        self.name.clone_from(&other.name);
        self.gain_db = other.gain_db;
        self.pan = other.pan;
        self.mute = other.mute;
        self.solo = other.solo;
        self.armed = other.armed;
        self.input = other.input;
    }

    /// Channel count of this track's material: 2 if any clip is stereo.
    pub fn channels(&self) -> usize {
        if self.clips.iter().any(|c| c.source.num_channels() > 1) { 2 } else { 1 }
    }
}

#[derive(Debug, Clone)]
pub struct Project {
    pub dir: PathBuf,
    pub rate: u32,
    /// Recorded takes are shifted this much earlier to compensate for
    /// round-trip device latency. Calibrate once per interface.
    pub latency_ms: f32,
    pub tracks: Vec<Track>,
}

#[derive(Serialize, Deserialize)]
struct ProjectFile {
    format: u32,
    sample_rate: u32,
    #[serde(default)]
    latency_ms: f32,
    tracks: Vec<TrackFile>,
}

#[derive(Serialize, Deserialize)]
struct TrackFile {
    name: String,
    #[serde(default)]
    gain_db: f32,
    #[serde(default)]
    pan: f32,
    #[serde(default)]
    mute: bool,
    #[serde(default)]
    solo: bool,
    input: Input,
    clips: Vec<ClipFile>,
}

#[derive(Serialize, Deserialize)]
struct ClipFile {
    name: String,
    /// File name inside `audio/`.
    source: String,
    start: u64,
    offset: u64,
    len: u64,
    #[serde(default)]
    gain_db: f32,
    #[serde(default)]
    fade_in: u64,
    #[serde(default)]
    fade_out: u64,
}

impl Project {
    /// A fresh session: two mono tracks on inputs 1 and 2. Nothing is written
    /// to disk until the first save, record or import.
    pub fn new(dir: impl Into<PathBuf>, rate: u32) -> Project {
        Project {
            dir: dir.into(),
            rate,
            latency_ms: 0.0,
            tracks: vec![
                Track::new("Track 1", Input { channel: 0, stereo: false }),
                Track::new("Track 2", Input { channel: 1, stereo: false }),
            ],
        }
    }

    pub fn exists(dir: &Path) -> bool {
        dir.join(PROJECT_FILE).is_file()
    }

    pub fn name(&self) -> String {
        let abs = std::path::absolute(&self.dir).unwrap_or_else(|_| self.dir.clone());
        abs.file_name().map_or_else(|| "untitled".into(), |n| n.to_string_lossy().into_owned())
    }

    pub fn audio_dir(&self) -> PathBuf {
        self.dir.join(AUDIO_DIR)
    }

    pub fn end(&self) -> u64 {
        self.tracks.iter().map(Track::end).max().unwrap_or(0)
    }

    pub fn secs_to_frames(&self, secs: f64) -> u64 {
        (secs.max(0.0) * self.rate as f64).round() as u64
    }

    pub fn frames_to_secs(&self, frames: u64) -> f64 {
        frames as f64 / self.rate as f64
    }

    pub fn latency_frames(&self) -> u64 {
        self.secs_to_frames(self.latency_ms as f64 / 1000.0)
    }

    /// Load a project; returns warnings for anything skipped (e.g. missing audio).
    pub fn load(dir: &Path) -> Result<(Project, Vec<String>)> {
        let path = dir.join(PROJECT_FILE);
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let file: ProjectFile = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if file.format > FORMAT_VERSION {
            bail!("{} is format {}, this asciidaw understands {FORMAT_VERSION}", path.display(), file.format);
        }
        if file.sample_rate == 0 {
            bail!("{} has sample_rate 0", path.display());
        }
        let mut project =
            Project { dir: dir.to_path_buf(), rate: file.sample_rate, latency_ms: file.latency_ms, tracks: vec![] };
        let mut sources: HashMap<String, Option<Arc<AudioData>>> = HashMap::new();
        let mut warnings = Vec::new();
        for tf in file.tracks {
            let mut track = Track::new(tf.name, tf.input);
            track.gain_db = tf.gain_db;
            track.pan = tf.pan.clamp(-1.0, 1.0);
            track.mute = tf.mute;
            track.solo = tf.solo;
            for cf in tf.clips {
                let src = sources
                    .entry(cf.source.clone())
                    .or_insert_with(|| match project.load_source(&cf.source) {
                        Ok((src, note)) => {
                            warnings.extend(note);
                            Some(src)
                        }
                        Err(e) => {
                            warnings.push(format!("{e:#}"));
                            None
                        }
                    })
                    .clone();
                let Some(source) = src else { continue };
                let avail = (source.frames() as u64).saturating_sub(cf.offset);
                let len = cf.len.min(avail);
                if len == 0 {
                    warnings.push(format!("clip {:?} points past the end of {}", cf.name, cf.source));
                    continue;
                }
                track.clips.push(Clip {
                    name: cf.name,
                    source,
                    start: cf.start,
                    offset: cf.offset,
                    len,
                    gain_db: cf.gain_db,
                    fade_in: cf.fade_in.min(len),
                    fade_out: cf.fade_out.min(len),
                });
            }
            // Re-establish the no-overlap invariant in case the file was hand-edited.
            let clips = std::mem::take(&mut track.clips);
            for c in clips {
                track.insert(c);
            }
            project.tracks.push(track);
        }
        Ok((project, warnings))
    }

    fn load_source(&self, file: &str) -> Result<(Arc<AudioData>, Option<String>)> {
        let path = self.audio_dir().join(file);
        let decoded = wav::read_wav(&path)?;
        let mut note = None;
        let mut channels = decoded.channels;
        channels.truncate(2);
        if decoded.rate != self.rate {
            note = Some(format!("{file} is {} Hz; resampled to {} Hz in memory", decoded.rate, self.rate));
            channels = channels.iter().map(|c| resample(c, decoded.rate, self.rate)).collect();
        }
        Ok((Arc::new(AudioData::new(file, channels)), note))
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let file = ProjectFile {
            format: FORMAT_VERSION,
            sample_rate: self.rate,
            latency_ms: self.latency_ms,
            tracks: self
                .tracks
                .iter()
                .map(|t| TrackFile {
                    name: t.name.clone(),
                    gain_db: t.gain_db,
                    pan: t.pan,
                    mute: t.mute,
                    solo: t.solo,
                    input: t.input,
                    clips: t
                        .clips
                        .iter()
                        .map(|c| ClipFile {
                            name: c.name.clone(),
                            source: c.source.file.clone(),
                            start: c.start,
                            offset: c.offset,
                            len: c.len,
                            gain_db: c.gain_db,
                            fade_in: c.fade_in,
                            fade_out: c.fade_out,
                        })
                        .collect(),
                })
                .collect(),
        };
        let path = self.dir.join(PROJECT_FILE);
        let tmp = self.dir.join(format!("{PROJECT_FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_string_pretty(&file)? + "\n")?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// A not-yet-used `audio/<stem>-NNN.wav` file name.
    pub fn fresh_audio_name(&self, stem: &str) -> String {
        let stem: String = stem
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
            .collect();
        let stem = stem.trim_matches('-');
        let stem = if stem.is_empty() { "audio" } else { stem };
        let dir = self.audio_dir();
        (1..).map(|n| format!("{stem}-{n:03}.wav")).find(|name| !dir.join(name).exists()).expect("unbounded search")
    }

    /// Bring an external file into `audio/` (at the project rate, at most
    /// stereo) and return it as a source ready to place in a clip.
    pub fn import(&self, path: &Path) -> Result<Arc<AudioData>> {
        std::fs::create_dir_all(self.audio_dir())?;
        let stem = path.file_stem().map_or("import".into(), |s| s.to_string_lossy().into_owned());
        let name = self.fresh_audio_name(&stem);
        let dest = self.audio_dir().join(&name);
        let decoded = wav::read_any(path, &self.audio_dir().join(".import-tmp.wav"))?;
        if decoded.frames() == 0 {
            bail!("{} contains no audio", path.display());
        }
        let mut channels = decoded.channels;
        let untouched = wav::is_wav(path) && decoded.rate == self.rate && channels.len() <= 2;
        channels.truncate(2);
        if decoded.rate != self.rate {
            channels = channels.iter().map(|c| resample(c, decoded.rate, self.rate)).collect();
        }
        if untouched {
            std::fs::copy(path, &dest).with_context(|| format!("copying into {}", dest.display()))?;
        } else {
            wav::write_wav(&dest, self.rate, &channels, Bits::F32)?;
        }
        Ok(Arc::new(AudioData::new(name, channels)))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn src(frames: usize, value: f32) -> Arc<AudioData> {
        Arc::new(AudioData::new("t.wav", vec![vec![value; frames]]))
    }

    fn spans(t: &Track) -> Vec<(u64, u64, u64)> {
        t.clips.iter().map(|c| (c.start, c.end(), c.offset)).collect()
    }

    #[test]
    fn split_keeps_outer_fades() {
        let mut c = Clip::new("a", src(1000, 0.5), 100);
        c.fade_in = 50;
        c.fade_out = 60;
        let (l, r) = c.split(400).unwrap();
        assert_eq!((l.start, l.len, l.offset, l.fade_in, l.fade_out), (100, 300, 0, 50, 0));
        assert_eq!((r.start, r.len, r.offset, r.fade_in, r.fade_out), (400, 700, 300, 0, 60));
        assert!(c.split(100).is_none());
        assert!(c.split(1100).is_none());
    }

    #[test]
    fn insert_overwrites_and_splits() {
        let mut t = Track::new("t", Input { channel: 0, stereo: false });
        t.insert(Clip::new("a", src(1000, 0.1), 0));
        t.insert(Clip::new("b", src(100, 0.2), 400));
        assert_eq!(spans(&t), vec![(0, 400, 0), (400, 500, 0), (500, 1000, 500)]);
        // Swallow the middle entirely and bite both neighbours.
        t.insert(Clip::new("c", src(200, 0.3), 350));
        assert_eq!(spans(&t), vec![(0, 350, 0), (350, 550, 0), (550, 1000, 550)]);
        assert_eq!(t.clip_at(360), Some(1));
        assert_eq!(t.free_span(1), (350, 550));
        assert_eq!(t.free_span(0), (0, 350));
        assert_eq!(t.free_span(2), (550, u64::MAX));
    }

    #[test]
    fn fades_shape_envelope() {
        let mut c = Clip::new("a", src(100, 1.0), 0);
        c.fade_in = 10;
        c.fade_out = 10;
        assert_eq!(c.fade(0), 0.0);
        assert!((c.fade(50) - 1.0).abs() < 1e-6);
        assert!(c.fade(99) < 1e-6);
        assert!(c.fade(5) > 0.0 && c.fade(5) < 1.0);
    }

    #[test]
    fn input_parse_and_display() {
        assert_eq!(Input::parse("1"), Some(Input { channel: 0, stereo: false }));
        assert_eq!(Input::parse("3-4"), Some(Input { channel: 2, stereo: true }));
        assert_eq!(Input::parse("1+2").unwrap().to_string(), "1-2");
        assert_eq!(Input::parse("2-4"), None);
        assert_eq!(Input::parse("0"), None);
        assert_eq!(Input::parse("65535-1"), None);
        assert_eq!(Input::parse("65535-0"), None);
        assert!(!Input { channel: u16::MAX, stereo: true }.fits(u16::MAX));
        assert!(Input { channel: 1, stereo: false }.fits(2));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("asciidaw-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new(&dir, 48000);
        std::fs::create_dir_all(p.audio_dir()).unwrap();
        let data: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.1).sin() * 0.5).collect();
        let name = p.fresh_audio_name("Take One!");
        assert_eq!(name, "take-one-001.wav");
        wav::write_wav(&p.audio_dir().join(&name), 48000, std::slice::from_ref(&data), Bits::F32).unwrap();
        let source = Arc::new(AudioData::new(name, vec![data]));
        let mut clip = Clip::new("take", source, 1000);
        clip.gain_db = -3.0;
        clip.fade_in = 100;
        p.tracks[0].insert(clip);
        p.tracks[1].pan = -0.5;
        p.tracks[1].mute = true;
        p.latency_ms = 12.5;
        p.save().unwrap();

        let (q, warnings) = Project::load(&dir).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(q.rate, 48000);
        assert_eq!(q.latency_ms, 12.5);
        assert_eq!(q.tracks.len(), 2);
        let c = &q.tracks[0].clips[0];
        assert_eq!((c.start, c.len, c.gain_db, c.fade_in), (1000, 4800, -3.0, 100));
        assert_eq!(q.tracks[1].pan, -0.5);
        assert!(q.tracks[1].mute);
        assert_eq!(p.fresh_audio_name("take one"), "take-one-002.wav");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_audio_is_a_warning_not_an_error() {
        let dir = std::env::temp_dir().join(format!("asciidaw-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new(&dir, 44100);
        p.tracks[0].insert(Clip::new("ghost", Arc::new(AudioData::new("gone.wav", vec![vec![0.0; 10]])), 0));
        p.save().unwrap();
        let (q, warnings) = Project::load(&dir).unwrap();
        assert!(q.tracks[0].clips.is_empty());
        assert_eq!(warnings.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
