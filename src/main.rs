//! asciidaw — a terminal DAW for recording a track or two and getting WAVs out.

mod app;
mod audio;
mod cli;
mod engine;
mod mix;
mod project;
mod tui;
mod ui;
mod wav;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::engine::AudioOpts;
use crate::wav::Bits;

#[derive(Parser)]
#[command(
    name = "asciidaw",
    version,
    about = "A terminal DAW: record a track or two, trim and arrange clips, export .wav files.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Project directory to open or create (default: current directory).
    /// Holds project.json, audio/ (takes) and exports/.
    dir: Option<PathBuf>,
    #[command(flatten)]
    audio: AudioArgs,
}

#[derive(Args)]
struct AudioArgs {
    /// Audio host (e.g. ALSA, JACK, PipeWire; see `asciidaw devices`)
    #[arg(long, global = true)]
    host: Option<String>,
    /// Input device: id or part of its name
    #[arg(long, global = true, value_name = "DEVICE")]
    input: Option<String>,
    /// Output device: id or part of its name
    #[arg(long, global = true, value_name = "DEVICE")]
    output: Option<String>,
    /// Sample rate for new projects / headless recording (default: device's)
    #[arg(long, global = true, value_name = "HZ")]
    rate: Option<u32>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List audio hosts and devices
    Devices,
    /// Record straight to a WAV file, no TUI
    Record {
        /// Output file
        out: PathBuf,
        /// Input channel(s), 1-based: 1, 2, or a stereo pair like 1-2
        /// (default: 1-2 if the device has two inputs, else 1)
        #[arg(short, long, value_name = "N|N-M")]
        channels: Option<String>,
        /// Stop after this long (seconds or m:ss); otherwise press enter
        #[arg(short, long, value_name = "TIME")]
        duration: Option<String>,
        /// 16 (dithered), 24 or 32 (float)
        #[arg(short, long, default_value = "24")]
        bits: Bits,
        /// With a stereo pair, write two mono files (NAME-1.wav, NAME-2.wav)
        #[arg(long)]
        split: bool,
    },
    /// Render a project to WAV without opening the TUI
    Export {
        /// Project directory
        dir: PathBuf,
        /// Output file (or directory with --stems); default under DIR/exports/
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// One file per track instead of a stereo mix
        #[arg(long)]
        stems: bool,
        /// Mono mixdown (pan ignored)
        #[arg(long, conflicts_with = "stems")]
        mono: bool,
        /// 16 (dithered), 24 or 32 (float)
        #[arg(short, long, default_value = "24")]
        bits: Bits,
        /// Start time (seconds or m:ss)
        #[arg(long)]
        from: Option<String>,
        /// End time (seconds or m:ss)
        #[arg(long)]
        to: Option<String>,
    },
    /// Show format, length and levels of WAV files
    Info {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let opts = AudioOpts { host: cli.audio.host, input: cli.audio.input, output: cli.audio.output };
    match cli.cmd {
        None => tui::run(cli.dir.unwrap_or_else(|| PathBuf::from(".")), opts, cli.audio.rate),
        Some(Cmd::Devices) => cli::devices(&opts),
        Some(Cmd::Record { out, channels, duration, bits, split }) => {
            cli::record(cli::RecordArgs { out, channels, duration, bits, split, rate: cli.audio.rate }, &opts)
        }
        Some(Cmd::Export { dir, out, stems, mono, bits, from, to }) => {
            cli::export(cli::ExportArgs { dir, out, stems, mono, bits, from, to })
        }
        Some(Cmd::Info { files }) => cli::info(&files),
    }
}
