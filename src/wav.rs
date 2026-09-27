//! WAV in/out via `hound`, plus optional ffmpeg-backed import of other formats.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;

use anyhow::{Context, Result, bail};

/// Output sample format for rendered/recorded files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Bits {
    /// 16-bit PCM with TPDF dither.
    I16,
    #[default]
    I24,
    F32,
}

impl FromStr for Bits {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim_end_matches("bit").trim_end_matches('f') {
            "16" => Ok(Bits::I16),
            "24" => Ok(Bits::I24),
            "32" => Ok(Bits::F32),
            _ => Err(format!("bit depth must be 16, 24 or 32 (float), not {s:?}")),
        }
    }
}

impl std::fmt::Display for Bits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Bits::I16 => "16-bit",
            Bits::I24 => "24-bit",
            Bits::F32 => "32-bit float",
        })
    }
}

fn spec(rate: u32, channels: u16, bits: Bits) -> hound::WavSpec {
    let (bits_per_sample, sample_format) = match bits {
        Bits::I16 => (16, hound::SampleFormat::Int),
        Bits::I24 => (24, hound::SampleFormat::Int),
        Bits::F32 => (32, hound::SampleFormat::Float),
    };
    hound::WavSpec { channels, sample_rate: rate, bits_per_sample, sample_format }
}

/// Tiny xorshift for dither noise; no need for a rand dependency.
struct Dither(u32);

impl Dither {
    fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x as f32 / u32::MAX as f32
    }
    /// Triangular noise in (-1, 1) LSB.
    fn tpdf(&mut self) -> f32 {
        self.next() - self.next()
    }
}

/// Streaming writer used for recording (so a crash keeps the take) and export.
pub struct WavOut {
    writer: hound::WavWriter<BufWriter<File>>,
    bits: Bits,
    dither: Dither,
    pub frames: u64,
    channels: usize,
}

impl WavOut {
    pub fn create(path: &Path, rate: u32, channels: u16, bits: Bits) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let writer = hound::WavWriter::create(path, spec(rate, channels, bits))
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(WavOut { writer, bits, dither: Dither(0x9E37_79B9), frames: 0, channels: channels as usize })
    }

    /// Write interleaved samples (length must be a multiple of the channel count).
    pub fn write_interleaved(&mut self, samples: &[f32]) -> Result<()> {
        for &s in samples {
            match self.bits {
                Bits::F32 => self.writer.write_sample(s)?,
                Bits::I24 => {
                    let v = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
                    self.writer.write_sample(v)?
                }
                Bits::I16 => {
                    let v = (s * 32767.0 + self.dither.tpdf()).round().clamp(-32768.0, 32767.0);
                    self.writer.write_sample(v as i16)?
                }
            }
        }
        self.frames += (samples.len() / self.channels) as u64;
        Ok(())
    }

    /// Rewrite the header so the file is valid up to this point.
    pub fn flush(&mut self) -> Result<()> {
        Ok(self.writer.flush()?)
    }

    pub fn finalize(self) -> Result<()> {
        Ok(self.writer.finalize()?)
    }
}

/// Write planar channels to a WAV file in one go. Goes through a sibling
/// temp file and a rename, so an existing file (say, the only copy of a
/// take) survives a failed write untouched.
pub fn write_wav(path: &Path, rate: u32, channels: &[Vec<f32>], bits: Bits) -> Result<()> {
    if channels.is_empty() {
        bail!("no audio channels to write to {}", path.display());
    }
    let name = path.file_name().with_context(|| format!("{} is not a file path", path.display()))?;
    let tmp = path.with_file_name(format!(".{}.tmp", name.to_string_lossy()));
    let written = (|| {
        let nch = channels.len();
        let frames = channels.first().map_or(0, Vec::len);
        let mut out = WavOut::create(&tmp, rate, nch as u16, bits)?;
        let mut buf = Vec::with_capacity(4096 * nch);
        for chunk_start in (0..frames).step_by(4096) {
            buf.clear();
            for f in chunk_start..(chunk_start + 4096).min(frames) {
                buf.extend(channels.iter().map(|c| c[f]));
            }
            out.write_interleaved(&buf)?;
        }
        out.finalize()?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Decoded file: sample rate plus planar f32 channels.
pub struct Decoded {
    pub rate: u32,
    pub channels: Vec<Vec<f32>>,
}

impl Decoded {
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }
}

/// Read a WAV file into planar f32.
pub fn read_wav(path: &Path) -> Result<Decoded> {
    let mut reader = hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    let nch = spec.channels as usize;
    if nch == 0 {
        bail!("{} has no channels", path.display());
    }
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect::<Result<_, _>>()?
        }
    };
    let frames = interleaved.len() / nch;
    let mut channels = vec![Vec::with_capacity(frames); nch];
    for frame in interleaved.chunks_exact(nch) {
        for (c, &s) in channels.iter_mut().zip(frame) {
            c.push(s);
        }
    }
    Ok(Decoded { rate: spec.sample_rate, channels })
}

pub fn is_wav(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("wav") || e.eq_ignore_ascii_case("wave"))
}

/// Decode any file: WAV natively, anything else through `ffmpeg` if installed.
pub fn read_any(path: &Path, scratch: &Path) -> Result<Decoded> {
    if is_wav(path) {
        return read_wav(path);
    }
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(path)
        .args(["-vn", "-c:a", "pcm_f32le"])
        .arg(scratch)
        .status();
    match status {
        Ok(s) if s.success() => {
            let decoded = read_wav(scratch);
            let _ = std::fs::remove_file(scratch);
            decoded
        }
        Ok(s) => bail!("ffmpeg failed to decode {} ({s})", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("{} is not a WAV file and ffmpeg is not installed", path.display())
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("asciidaw-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn roundtrip_all_depths() {
        let sig: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.05).sin() * 0.8).collect();
        let chans = vec![sig.clone(), sig.iter().map(|s| -s).collect()];
        for (bits, tol) in [(Bits::F32, 0.0), (Bits::I24, 1e-6), (Bits::I16, 1e-4)] {
            let p = tmp(&format!("rt-{bits:?}.wav"));
            write_wav(&p, 44100, &chans, bits).unwrap();
            let d = read_wav(&p).unwrap();
            assert_eq!(d.rate, 44100);
            assert_eq!(d.channels.len(), 2);
            assert_eq!(d.frames(), 1000);
            let err = d.channels[1].iter().zip(&chans[1]).fold(0f32, |m, (a, b)| m.max((a - b).abs()));
            assert!(err <= tol, "{bits:?} error {err}");
        }
    }

    #[test]
    fn failed_rewrite_leaves_the_original_intact() {
        let p = tmp("keep.wav");
        let take = vec![vec![0.25f32; 500]];
        write_wav(&p, 48000, &take, Bits::F32).unwrap();
        // Make the replacement write fail: a directory squats on the temp name.
        let blocker = p.with_file_name(".keep.wav.tmp");
        std::fs::create_dir_all(&blocker).unwrap();
        assert!(write_wav(&p, 44100, &[vec![0.5; 10]], Bits::F32).is_err());
        let d = read_wav(&p).unwrap();
        assert_eq!((d.rate, d.frames(), d.channels[0][0]), (48000, 500, 0.25));
        std::fs::remove_dir(&blocker).unwrap();
        assert!(write_wav(&p, 48000, &[], Bits::F32).is_err());
        // A successful rewrite replaces it.
        write_wav(&p, 44100, &[vec![0.5; 10]], Bits::F32).unwrap();
        assert_eq!(read_wav(&p).unwrap().rate, 44100);
        assert!(!blocker.exists(), "temp file left behind");
    }

    #[test]
    fn bits_parse() {
        assert_eq!("16".parse::<Bits>().unwrap(), Bits::I16);
        assert_eq!("24bit".parse::<Bits>().unwrap(), Bits::I24);
        assert_eq!("32f".parse::<Bits>().unwrap(), Bits::F32);
        assert!("8".parse::<Bits>().is_err());
    }
}
