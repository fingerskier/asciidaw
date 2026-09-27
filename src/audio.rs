//! Immutable audio sources, peak caches for drawing, and small DSP helpers.

/// Frames summarised by one peak-cache entry.
pub const PEAK_BLOCK: usize = 256;

/// Max |sample| per `PEAK_BLOCK` frames (across all channels), built
/// incrementally so a take can be drawn while it is still being recorded.
#[derive(Debug, Clone, Default)]
pub struct Peaks {
    blocks: Vec<f32>,
    frames: usize,
}

impl Peaks {
    pub fn build(channels: &[Vec<f32>]) -> Self {
        let mut p = Peaks::default();
        p.extend(channels);
        p
    }

    /// Account for frames appended to `channels` since the last call.
    pub fn extend(&mut self, channels: &[Vec<f32>]) {
        let total = channels.first().map_or(0, Vec::len);
        for f in self.frames..total {
            let v = channels.iter().fold(0f32, |m, c| m.max(c[f].abs()));
            if f % PEAK_BLOCK == 0 {
                self.blocks.push(v);
            } else if let Some(last) = self.blocks.last_mut() {
                *last = last.max(v);
            }
        }
        self.frames = total;
    }

    /// Peak over frames `[start, end)`, using whole cached blocks where possible.
    pub fn peak(&self, channels: &[Vec<f32>], start: usize, end: usize) -> f32 {
        let end = end.min(self.frames);
        if start >= end {
            return 0.0;
        }
        let raw = |a: usize, b: usize| channels.iter().flat_map(|c| &c[a..b]).fold(0f32, |m, s| m.max(s.abs()));
        let first_full = start.div_ceil(PEAK_BLOCK);
        let last_full = end / PEAK_BLOCK; // exclusive
        if first_full >= last_full {
            return raw(start, end);
        }
        let head = raw(start, first_full * PEAK_BLOCK);
        let tail = raw(last_full * PEAK_BLOCK, end);
        self.blocks[first_full..last_full].iter().fold(head.max(tail), |m, &b| m.max(b))
    }
}

/// A decoded audio file living in the project's `audio/` directory.
/// Shared by clips via `Arc`, never mutated, always at the project rate
/// (resampled on import/load), so the mixer never converts rates.
#[derive(Debug)]
pub struct AudioData {
    /// File name inside the project's audio directory.
    pub file: String,
    pub channels: Vec<Vec<f32>>,
    peaks: Peaks,
}

impl AudioData {
    pub fn new(file: impl Into<String>, channels: Vec<Vec<f32>>) -> Self {
        assert!(!channels.is_empty(), "audio needs at least one channel");
        let peaks = Peaks::build(&channels);
        AudioData { file: file.into(), channels, peaks }
    }

    pub fn frames(&self) -> usize {
        self.channels[0].len()
    }

    pub fn num_channels(&self) -> usize {
        self.channels.len()
    }

    pub fn peak(&self, start: usize, end: usize) -> f32 {
        self.peaks.peak(&self.channels, start, end)
    }
}

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 1e-9 { -180.0 } else { 20.0 * gain.log10() }
}

/// Linear-interpolating sample-rate conversion. Adequate for speech and
/// tracking; not audiophile (no anti-alias filter when downsampling).
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let out_len = ((input.len() as u128 * to as u128) / from as u128) as usize;
    let step = from as f64 / to as f64;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * step;
            let idx = pos as usize;
            let frac = (pos - idx as f64) as f32;
            let a = input[idx.min(input.len() - 1)];
            let b = input[(idx + 1).min(input.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

/// Map a linear peak to 0..=1 on a dB scale spanning `floor_db`..0 dBFS.
/// Quiet recordings stay visible; that's the point of a meter.
pub fn meter_level(peak: f32, floor_db: f32) -> f32 {
    ((gain_to_db(peak) - floor_db) / -floor_db).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_match_brute_force() {
        let n = PEAK_BLOCK * 7 + 13;
        let a: Vec<f32> = (0..n).map(|i| ((i * 37 % 101) as f32 / 101.0) - 0.5).collect();
        let b: Vec<f32> = (0..n).map(|i| ((i * 53 % 97) as f32 / 97.0) * -0.9).collect();
        let ch = vec![a, b];
        let p = Peaks::build(&ch);
        for (s, e) in [(0, n), (5, 300), (255, 257), (256, 512), (700, n), (n - 1, n), (10, 10)] {
            let brute = ch.iter().flat_map(|c| &c[s..e]).fold(0f32, |m, x| m.max(x.abs()));
            assert_eq!(p.peak(&ch, s, e), brute, "range {s}..{e}");
        }
    }

    #[test]
    fn incremental_peaks_equal_bulk() {
        let full: Vec<f32> = (0..3000).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut growing = vec![Vec::new()];
        let mut p = Peaks::default();
        for chunk in full.chunks(333) {
            growing[0].extend_from_slice(chunk);
            p.extend(&growing);
        }
        let bulk = Peaks::build(&growing);
        assert_eq!(p.blocks, bulk.blocks);
    }

    #[test]
    fn resample_lengths_and_endpoints() {
        let x: Vec<f32> = (0..44100).map(|i| i as f32 / 44100.0).collect();
        let y = resample(&x, 44100, 48000);
        assert_eq!(y.len(), 48000);
        assert!((y[24000] - 0.5).abs() < 1e-3);
        assert_eq!(resample(&x, 48000, 48000), x);
    }

    #[test]
    fn db_roundtrip() {
        assert!((db_to_gain(-6.0206) - 0.5).abs() < 1e-4);
        assert!((gain_to_db(db_to_gain(-3.0)) + 3.0).abs() < 1e-4);
        assert_eq!(meter_level(1.0, -60.0), 1.0);
        assert_eq!(meter_level(0.0, -60.0), 0.0);
    }
}
