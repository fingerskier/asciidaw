//! One mixer for both the realtime callback and offline export.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::audio::db_to_gain;
use crate::project::{Clip, Project, Track};
use crate::wav::{Bits, WavOut};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// All audible tracks (mute/solo respected).
    Mix,
    /// One track regardless of mute/solo.
    Only(usize),
}

#[derive(Debug, Clone, Copy)]
pub struct RenderOpts {
    pub which: Which,
    /// Output channels: 1 or 2.
    pub channels: usize,
    /// Apply track pan (only meaningful for stereo output).
    pub pan: bool,
}

impl RenderOpts {
    pub const STEREO_MIX: RenderOpts = RenderOpts { which: Which::Mix, channels: 2, pan: true };
}

/// Constant-power pan normalised to unity at centre ("0 dB" pan law), so a
/// mono take exported dead-centre comes out at exactly its recorded level.
fn pan_gains(pan: f32) -> (f32, f32) {
    let theta = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    let k = std::f32::consts::SQRT_2;
    (theta.cos() * k, theta.sin() * k)
}

/// Stereo sources get balance, not pan: attenuate the far side.
fn balance_gains(pan: f32) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    ((1.0 - p).min(1.0), (1.0 + p).min(1.0))
}

/// Mix `tracks` into interleaved `out` starting at timeline frame `start`.
/// Per-track output peaks are max'ed into `track_peaks` (may be shorter than
/// the track list, or empty).
pub fn render(tracks: &[Track], start: u64, out: &mut [f32], opts: &RenderOpts, track_peaks: &mut [f32]) {
    out.fill(0.0);
    let ch = opts.channels;
    let frames = (out.len() / ch) as u64;
    let end = start + frames;
    let any_solo = opts.which == Which::Mix && tracks.iter().any(|t| t.solo);
    for (ti, track) in tracks.iter().enumerate() {
        let audible = match opts.which {
            Which::Mix => !track.mute && (!any_solo || track.solo),
            Which::Only(i) => i == ti,
        };
        if !audible {
            continue;
        }
        let tg = db_to_gain(track.gain_db);
        let (pl, pr) = if opts.pan { pan_gains(track.pan) } else { (1.0, 1.0) };
        let (bl, br) = if opts.pan { balance_gains(track.pan) } else { (1.0, 1.0) };
        let mut peak = 0f32;
        let first = track.clips.partition_point(|c| c.end() <= start);
        for clip in track.clips[first..].iter().take_while(|c| c.start < end) {
            let from = clip.start.max(start);
            let to = clip.end().min(end);
            let g = clip.gain() * tg;
            let src = &clip.source;
            let stereo = src.num_channels() > 1;
            let (c0, c1) = (&src.channels[0], &src.channels[if stereo { 1 } else { 0 }]);
            for t in from..to {
                let i = t - clip.start;
                let si = (clip.offset + i) as usize;
                if si >= c0.len() {
                    break;
                }
                let gi = g * clip.fade(i);
                let o = (t - start) as usize * ch;
                if ch == 1 {
                    let s = if stereo { 0.5 * (c0[si] + c1[si]) } else { c0[si] } * gi;
                    out[o] += s;
                    peak = peak.max(s.abs());
                } else {
                    let (l, r) = if stereo {
                        (c0[si] * bl * gi, c1[si] * br * gi)
                    } else {
                        let s = c0[si] * gi;
                        (s * pl, s * pr)
                    };
                    out[o] += l;
                    out[o + 1] += r;
                    peak = peak.max(l.abs()).max(r.abs());
                }
            }
        }
        if let Some(p) = track_peaks.get_mut(ti) {
            *p = p.max(peak);
        }
    }
}

#[derive(Debug)]
pub struct Rendered {
    pub path: PathBuf,
    pub frames: u64,
    pub peak: f32,
}

impl Rendered {
    /// Integer formats clip above full scale; float keeps the overs.
    pub fn clipped(&self, bits: Bits) -> bool {
        bits != Bits::F32 && self.peak > 1.0
    }
}

/// Render `[from, to)` of `tracks` to a WAV file.
pub fn render_to_file(
    tracks: &[Track],
    rate: u32,
    from: u64,
    to: u64,
    opts: RenderOpts,
    path: &Path,
    bits: Bits,
) -> Result<Rendered> {
    if to <= from {
        bail!("nothing to render: empty range (check the in/out markers)");
    }
    const CHUNK: usize = 8192;
    let mut out = WavOut::create(path, rate, opts.channels as u16, bits)?;
    let mut buf = vec![0f32; CHUNK * opts.channels];
    let mut peak = 0f32;
    let mut pos = from;
    while pos < to {
        let n = CHUNK.min((to - pos) as usize);
        let slice = &mut buf[..n * opts.channels];
        render(tracks, pos, slice, &opts, &mut []);
        peak = slice.iter().fold(peak, |m, s| m.max(s.abs()));
        out.write_interleaved(slice)?;
        pos += n as u64;
    }
    out.finalize()?;
    Ok(Rendered { path: path.to_path_buf(), frames: to - from, peak })
}

/// Default export range: the in/out markers if set, else frame 0 to the end
/// of the last clip.
pub fn export_range(project: &Project, markers: (Option<u64>, Option<u64>)) -> (u64, u64) {
    match markers {
        // Equal markers give an empty range, which export reports, rather
        // than silently falling back to the whole project.
        (Some(a), Some(b)) => (a.min(b), a.max(b)),
        (Some(a), None) => (a, project.end()),
        (None, Some(b)) => (0, b),
        _ => (0, project.end()),
    }
}

pub fn export_mix(project: &Project, path: &Path, range: (u64, u64), mono: bool, bits: Bits) -> Result<Rendered> {
    let opts = if mono { RenderOpts { which: Which::Mix, channels: 1, pan: false } } else { RenderOpts::STEREO_MIX };
    render_to_file(&project.tracks, project.rate, range.0, range.1, opts, path, bits)
}

/// One file per non-empty track, at the track's native channel count,
/// post-fader but unpanned. Mute/solo ignored.
pub fn export_stems(project: &Project, dir: &Path, range: (u64, u64), bits: Bits) -> Result<Vec<Rendered>> {
    let mut done = Vec::new();
    for (i, track) in project.tracks.iter().enumerate() {
        if !track.clips.iter().any(|c| c.end() > range.0 && c.start < range.1) {
            continue;
        }
        let slug: String =
            track.name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        let path = dir.join(format!("{:02}-{slug}.wav", i + 1));
        let opts = RenderOpts { which: Which::Only(i), channels: track.channels(), pan: false };
        done.push(render_to_file(&project.tracks, project.rate, range.0, range.1, opts, &path, bits)?);
    }
    if done.is_empty() {
        bail!("no track has audio in the export range");
    }
    Ok(done)
}

/// A single clip (clip gain and fades applied, nothing else).
pub fn export_clip(clip: &Clip, rate: u32, path: &Path, bits: Bits) -> Result<Rendered> {
    let mut solo = clip.clone();
    solo.start = 0;
    let mut track = Track::new("clip", crate::project::Input { channel: 0, stereo: false });
    let channels = solo.source.num_channels().min(2);
    let len = solo.len;
    track.clips.push(solo);
    let opts = RenderOpts { which: Which::Only(0), channels, pan: false };
    render_to_file(std::slice::from_ref(&track), rate, 0, len, opts, path, bits)
}

/// A snapshot the audio thread can own.
pub type Snapshot = Arc<Vec<Track>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::tests::src;
    use crate::project::{Input, Track};

    fn track_with(value: f32, frames: usize, start: u64) -> Track {
        let mut t = Track::new("t", Input { channel: 0, stereo: false });
        t.insert(Clip::new("c", src(frames, value), start));
        t
    }

    #[test]
    fn centre_pan_is_unity() {
        let tracks = vec![track_with(0.5, 100, 0)];
        let mut out = vec![0f32; 20];
        render(&tracks, 0, &mut out, &RenderOpts::STEREO_MIX, &mut []);
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-6), "{out:?}");
    }

    #[test]
    fn hard_pan_and_offsets() {
        let mut t = track_with(0.5, 10, 5);
        t.pan = -1.0;
        let mut out = vec![0f32; 40]; // 20 stereo frames
        let mut peaks = [0f32; 1];
        render(&[t], 0, &mut out, &RenderOpts::STEREO_MIX, &mut peaks);
        for f in 0..20 {
            let (l, r) = (out[2 * f], out[2 * f + 1]);
            if (5..15).contains(&f) {
                assert!((l - 0.5 * std::f32::consts::SQRT_2).abs() < 1e-5 && r.abs() < 1e-6, "frame {f}: {l} {r}");
            } else {
                assert_eq!((l, r), (0.0, 0.0), "frame {f}");
            }
        }
        assert!((peaks[0] - 0.707).abs() < 1e-3);
    }

    #[test]
    fn mute_solo_and_gain() {
        let a = track_with(0.25, 10, 0);
        let mut b = track_with(0.5, 10, 0);
        b.gain_db = -6.0206;
        let mut out = vec![0f32; 10];
        let mono = RenderOpts { which: Which::Mix, channels: 1, pan: false };
        render(&[a.clone(), b.clone()], 0, &mut out, &mono, &mut []);
        assert!((out[0] - 0.5).abs() < 1e-4);
        let mut muted = a.clone();
        muted.mute = true;
        render(&[muted, b.clone()], 0, &mut out, &mono, &mut []);
        assert!((out[0] - 0.25).abs() < 1e-4);
        let mut soloed = a.clone();
        soloed.solo = true;
        render(&[soloed, b.clone()], 0, &mut out, &mono, &mut []);
        assert!((out[0] - 0.25).abs() < 1e-4);
        // Stems ignore mute.
        let mut muted = a;
        muted.mute = true;
        let only = RenderOpts { which: Which::Only(0), channels: 1, pan: false };
        render(&[muted, b], 0, &mut out, &only, &mut []);
        assert!((out[0] - 0.25).abs() < 1e-4);
    }

    #[test]
    fn chunked_render_equals_one_shot() {
        let mut t = track_with(0.3, 1000, 37);
        t.clips[0].fade_in = 100;
        t.clips[0].fade_out = 200;
        t.pan = 0.3;
        let tracks = vec![t];
        let mut whole = vec![0f32; 2 * 1100];
        render(&tracks, 0, &mut whole, &RenderOpts::STEREO_MIX, &mut []);
        let mut pieces = Vec::new();
        for start in (0..1100).step_by(128) {
            let n = 128.min(1100 - start);
            let mut buf = vec![0f32; 2 * n];
            render(&tracks, start as u64, &mut buf, &RenderOpts::STEREO_MIX, &mut []);
            pieces.extend(buf);
        }
        assert_eq!(whole, pieces);
    }

    #[test]
    fn export_range_uses_markers() {
        let mut p = Project::new("x", 48000);
        p.tracks[0].insert(Clip::new("c", src(500, 0.1), 100));
        assert_eq!(export_range(&p, (None, None)), (0, 600));
        assert_eq!(export_range(&p, (Some(300), Some(200))), (200, 300));
        assert_eq!(export_range(&p, (Some(50), None)), (50, 600));
        assert_eq!(export_range(&p, (Some(70), Some(70))), (70, 70));
        let err = export_mix(&p, Path::new("never-written.wav"), (70, 70), false, Bits::I24).unwrap_err();
        assert!(err.to_string().contains("empty range"));
    }

    #[test]
    fn stems_and_mix_files() {
        let dir = std::env::temp_dir().join(format!("asciidaw-mix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Project::new(&dir, 48000);
        p.tracks[0].insert(Clip::new("a", src(4800, 0.5), 0));
        p.tracks[1].insert(Clip::new("b", src(2400, 0.25), 2400));
        let range = export_range(&p, (None, None));
        let mix = export_mix(&p, &dir.join("mix.wav"), range, false, Bits::I24).unwrap();
        assert_eq!(mix.frames, 4800);
        assert!((mix.peak - 0.75).abs() < 1e-5);
        let stems = export_stems(&p, &dir.join("stems"), range, Bits::I16).unwrap();
        assert_eq!(stems.len(), 2);
        let d = crate::wav::read_wav(&stems[1].path).unwrap();
        assert_eq!((d.channels.len(), d.frames()), (1, 4800));
        assert!(d.channels[0][..2400].iter().all(|s| s.abs() < 1e-3));
        assert!((d.channels[0][3000] - 0.25).abs() < 1e-3);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
