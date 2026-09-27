//! Non-interactive subcommands.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait};

use crate::app::{fmt_time, parse_time};
use crate::audio::{gain_to_db, meter_level};
use crate::engine::{AudioOpts, Engine, StderrRedirect, device_label, select_host};
use crate::mix;
use crate::project::{Input, Project};
use crate::wav::{self, Bits, WavOut};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn quiet_alsa() -> Option<StderrRedirect> {
    StderrRedirect::to(Path::new("/dev/null"))
}

pub fn devices(opts: &AudioOpts) -> Result<()> {
    let hosts: Vec<_> = cpal::available_hosts().iter().map(|h| h.name()).collect();
    println!("hosts: {}  (pick with --host)", hosts.join(", "));
    let gag = quiet_alsa();
    let host = select_host(opts.host.as_deref())?;
    let id_of = |d: &cpal::Device| d.id().map(|i| i.to_string()).ok();
    let default_in = host.default_input_device().and_then(|d| id_of(&d));
    let default_out = host.default_output_device().and_then(|d| id_of(&d));
    let devices: Vec<_> = host.devices()?.collect();
    let mut lines = vec![format!("using {}", host.id().name())];
    for d in devices {
        let label = device_label(&d);
        let id = id_of(&d).unwrap_or_default();
        let mut tags = Vec::new();
        if default_in.as_deref() == Some(&id) {
            tags.push("default in");
        }
        if default_out.as_deref() == Some(&id) {
            tags.push("default out");
        }
        let tags = if tags.is_empty() { String::new() } else { format!("  [{}]", tags.join(", ")) };
        lines.push(format!("\n{label}{tags}\n  id: {id}"));
        if let Ok(c) = d.default_input_config() {
            lines.push(format!("  in:  {} ch, {} Hz, {}", c.channels(), c.sample_rate(), c.sample_format()));
        }
        if let Ok(c) = d.default_output_config() {
            lines.push(format!("  out: {} ch, {} Hz, {}", c.channels(), c.sample_rate(), c.sample_format()));
        }
    }
    drop(gag);
    println!("{}", lines.join("\n"));
    println!("\nselect with --input / --output <id or part of the name>");
    Ok(())
}

pub struct RecordArgs {
    pub out: PathBuf,
    pub channels: Option<String>,
    pub duration: Option<String>,
    pub bits: Bits,
    pub split: bool,
    pub rate: Option<u32>,
}

/// Headless capture straight to WAV: the "just record something" path.
pub fn record(args: RecordArgs, opts: &AudioOpts) -> Result<()> {
    let limit = match &args.duration {
        Some(d) => Some(parse_time(d).with_context(|| format!("bad duration {d:?} (use seconds or m:ss)"))?),
        None => None,
    };
    let gag = quiet_alsa();
    let mut engine = Engine::open(opts, args.rate, false, true);
    drop(gag);
    let Some(input) = engine.input.as_ref() else {
        bail!("{}", engine.notes.join("; "));
    };
    let info = input.info.clone();
    let spec = match &args.channels {
        Some(s) => Input::parse(s).with_context(|| format!("bad --channels {s:?}: use 1, 2 or 1-2"))?,
        None => Input { channel: 0, stereo: info.channels >= 2 },
    };
    if !spec.fits(info.channels) {
        bail!("{} has {} input channel(s); can't record {spec}", info.name, info.channels);
    }
    let mono_files = args.split && spec.stereo;
    let paths: Vec<PathBuf> = if mono_files {
        let stem = args.out.file_stem().map_or("take".into(), |s| s.to_string_lossy().into_owned());
        (0..2).map(|k| args.out.with_file_name(format!("{stem}-{}.wav", spec.channel + 1 + k))).collect()
    } else {
        vec![args.out.clone()]
    };
    for p in &paths {
        if p.exists() {
            bail!("{} already exists; not overwriting", p.display());
        }
    }
    let mut writers = Vec::new();
    for p in &paths {
        match WavOut::create(p, info.rate, if mono_files { 1 } else { spec.channels() }, args.bits) {
            Ok(w) => writers.push(w),
            Err(e) => {
                drop(writers);
                discard(&paths);
                return Err(e);
            }
        }
    }

    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
    }
    std::thread::spawn(|| {
        // EOF (stdin not a terminal) is not a stop request; a line is.
        let mut line = String::new();
        if matches!(std::io::stdin().lock().read_line(&mut line), Ok(n) if n > 0) {
            STOP.store(true, Ordering::Relaxed);
        }
    });

    eprintln!(
        "recording {} → {}  ({} Hz, {})",
        info.name,
        paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
        info.rate,
        args.bits
    );
    eprintln!("input {spec}; press enter or ctrl-c to stop");
    let dev_ch = info.channels as usize;
    let (c0, n) = (spec.channel as usize, spec.channels() as usize);
    let mut buf = Vec::new();
    let mut scratch: Vec<Vec<f32>> = vec![Vec::new(); writers.len()];
    let mut frames = 0u64;
    let mut peak_total = [0f32; 2];
    let mut level = [0f32; 2];
    let mut last_audio = Instant::now();
    let mut stalled = false;
    let mut last_flush = Instant::now();
    let mut overruns_seen = engine.shared.input_overruns.load(Ordering::Relaxed);
    let mut capture = || -> Result<()> {
        loop {
            buf.clear();
            let overruns = engine.shared.input_overruns.load(Ordering::Relaxed);
            if let Some(input) = &mut engine.input {
                input.drain(&mut buf);
            }
            // Dropped buffers become silence so the file keeps real time.
            let gap = overruns.saturating_sub(overruns_seen).min(info.rate as u64 * 60);
            overruns_seen = overruns;
            buf.resize(buf.len() + gap as usize * dev_ch, 0.0);
            for s in &mut scratch {
                s.clear();
            }
            if let Some(l) = limit {
                // Stop exactly at the requested length, not at the end of a buffer.
                let want = (l * info.rate as f64).round() as u64;
                buf.truncate(want.saturating_sub(frames) as usize * dev_ch);
            }
            let mut peak = [0f32; 2];
            for frame in buf.chunks_exact(dev_ch) {
                for k in 0..n {
                    let s = frame[c0 + k];
                    peak[k] = peak[k].max(s.abs());
                    scratch[if mono_files { k } else { 0 }].push(s);
                }
            }
            for (w, s) in writers.iter_mut().zip(&scratch) {
                w.write_interleaved(s)?;
            }
            frames += (buf.len() / dev_ch) as u64;
            for k in 0..n {
                peak_total[k] = peak_total[k].max(peak[k]);
                level[k] = peak[k].max(level[k] * 0.8);
            }
            if last_flush.elapsed() > Duration::from_secs(1) {
                for w in &mut writers {
                    w.flush()?;
                }
                last_flush = Instant::now();
            }
            let mut meter = String::new();
            for (k, &l) in level.iter().take(n).enumerate() {
                let cells = (meter_level(l, -60.0) * 20.0).round() as usize;
                meter += &format!(
                    "  {} {:<20} {:>5.1}",
                    spec.channel as usize + 1 + k,
                    "█".repeat(cells),
                    gain_to_db(l).max(-99.0)
                );
            }
            eprint!("\r\x1b[2K● {}{meter}", fmt_time(frames, info.rate));
            let _ = std::io::stderr().flush();
            let done = limit.is_some_and(|l| frames >= (l * info.rate as f64).round() as u64);
            if STOP.load(Ordering::Relaxed) || done {
                break Ok(());
            }
            if let Some(e) = engine.shared.take_errors().first() {
                eprintln!("\n{e}");
            }
            std::thread::sleep(Duration::from_millis(30));
            // Watch wall time, not frames: a device that stops delivering
            // (unplugged, server gone) must not hang a timed recording.
            if !buf.is_empty() {
                last_audio = Instant::now();
            } else if last_audio.elapsed() > Duration::from_secs(3) {
                if frames == 0 {
                    bail!("no audio arriving from {} after 3 s", info.name);
                }
                stalled = true;
                break Ok(());
            }
        }
    };
    let captured = capture();
    engine.input = None; // stop capture before finalising
    eprintln!();
    if let Err(e) = captured {
        drop(writers);
        discard(&paths);
        return Err(e);
    }
    if frames == 0 {
        drop(writers);
        discard(&paths);
        bail!("nothing was recorded");
    }
    for w in writers {
        w.finalize()?;
    }
    let overruns = engine.shared.input_overruns.load(Ordering::Relaxed);
    for (k, p) in paths.iter().enumerate() {
        let pk = if mono_files { peak_total[k] } else { peak_total[0].max(peak_total[1]) };
        println!(
            "{}  {}  peak {:.1} dBFS{}",
            p.display(),
            fmt_time(frames, info.rate),
            gain_to_db(pk),
            if pk >= 0.999 { "  (clipped!)" } else { "" }
        );
    }
    if overruns > 0 {
        eprintln!("warning: {overruns} frames dropped (system too busy)");
    }
    if stalled {
        bail!("{} stopped delivering audio; kept the {} recorded before that", info.name, fmt_time(frames, info.rate));
    }
    Ok(())
}

/// Remove files a failed recording created, so a retry isn't refused.
fn discard(paths: &[PathBuf]) {
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}

pub struct ExportArgs {
    pub dir: PathBuf,
    pub out: Option<PathBuf>,
    pub stems: bool,
    pub mono: bool,
    pub bits: Bits,
    pub from: Option<String>,
    pub to: Option<String>,
}

pub fn export(args: ExportArgs) -> Result<()> {
    let (project, warnings) = Project::load(&args.dir)?;
    for w in warnings {
        eprintln!("warning: {w}");
    }
    let t = |s: &Option<String>| -> Result<Option<u64>> {
        s.as_deref()
            .map(|v| parse_time(v).map(|secs| project.secs_to_frames(secs)).with_context(|| format!("bad time {v:?}")))
            .transpose()
    };
    let range = mix::export_range(&project, (t(&args.from)?, t(&args.to)?));
    let exports = project.dir.join("exports");
    let name = project.name();
    let report = |r: &mix::Rendered| {
        println!(
            "{}  {}  peak {:.1} dBFS{}",
            r.path.display(),
            fmt_time(r.frames, project.rate),
            gain_to_db(r.peak),
            if r.clipped(args.bits) { "  (clipped!)" } else { "" }
        );
    };
    if args.stems {
        let dir = args.out.unwrap_or_else(|| exports.join(format!("{name}-stems")));
        for r in mix::export_stems(&project, &dir, range, args.bits)? {
            report(&r);
        }
    } else {
        let path = args.out.unwrap_or_else(|| exports.join(format!("{name}-mix.wav")));
        report(&mix::export_mix(&project, &path, range, args.mono, args.bits)?);
    }
    Ok(())
}

pub fn info(files: &[PathBuf]) -> Result<()> {
    for path in files {
        let spec = hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?.spec();
        let d = wav::read_wav(path)?;
        let fmt = match spec.sample_format {
            hound::SampleFormat::Float => format!("{}-bit float", spec.bits_per_sample),
            hound::SampleFormat::Int => format!("{}-bit", spec.bits_per_sample),
        };
        println!("{}", path.display());
        println!(
            "  {} Hz, {} ch, {fmt}, {} ({} frames)",
            d.rate,
            d.channels.len(),
            fmt_time(d.frames() as u64, d.rate),
            d.frames()
        );
        for (i, c) in d.channels.iter().enumerate() {
            let peak = c.iter().fold(0f32, |m, s| m.max(s.abs()));
            let rms = (c.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / c.len().max(1) as f64).sqrt() as f32;
            println!("  ch{}: peak {:>6.1} dBFS  rms {:>6.1} dBFS", i + 1, gain_to_db(peak), gain_to_db(rms));
        }
    }
    Ok(())
}
