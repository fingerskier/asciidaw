//! Drawing. Everything is painted straight into the frame buffer; the
//! regions that respond to the mouse are recorded in `app.hits`.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, Mode, StatusKind, TrackHit, fmt_pan, fmt_time};
use crate::audio::{gain_to_db, meter_level};
use crate::project::Clip;

pub const HEADER_W: u16 = 26;
pub const TRACK_H: u16 = 4;
const BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const HBARS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
const WAVE_FLOOR_DB: f32 = -48.0;
const METER_FLOOR_DB: f32 = -60.0;

const BAR_BG: Color = Color::Indexed(235);
const HEADER_BG: Color = Color::Indexed(234);
const HEADER_SEL_BG: Color = Color::Indexed(237);
const DIM: Color = Color::Indexed(244);

#[derive(Clone, Copy)]
struct ItemStyle {
    bg: Color,
    title_bg: Color,
    wave: Color,
    title: Color,
}

const CLIP: ItemStyle =
    ItemStyle { bg: Color::Indexed(236), title_bg: Color::Indexed(238), wave: Color::Cyan, title: Color::Gray };
const CLIP_SEL: ItemStyle =
    ItemStyle { bg: Color::Indexed(17), title_bg: Color::Indexed(25), wave: Color::LightCyan, title: Color::White };
const CLIP_MUTED: ItemStyle =
    ItemStyle { bg: Color::Indexed(235), title_bg: Color::Indexed(237), wave: DIM, title: DIM };
const CLIP_DRAG: ItemStyle =
    ItemStyle { bg: Color::Indexed(58), title_bg: Color::Indexed(100), wave: Color::Yellow, title: Color::White };
const TAKE: ItemStyle =
    ItemStyle { bg: Color::Indexed(52), title_bg: Color::Indexed(88), wave: Color::LightRed, title: Color::White };

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < 50 || area.height < 3 + TRACK_H + 2 {
        f.render_widget(Paragraph::new("asciidaw: terminal too small (need 50×9)"), area);
        return;
    }
    let [top, ruler, body, meters, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    app.hits = crate::app::Hits {
        lanes_x: area.x + HEADER_W,
        lanes_w: area.width.saturating_sub(HEADER_W),
        visible_tracks: (body.height / TRACK_H).max(1) as usize,
        ..Default::default()
    };
    if app.fit_pending {
        app.zoom_fit();
        app.fit_pending = false;
    }
    {
        let buf = f.buffer_mut();
        draw_top(buf, top, app);
        draw_ruler(buf, ruler, app);
        draw_tracks(buf, body, app);
        draw_meters(buf, meters, app);
        draw_status(buf, status, app);
    }
    if app.mode == Mode::Help {
        draw_help(f, area);
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, max: u16, style: Style) -> u16 {
    if max == 0 {
        return x;
    }
    buf.set_stringn(x, y, s, max as usize, style).0
}

fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    buf.set_style(area, style);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(c) = buf.cell_mut(Position { x, y }) {
                c.set_char(' ');
            }
        }
    }
}

fn zoom_label(app: &App) -> String {
    let secs = app.zoom as f64 / app.rate() as f64;
    if secs < 1.0 { format!("{:.0} ms/col", secs * 1000.0) } else { format!("{secs:.1} s/col") }
}

fn draw_top(buf: &mut Buffer, area: Rect, app: &mut App) {
    let base = Style::new().bg(BAR_BG).fg(Color::Gray);
    fill(buf, area, base);
    let (x0, y) = (area.x, area.y);
    let playing = app.is_playing() && app.recording.is_none();
    let recording = app.recording.is_some();
    let btn = |on: bool, color: Color| {
        if on { Style::new().bg(color).fg(Color::Black).add_modifier(Modifier::BOLD) } else { base.fg(color) }
    };
    put(buf, x0, y, " ▶ ", 3, btn(playing, Color::Green));
    put(buf, x0 + 3, y, " ● ", 3, btn(recording, Color::Red));
    put(buf, x0 + 6, y, " ■ ", 3, base.fg(Color::Gray));
    app.hits.play = Rect::new(x0, y, 3, 1);
    app.hits.rec = Rect::new(x0 + 3, y, 3, 1);
    app.hits.stop = Rect::new(x0 + 6, y, 3, 1);

    let mut x = x0 + 10;
    let time = fmt_time(app.position(), app.rate());
    x = put(buf, x, y, &format!("{time:>11}"), 11, base.fg(Color::White).add_modifier(Modifier::BOLD));
    let (label, style) = if recording {
        (" REC ", Style::new().bg(Color::Red).fg(Color::White).add_modifier(Modifier::BOLD))
    } else if playing {
        (" PLAY ", Style::new().bg(Color::Green).fg(Color::Black))
    } else {
        (" STOP ", base.fg(DIM))
    };
    x = put(buf, x + 1, y, label, 6, style);
    let name = format!("  {}{}", app.project.name(), if app.dirty { " *" } else { "" });
    let name_style = if app.dirty { base.fg(Color::Yellow) } else { base.fg(Color::White) };
    x = put(buf, x, y, &name, 30, name_style);

    let rate = app.rate();
    let mut right = format!("{} Hz · {} ", rate, zoom_label(app));
    if let Some(o) = &app.engine.output
        && o.info.rate != rate
    {
        right = format!("⚠ output at {} Hz · {right}", o.info.rate);
    }
    let rw = right.chars().count() as u16;
    let rx = (area.right().saturating_sub(rw)).max(x + 1);
    put(buf, rx, y, &right, area.right().saturating_sub(rx), base.fg(DIM));
}

fn nice_interval(secs_per_col: f64) -> f64 {
    const NICE: [f64; 20] = [
        0.001, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0,
        1200.0, 3600.0,
    ];
    let target = secs_per_col * 11.0;
    NICE.into_iter().find(|&n| n >= target).unwrap_or(3600.0)
}

fn fmt_short(secs: f64, interval: f64) -> String {
    let m = (secs / 60.0).floor();
    let s = secs - m * 60.0;
    if interval >= 1.0 {
        format!("{}:{:02}", m, s.round() as u64)
    } else if interval >= 0.1 {
        format!("{}:{:04.1}", m, s)
    } else if interval >= 0.01 {
        format!("{}:{:05.2}", m, s)
    } else {
        format!("{}:{:06.3}", m, s)
    }
}

fn col_of(app: &App, frame: u64) -> Option<u16> {
    if frame < app.view_start {
        return None;
    }
    let x = (frame - app.view_start) / app.zoom;
    (x < app.hits.lanes_w as u64).then(|| app.hits.lanes_x + x as u16)
}

fn draw_ruler(buf: &mut Buffer, area: Rect, app: &mut App) {
    let base = Style::new().fg(DIM);
    fill(buf, area, base);
    app.hits.ruler = area;
    let y = area.y;
    let rate = app.rate();
    let info = match app.markers {
        (Some(a), Some(b)) => format!(" in {} out {}", fmt_time(a, rate), fmt_time(b, rate)),
        (Some(a), None) => format!(" in {}", fmt_time(a, rate)),
        (None, Some(b)) => format!(" out {}", fmt_time(b, rate)),
        _ => format!(" cursor {}", fmt_time(app.cursor, rate)),
    };
    put(buf, area.x, y, &info, HEADER_W - 1, base);

    let (lx, lw) = (app.hits.lanes_x, app.hits.lanes_w);
    // Marker range shading.
    if let (Some(a), Some(b)) = app.markers {
        for x in lx..lx + lw {
            let f = app.view_start + (x - lx) as u64 * app.zoom;
            if f + app.zoom > a
                && f < b
                && let Some(c) = buf.cell_mut(Position { x, y })
            {
                c.set_bg(Color::Indexed(24));
            }
        }
    }
    let interval = nice_interval(app.zoom as f64 / rate as f64);
    let step = ((interval * rate as f64).round() as u64).max(1);
    let mut next_free = lx;
    for x in lx..lx + lw {
        let f0 = app.view_start + (x - lx) as u64 * app.zoom;
        let tick = f0.div_ceil(step) * step;
        if tick < f0 + app.zoom && x >= next_free {
            let label = format!("▏{}", fmt_short(tick as f64 / rate as f64, interval));
            let end = put(buf, x, y, &label, lx + lw - x, base.fg(Color::Gray));
            next_free = end + 1;
        }
    }
    for (frame, color) in [(app.markers.0, Color::Blue), (app.markers.1, Color::Blue)] {
        if let Some(x) = frame.and_then(|f| col_of(app, f)) {
            put(buf, x, y, "┃", 1, Style::new().fg(color).add_modifier(Modifier::BOLD));
        }
    }
    if let Some(x) = col_of(app, app.cursor) {
        put(buf, x, y, "▼", 1, Style::new().fg(Color::Yellow));
    }
    if (app.is_playing() || app.recording.is_some())
        && let Some(x) = col_of(app, app.position())
    {
        let c = if app.recording.is_some() { Color::Red } else { Color::Green };
        put(buf, x, y, "▼", 1, Style::new().fg(c));
    }
}

fn meter_cells(buf: &mut Buffer, x: u16, y: u16, width: u16, peak: f32, bg: Color) {
    let level = meter_level(peak, METER_FLOOR_DB);
    let eighths = (level * width as f32 * 8.0).round() as u32;
    for i in 0..width {
        let n = eighths.saturating_sub(i as u32 * 8).min(8) as usize;
        let db_here = METER_FLOOR_DB * (1.0 - (i as f32 + 0.5) / width as f32);
        let color = if db_here > -6.0 {
            Color::Red
        } else if db_here > -18.0 {
            Color::Yellow
        } else {
            Color::Green
        };
        if let Some(c) = buf.cell_mut(Position { x: x + i, y }) {
            let (ch, fg) = if n == 0 { ('·', Color::Indexed(238)) } else { (HBARS[n], color) };
            c.set_char(ch).set_style(Style::new().fg(fg).bg(bg));
        }
    }
}

fn draw_tracks(buf: &mut Buffer, body: Rect, app: &mut App) {
    let visible = app.hits.visible_tracks;
    let n = app.project.tracks.len();
    app.track_scroll = app.track_scroll.min(n.saturating_sub(visible));
    let lanes = Rect::new(app.hits.lanes_x, body.y, app.hits.lanes_w, body.height);
    for (row, ti) in (app.track_scroll..n).take(visible).enumerate() {
        let y = body.y + row as u16 * TRACK_H;
        let header = Rect::new(body.x, y, HEADER_W, TRACK_H);
        let lane = Rect::new(lanes.x, y, lanes.width, TRACK_H);
        let mut hit = draw_header(buf, header, app, ti);
        hit.lane = lane;
        draw_lane(buf, lane, app, ti);
        app.hits.tracks.push(hit);
    }
    let shown = n.saturating_sub(app.track_scroll).min(visible);
    if shown < n {
        let more = format!(" {} of {n} tracks (scroll) ", shown);
        put(buf, body.x, body.bottom() - 1, &more, HEADER_W, Style::new().fg(DIM));
    }
    // Cursor, playhead and marker lines through every lane.
    let used = Rect::new(lanes.x, body.y, lanes.width, shown as u16 * TRACK_H);
    let mut lines = vec![(app.markers.0, '┆', Color::Blue, None), (app.markers.1, '┆', Color::Blue, None)];
    lines.push((Some(app.cursor), '│', Color::Yellow, Some(Color::Indexed(94))));
    if app.is_playing() || app.recording.is_some() {
        let c = if app.recording.is_some() { Color::Red } else { Color::Green };
        lines.push((Some(app.position()), '│', c, Some(Color::Indexed(22))));
    }
    for (frame, ch, fg, bg) in lines {
        let Some(x) = frame.and_then(|f| col_of(app, f)) else { continue };
        for y in used.top()..used.bottom() {
            if let Some(c) = buf.cell_mut(Position { x, y }) {
                if c.symbol() == " " {
                    c.set_char(ch).set_fg(fg);
                } else if let Some(bg) = bg {
                    c.set_bg(bg);
                }
            }
        }
    }
}

fn draw_header(buf: &mut Buffer, area: Rect, app: &App, ti: usize) -> TrackHit {
    let track = &app.project.tracks[ti];
    let selected = ti == app.sel_track;
    let bg = if selected { HEADER_SEL_BG } else { HEADER_BG };
    let base = Style::new().bg(bg).fg(Color::Gray);
    fill(buf, area, base);
    let (x, y) = (area.x, area.y);
    let w = area.width.saturating_sub(1);
    if selected {
        put(buf, x, y, "▌", 1, base.fg(Color::Yellow));
    }
    let title = format!("{} {}", ti + 1, track.name);
    let title_style = if selected { base.fg(Color::White).add_modifier(Modifier::BOLD) } else { base };
    put(buf, x + 1, y, &title, w - 1, title_style);

    let btn = |on: bool, color: Color| {
        if on { Style::new().bg(color).fg(Color::Black).add_modifier(Modifier::BOLD) } else { base.fg(DIM) }
    };
    let recording_here = app.recording.as_ref().is_some_and(|r| r.takes.iter().any(|t| t.track == ti));
    let arm_style = if recording_here {
        btn(true, Color::Red).add_modifier(Modifier::SLOW_BLINK)
    } else {
        btn(track.armed, Color::Red)
    };
    let y1 = y + 1;
    put(buf, x + 1, y1, " M ", 3, btn(track.mute, Color::Yellow));
    put(buf, x + 5, y1, " S ", 3, btn(track.solo, Color::Green));
    put(buf, x + 9, y1, " R ", 3, arm_style);
    let input = format!("in {}", track.input);
    let bad_input = app.input_channels().is_some_and(|ch| !track.input.fits(ch));
    let in_style = if bad_input { base.fg(Color::Red) } else { base.fg(Color::Gray) };
    let in_w = put(buf, x + 14, y1, &input, w.saturating_sub(14), in_style) - (x + 14);

    let y2 = y + 2;
    let gain = format!("{:+.1} dB", track.gain_db);
    let gain_end =
        put(buf, x + 1, y2, &gain, 10, base.fg(if track.gain_db == 0.0 { Color::Gray } else { Color::White }));
    let pan = format!("pan {}", fmt_pan(track.pan));
    let pan_x = x + 12;
    let pan_end = put(buf, pan_x, y2, &pan, w.saturating_sub(12), base);

    let y3 = y + 3;
    let live_input = track.armed && !app.is_playing();
    let level = if live_input || recording_here {
        let (c0, n) = (track.input.channel as usize, track.input.channels() as usize);
        app.in_levels.iter().skip(c0).take(n).fold(0f32, |m, &l| m.max(l))
    } else {
        app.track_levels.get(ti).copied().unwrap_or(0.0)
    };
    meter_cells(buf, x + 1, y3, w.saturating_sub(1), level, bg);

    TrackHit {
        track: ti,
        header: area,
        lane: Rect::default(),
        mute: Rect::new(x + 1, y1, 3, 1),
        solo: Rect::new(x + 5, y1, 3, 1),
        arm: Rect::new(x + 9, y1, 3, 1),
        input: Rect::new(x + 14, y1, in_w, 1),
        gain: Rect::new(x + 1, y2, gain_end - (x + 1), 1),
        pan: Rect::new(pan_x, y2, pan_end - pan_x, 1),
    }
}

/// Something drawn in a lane: a clip, a drag preview or a live take.
struct Item<'a> {
    start: u64,
    len: u64,
    name: &'a str,
    style: ItemStyle,
}

/// Draw an item as a titled bar-graph. `peak(a, b)` gives the linear peak
/// over timeline frames [a, b).
fn draw_item(buf: &mut Buffer, lane: Rect, app: &App, item: Item, peak: impl Fn(u64, u64) -> f32) {
    let Item { start, len, name, style } = item;
    let end = start + len;
    let (vs, z) = (app.view_start, app.zoom);
    if len == 0 || end <= vs {
        return;
    }
    let x0 = start.saturating_sub(vs) / z;
    if x0 >= lane.width as u64 {
        return;
    }
    let x1 = (end - vs).div_ceil(z).min(lane.width as u64);
    let rows = lane.height.saturating_sub(1) as u32;
    for x in x0..x1 {
        let f0 = vs + x * z;
        let (a, b) = (f0.max(start), (f0 + z).min(end));
        if a >= b {
            continue;
        }
        let p = peak(a, b);
        let eighths = (meter_level(p, WAVE_FLOOR_DB) * (rows * 8) as f32).round() as u32;
        let wave = if p >= 0.999 { Color::Red } else { style.wave };
        let cx = lane.x + x as u16;
        if let Some(c) = buf.cell_mut(Position { x: cx, y: lane.y }) {
            c.set_char(' ').set_style(Style::new().bg(style.title_bg));
        }
        for r in 0..rows {
            let from_bottom = rows - 1 - r;
            let n = eighths.saturating_sub(from_bottom * 8).min(8) as usize;
            if let Some(c) = buf.cell_mut(Position { x: cx, y: lane.y + 1 + r as u16 }) {
                c.set_char(BARS[n]).set_style(Style::new().fg(wave).bg(style.bg));
            }
        }
    }
    let title = format!("▏{name}");
    put(buf, lane.x + x0 as u16, lane.y, &title, (x1 - x0) as u16, Style::new().fg(style.title).bg(style.title_bg));
}

fn clip_peak(clip: &Clip, a: u64, b: u64) -> f32 {
    let (i0, i1) = (a - clip.start, b - clip.start);
    let raw = clip.source.peak((clip.offset + i0) as usize, (clip.offset + i1) as usize);
    raw * clip.gain() * clip.fade((i0 + i1) / 2)
}

fn draw_lane(buf: &mut Buffer, lane: Rect, app: &App, ti: usize) {
    let track = &app.project.tracks[ti];
    let dragging = app.drag.as_ref().filter(|d| d.moved);
    let audible = !track.mute && (!app.project.tracks.iter().any(|t| t.solo) || track.solo);
    for (ci, clip) in track.clips.iter().enumerate() {
        if dragging.is_some_and(|d| d.from_track == ti && d.clip == ci) {
            continue;
        }
        let style = if ti == app.sel_track && app.sel_clip == Some(ci) {
            CLIP_SEL
        } else if audible {
            CLIP
        } else {
            CLIP_MUTED
        };
        let item = Item { start: clip.start, len: clip.len, name: &clip.name, style };
        draw_item(buf, lane, app, item, |a, b| clip_peak(clip, a, b));
    }
    if let Some(d) = dragging.filter(|d| d.to_track == ti)
        && let Some(clip) = app.project.tracks.get(d.from_track).and_then(|t| t.clips.get(d.clip))
    {
        let offset = d.to_start as i64 - clip.start as i64;
        let name = format!("{} → {}", clip.name, fmt_time(d.to_start, app.rate()));
        let item = Item { start: d.to_start, len: clip.len, name: &name, style: CLIP_DRAG };
        draw_item(buf, lane, app, item, |a, b| clip_peak(clip, (a as i64 - offset) as u64, (b as i64 - offset) as u64));
    }
    if let Some(rec) = &app.recording {
        let proj = app.rate() as u64;
        let dev = rec.rate as u64;
        for take in rec.takes.iter().filter(|t| t.track == ti) {
            let len = take.data[0].len() as u64 * proj / dev;
            let secs = len as f64 / proj as f64;
            let name = format!("● REC {secs:.1}s → {}", take.file);
            let item = Item { start: rec.start, len, name: &name, style: TAKE };
            draw_item(buf, lane, app, item, |a, b| {
                let sa = ((a - rec.start) * dev / proj) as usize;
                let sb = (((b - rec.start) * dev).div_ceil(proj) as usize).max(sa + 1);
                take.peaks.peak(&take.data, sa, sb)
            });
        }
    }
}

fn draw_meters(buf: &mut Buffer, area: Rect, app: &App) {
    let base = Style::new().bg(BAR_BG).fg(Color::Gray);
    fill(buf, area, base);
    let y = area.y;
    let mut x = put(buf, area.x, y, " IN ", 4, base.fg(DIM));
    let out_w: u16 = 2 * 12 + 12;
    let avail = area.width.saturating_sub(4 + out_w + 2);
    match &app.engine.input {
        None => {
            put(buf, x, y, "no input device", avail, base.fg(Color::Red));
        }
        Some(_) => {
            let n = app.in_levels.len().clamp(1, 8) as u16;
            let each = (avail / n).clamp(8, 24);
            for (c, &level) in app.in_levels.iter().take(n as usize).enumerate() {
                if x + each > area.x + 4 + avail {
                    break;
                }
                let db = gain_to_db(level);
                let label = format!("{:<2}", c + 1);
                x = put(buf, x, y, &label, 2, base.fg(Color::Gray));
                let mw = each.saturating_sub(8);
                meter_cells(buf, x, y, mw, level, BAR_BG);
                x += mw;
                let num = if db > -99.0 { format!("{db:>5.0} ") } else { "   -∞ ".into() };
                let clip_style =
                    if level >= 0.999 { base.fg(Color::Red).add_modifier(Modifier::BOLD) } else { base.fg(DIM) };
                x = put(buf, x, y, &num, 6, clip_style);
            }
        }
    }
    let ox = area.right().saturating_sub(out_w);
    put(buf, ox, y, "│ OUT ", 6, base.fg(DIM));
    if app.engine.output.is_none() {
        put(buf, ox + 6, y, "no output device", out_w - 6, base.fg(Color::Red));
    } else {
        for (i, (label, level)) in ["L", "R"].iter().zip(app.out_levels).enumerate() {
            let mx = ox + 6 + i as u16 * 15;
            put(buf, mx, y, label, 1, base.fg(Color::Gray));
            meter_cells(buf, mx + 2, y, 12, level, BAR_BG);
        }
    }
}

fn draw_status(buf: &mut Buffer, area: Rect, app: &App) {
    let base = Style::new().fg(Color::Gray);
    fill(buf, area, base);
    let (x, y, w) = (area.x, area.y, area.width);
    match &app.mode {
        Mode::Command(s) => {
            let end = put(buf, x, y, &format!(":{s}"), w - 1, Style::new().fg(Color::White));
            put(buf, end, y, "█", 1, Style::new().fg(Color::Gray));
            return;
        }
        Mode::ConfirmQuit => {
            put(
                buf,
                x,
                y,
                " unsaved changes — s: save & quit · q: quit without saving · any other key: cancel",
                w,
                Style::new().fg(Color::Yellow),
            );
            return;
        }
        _ => {}
    }
    let left_end = if let Some(s) = &app.status {
        let style = match s.kind {
            StatusKind::Info => Style::new().fg(Color::White),
            StatusKind::Warn => Style::new().fg(Color::Yellow),
            StatusKind::Error => Style::new().fg(Color::LightRed).add_modifier(Modifier::BOLD),
        };
        put(buf, x, y, &format!(" {}", s.text), w, style)
    } else {
        let hint = app.status_hint().unwrap_or_else(|| "? help · space play · r record · e export · : command".into());
        put(buf, x, y, &format!(" {hint}"), w, Style::new().fg(DIM))
    };
    if let Some(c) = app.selected_clip() {
        let rate = app.rate();
        let mut info = format!("{} · {} · peak {:.1} dBFS", c.name, fmt_time(c.len, rate), gain_to_db(c.peak()));
        if c.gain_db != 0.0 {
            info += &format!(" · {:+.1} dB", c.gain_db);
        }
        if c.fade_in > 0 || c.fade_out > 0 {
            info += &format!(" · fades {:.2}/{:.2}s", c.fade_in as f64 / rate as f64, c.fade_out as f64 / rate as f64);
        }
        let iw = info.chars().count() as u16 + 1;
        if left_end + iw + 2 < area.right() {
            put(buf, area.right() - iw, y, &info, iw, Style::new().fg(DIM));
        }
    }
}

const HELP: &[(&str, &str)] = &[
    ("", "TRANSPORT"),
    ("space", "play / stop (back to start) · stop recording"),
    ("enter", "stop here (pause)"),
    ("r", "record armed tracks (arms the selected one if none)"),
    ("←→ h l", "move cursor · shift ×10 · home/end g/G"),
    ("↑↓ j k", "select track · tab/shift-tab: next/prev clip"),
    ("= -  0", "zoom in / out / fit"),
    ("i o esc", "set in / out markers · clear (export uses them)"),
    ("", "TRACKS"),
    ("a m s", "arm · mute · solo"),
    ("[ ]", "track gain −/+ 1 dB (or wheel over dB / pan)"),
    ("n", "new track (:deltrack removes)"),
    ("", "CLIPS"),
    ("b", "split at cursor"),
    ("x c v", "cut · copy · paste at cursor (overwrites)"),
    ("d  del", "duplicate after itself · delete"),
    ("alt-←→", "nudge (stops at neighbours) · or drag with mouse"),
    ("{ }", "fade in up to cursor · fade out from cursor"),
    ("u U", "undo · redo (ctrl-z / ctrl-y too)"),
    ("", "FILES"),
    ("e", "export mix to exports/ · ctrl-s save · q quit"),
    (":", "export [path] [mono] [16|24|32] · stems [dir] · clip [path]"),
    ("", "import <file> · gain <dB> · pan L30 · input 1 | 1-2"),
    ("", "clipgain <dB> · normalize [-1] · fadein/fadeout <s>"),
    ("", "rename <name> · latency <ms> · goto <m:ss> · devices · w · q"),
    ("", "mouse: click to seek/select, drag clips, drag ruler = in/out,"),
    ("", "wheel scrolls, ctrl-wheel zooms, click M S R / input"),
];

fn draw_help(f: &mut Frame, area: Rect) {
    // Two columns when the terminal is wide but short.
    let cols: u16 = if area.height < HELP.len() as u16 + 2 && area.width >= 150 { 2 } else { 1 };
    let rows = HELP.len().div_ceil(cols as usize);
    let w = (76 * cols).min(area.width);
    let h = (rows as u16 + 2).min(area.height);
    let rect = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(k, d)| {
            if k.is_empty() && d.chars().all(|c| c.is_ascii_uppercase()) {
                Line::from(Span::styled(format!(" {d}"), Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)))
            } else {
                Line::from(vec![
                    Span::styled(format!(" {k:<9}"), Style::new().fg(Color::Cyan)),
                    Span::styled(d.to_string(), Style::new().fg(Color::Gray)),
                ])
            }
        })
        .collect();
    let block = Block::bordered().title(" asciidaw — any key closes ").style(Style::new().bg(Color::Indexed(234)));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    let areas = Layout::horizontal(vec![Constraint::Ratio(1, cols as u32); cols as usize]).split(inner);
    for (chunk, col) in lines.chunks(rows).zip(areas.iter()) {
        f.render_widget(Paragraph::new(chunk.to_vec()), *col);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::project::tests::src;
    use crate::project::{Clip, Project};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_tracks_waveforms_and_hits() {
        let mut p = Project::new("demo-song", 1000);
        let mut clip = Clip::new("vocal take", src(3000, 0.5), 1000);
        clip.fade_in = 500;
        p.tracks[0].insert(clip);
        let mut app = App::new(p, Engine::offline());
        let s = screen(&mut app, 100, 14);
        assert!(s.contains("demo-song"), "{s}");
        assert!(s.contains("1 Track 1") && s.contains("2 Track 2"), "{s}");
        assert!(s.contains("vocal take"), "{s}");
        assert!(s.contains('█'), "waveform bars missing:\n{s}");
        assert!(s.contains("no input device"), "{s}");
        assert_eq!(app.hits.tracks.len(), 2);
        assert_eq!(app.hits.lanes_x, HEADER_W);
        // Fit-to-window on open: the clip end (4000) lands inside the lane.
        assert!(app.zoom * app.hits.lanes_w as u64 >= 4000);
    }

    #[test]
    fn help_and_command_line() {
        let mut app = App::new(Project::new("x", 48000), Engine::offline());
        app.mode = Mode::Help;
        assert!(screen(&mut app, 100, 40).contains("TRANSPORT"));
        app.mode = Mode::Command("export mix.wav".into());
        assert!(screen(&mut app, 100, 20).contains(":export mix.wav█"));
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let mut app = App::new(Project::new("x", 48000), Engine::offline());
        screen(&mut app, 20, 5);
        screen(&mut app, 50, 9);
    }
}
