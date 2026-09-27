//! Terminal setup and the event loop.

use std::io::stdout;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use ratatui::crossterm::execute;

use crate::app::App;
use crate::engine::{AudioOpts, Engine, StderrRedirect};
use crate::project::Project;

/// The real stderr while it's redirected, so a panic can still be read.
static REAL_STDERR: AtomicI32 = AtomicI32::new(-1);

pub fn run(dir: PathBuf, opts: AudioOpts, rate: Option<u32>) -> Result<()> {
    let log = std::env::temp_dir().join("asciidaw.log");
    // SAFETY: dup of our own stderr, kept for the panic hook.
    REAL_STDERR.store(unsafe { libc::dup(2) }, Ordering::Relaxed);
    let redirect = StderrRedirect::to(&log);

    let (project, mut notes) =
        if Project::exists(&dir) { Project::load(&dir)? } else { (Project::new(&dir, rate.unwrap_or(48000)), vec![]) };
    let is_new = !Project::exists(&dir);
    let want_rate = if is_new { rate } else { Some(project.rate) };
    let engine = Engine::open(&opts, want_rate, true, true);
    let mut project = project;
    if is_new && rate.is_none() {
        // Adopt the hardware's rate so nothing needs converting.
        if let Some(r) = engine.output_rate().or(engine.input.as_ref().map(|i| i.info.rate)) {
            project.rate = r;
        }
    }
    notes.extend(engine.notes.iter().cloned());
    if let Some(i) = &engine.input
        && i.info.rate != project.rate
    {
        notes.push(format!("input runs at {} Hz; takes are resampled to {} Hz", i.info.rate, project.rate));
    }
    if let Some(o) = &engine.output
        && o.info.rate != project.rate
    {
        notes.push(format!(
            "output runs at {} Hz, project is {} Hz: playback pitch will be off",
            o.info.rate, project.rate
        ));
    }

    let mut app = App::new(project, engine);
    if !notes.is_empty() {
        app.warn(notes.join(" · "));
    }

    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture);
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        let fd = REAL_STDERR.load(Ordering::Relaxed);
        if fd >= 0 {
            // SAFETY: restore the saved stderr so the message is visible.
            unsafe { libc::dup2(fd, 2) };
        }
        prev_hook(info);
    }));

    let result = event_loop(&mut terminal, &mut app);
    app.shutdown();
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    drop(redirect);
    if app.dirty {
        eprintln!("asciidaw: quit without saving {}", app.project.dir.display());
    }
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.quit {
        app.tick();
        terminal.draw(|f| crate::ui::draw(f, app))?;
        if event::poll(Duration::from_millis(33))? {
            loop {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => app.on_key(k),
                    Event::Mouse(m) => app.on_mouse(m),
                    _ => {}
                }
                if app.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
    Ok(())
}
