# asciidaw

A terminal DAW for the boring, important job: **record a track or two, trim it, get a `.wav` out.**

```
 ▶  ●  ■     0:07.400  STOP   demo *                                              48000 Hz · 162 ms/col
 cursor 0:07.400          ▏0:00       ▏0:02       ▏0:04        ▏0:06   ▼   ▏0:08       ▏0:10        ▏0:1
▌1 Vocal                  ▏track-1-take-001                            ▏track-1-take-001
  M   S   R   in 1        ▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅│  ▁▃▃▄▄▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅▅
 +0.0 dB    pan C         █████████████████████████████████████████████│▄▇███████████████████████████
 ························ █████████████████████████████████████████████▅█████████████████████████████
 2 Guitar                                   ▏rec-001              ▏rec-002
  M   S   R   in 2                          ▂▂▂▂▂▂▂▂▂▂▂▂▂         ▂▂▂▂▂▂▂▂▂▂▂▂▂
 +0.0 dB    pan L30                         █████████████         █████████████
 ························                   █████████████         █████████████

 IN 1 ██████████████▍·   -6 2 ████████████▊···  -12                 │ OUT L ············ R ············
 fade in 1.60s                          track-1-take-001 · 0:04.625 · peak -6.0 dBFS · fades 1.60/0.00s
```

- Record mono or stereo takes from any input channel(s), several tracks at once, while hearing the rest.
- Clips: split, cut / copy / paste, duplicate, drag between tracks, nudge, fades, clip gain, normalize, undo.
- Tracks: mute, solo, arm, gain, pan, input routing, live meters.
- Rough bar-graph waveforms (dB-scaled, so quiet takes are still visible), mouse and keyboard.
- Export a stereo or mono mix, per-track stems, or a single clip as 16 (dithered) / 24 / 32-float WAV.
- Headless `record` / `export` / `info` subcommands when you don't want a UI at all.

## Install

### Prebuilt binary

Every [release](https://github.com/fingerskier/asciidaw/releases) has Linux (x86_64, arm64) and macOS (Apple
silicon, Intel) builds:

```sh
# x86_64-unknown-linux-gnu · aarch64-unknown-linux-gnu · aarch64-apple-darwin · x86_64-apple-darwin
target=x86_64-unknown-linux-gnu
mkdir -p ~/.local/bin
curl -fsSL "https://github.com/fingerskier/asciidaw/releases/latest/download/asciidaw-$target.tar.gz" \
  | tar -xz -C ~/.local/bin asciidaw
asciidaw --version                      # if not found, add ~/.local/bin to your PATH
```

Linux builds need glibc ≥ 2.35 (Ubuntu 22.04, Debian 12, anything rolling) and `libasound.so.2`, which any
desktop distro already has. macOS builds are unsigned: fine via `curl`, but if you downloaded with a browser run
`xattr -d com.apple.quarantine asciidaw` first. Each release also has a `SHA256SUMS`.

### From source

Needs Rust ≥ 1.88 and the ALSA headers:

```sh
sudo pacman -S alsa-lib                 # Arch / Omarchy
sudo apt install libasound2-dev pkg-config   # Debian / Ubuntu
sudo dnf install alsa-lib-devel         # Fedora

cargo install --locked --git https://github.com/fingerskier/asciidaw   # or, in a clone: cargo install --path .
```

On PipeWire or PulseAudio systems the default ALSA device is routed through their ALSA plugin, so it just
works. To talk to a server directly, build with `--features pipewire` (or `jack`, `pulseaudio`) and pass
`--host PipeWire`. `ffmpeg` is optional: only needed to `:import` non-WAV files.

Linux is the tested platform. macOS should work (cpal + CoreAudio); Windows won't build yet (a unix-only
stderr redirect keeps ALSA's chatter off the screen).

## Use

```sh
asciidaw devices                        # what can I record from?
asciidaw my-song                        # open or create a project directory; press r
asciidaw my-song --input "Scarlett"     # pick devices by id or part of the name
asciidaw record take.wav -c 1-2 -d 3:00 # no UI: stereo pair, three minutes (or press enter)
asciidaw record take.wav -c 1-2 --split # ... as take-1.wav + take-2.wav
asciidaw export my-song                 # my-song/exports/my-song-mix.wav
asciidaw export my-song --stems -b 16   # one file per track, 16-bit dithered
asciidaw info take.wav                  # format, length, peak/RMS per channel
```

### Keys

| key | does |
| --- | --- |
| `space` / `enter` | play–stop (cursor returns to where you started) / stop here |
| `r` | record armed tracks (arms the selected track if none are); `space` or `r` stops |
| `←` `→` `h` `l` | move the cursor one column (`shift` ×10), `home`/`end` or `g`/`G` |
| `↑` `↓` `j` `k` | select track · `tab` / `shift-tab` jump to next / previous clip |
| `=` `-` `0` | zoom in / out / to fit |
| `a` `m` `s` | arm · mute · solo |
| `[` `]` | track gain −/+ 1 dB |
| `n` | new track |
| `b` | split the clip under the cursor |
| `x` `c` `v` | cut · copy · paste at the cursor (paste overwrites what's under it) |
| `d` `del` | duplicate the clip after itself · delete it |
| `alt-←` `alt-→` | nudge the clip (stops at neighbours) |
| `{` `}` | fade in up to the cursor · fade out from the cursor |
| `i` `o` `esc` | in / out markers (exports use them) · clear |
| `u` `U` | undo · redo (`ctrl-z` / `ctrl-y` too) |
| `e` | export the mix to `exports/` |
| `ctrl-s` `q` | save · quit (asks if unsaved) |
| `?` | help |

**Mouse:** click to place the cursor and select, drag clips (across tracks too), drag on the ruler to
set in/out, click `M` `S` `R` and the input label, wheel over the dB / pan fields to adjust them,
wheel to scroll, `ctrl`-wheel to zoom.

### `:` commands

```
:export [path] [mono] [16|24|32]   mix (default exports/<name>-mix-NNN.wav)
:stems [dir] [16|24|32]            one file per track
:clip [path]                       just the selected clip
:import <file>                     at the cursor, on the selected track
:gain <dB>   :pan L30|C|R100   :input 1 | 1-2   :rename <name>
:clipgain <dB>   :normalize [peak dBFS, default -1]   :fadein <s>   :fadeout <s>
:latency <ms>   :goto <m:ss>   :bits 16|24|32   :devices   :track [name]   :deltrack
:w   :q   :q!   :wq
```

## Projects

A project is a directory:

```
my-song/
  project.json     tracks, clips, gains — plain JSON, diff-able
  audio/           every take and import, 32-bit float WAV, never modified
  exports/         what you render
```

Takes stream to `audio/` while you record (the header is refreshed every second, so a crash leaves a
playable file), and the project autosaves when a take finishes. Imports are copied in and converted to the
project rate. Edits never touch audio files; clips are windows onto them.

## Choices worth knowing

- **Clips on a track never overlap.** Pasting or dropping a clip overwrites what's underneath; nudging with
  the keyboard stops at neighbours instead. No hidden layers, no surprise sums.
- **0 dB pan law.** A mono take panned centre comes out of the mix at exactly its recorded level
  (hard-panned it's +3 dB on that side). Stems are unpanned, post-fader, at the track's native width.
- **Undo is for edits, not faders.** Undo restores clips and tracks but leaves mute, solo, arm, gain and
  pan where you put them.
- **Latency compensation is manual.** Record a clap along with an existing clap, measure how late it lands,
  set `:latency <ms>` once per interface. New takes shift earlier by that much.
- **No software input monitoring.** Use your interface's direct monitoring; a terminal app adding
  round-trip latency to your headphones helps nobody.
- **Rates:** new projects adopt the output device's rate. Input at a different rate is resampled when the
  take stops (linear; fine for tracking, not a mastering-grade SRC). Output at a different rate plays
  off-pitch and says so.

## On the original plan

The first README suggested Rust, ratatui, ffmpeg/ffprobe, DSP and MIDI. Kept: Rust, ratatui + crossterm (with
mouse). Dropped from the core: ffmpeg — WAV in and out is a few hundred lines with `hound`, and a DAW
that can't record without an external binary is fragile; ffmpeg remains an optional import path. "DSP" became
the concrete set that primary use case needs: gain, pan, fades, normalize, mixdown, dither, resampling.
MIDI is deferred.

## Not yet

Metronome / tempo grid · effects · MIDI · automatic latency measurement · proper sinc resampling ·
waveform zoom per track · Windows.

## Development

```sh
cargo test                 # model, mixer, WAV, edits, commands, and TUI rendering via TestBackend
cargo clippy --all-targets
```

To release, bump `version` in `Cargo.toml`, run `cargo check` so `Cargo.lock` follows, commit, then
`git tag v0.2.0 && git push origin v0.2.0`. The release workflow runs CI, checks the tag matches `Cargo.toml`,
builds the four targets and publishes them (a tag with a `-`, like `v0.2.0-rc.1`, becomes a prerelease).

The audio thread never locks or allocates: it owns a snapshot of the tracks (audio is `Arc`-shared, so
snapshots are cheap), swaps in new ones from a lock-free ring and hands the old ones back to the UI thread
to free. Input arrives through a second ring in whole buffers so channels can't tear.

No sound card? Fake one with ALSA plugins (the `null` slave isn't clocked, so expect "overruns"):

```
# ~/.asoundrc (or HOME=/some/dir with this file in it)
pcm.!default { type asym; playback.pcm "out"; capture.pcm "in" }
pcm.out { type file; slave.pcm "null"; file "/dev/null"; format "raw" }
pcm.in  { type file; slave.pcm "null"; file "/dev/null"; infile "/path/to/stereo-f32le.raw"; format "raw" }
```
