//! Terminal UI: roster rendering, terminal lifecycle guard, and the event loop.
//!
//! The UI thread only polls terminal input and applies worker events; every
//! file/database read happens in the worker thread.

mod roster;
pub mod widgets;

use std::io::{self, Stdout};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::App;
use crate::domain::Timestamp;
use crate::worker::{WorkerEvent, WorkerHandle, WorkerRequest};

pub use roster::{MIN_HEIGHT, MIN_WIDTH, render_roster};

/// Presentation inputs that are not part of `App` state.
#[derive(Clone, Debug, Default)]
pub struct RosterContext {
    pub no_color: bool,
    /// Provider roots that were checked, shown in the empty state.
    pub roots: Vec<String>,
    /// Startup problems such as a provider that could not be opened.
    pub notes: Vec<String>,
    /// Local time of the last completed refresh.
    pub last_refresh: Option<String>,
    /// Set when the worker stopped; the UI shows `Live: paused`.
    pub error: Option<String>,
}

const TICK: Duration = Duration::from_millis(100);

type Term = Terminal<CrosstermBackend<Stdout>>;

fn restore_terminal() {
    // Best effort and idempotent: every step is attempted.
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
}

/// Owns raw mode and the alternate screen. Dropping restores both, and a panic
/// hook restores them before the panic message prints.
pub struct TerminalGuard {
    terminal: Term,
}

impl TerminalGuard {
    /// Enter raw mode and the alternate screen. The caller must have checked
    /// that stdin and stdout are terminals.
    pub fn enter() -> io::Result<Self> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
        enable_raw_mode()?;
        let setup = execute!(io::stdout(), EnterAlternateScreen).and_then(|()| {
            let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
            terminal.hide_cursor()?;
            Ok(terminal)
        });
        match setup {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                restore_terminal();
                Err(error)
            }
        }
    }

    pub fn terminal(&mut self) -> &mut Term {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn now_millis() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as Timestamp)
}

fn clock_label() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Outcome of handling one key.
#[derive(Debug, PartialEq, Eq)]
enum KeyAction {
    None,
    Quit,
    Refresh,
}

fn handle_key(app: &mut App, key: KeyEvent) -> KeyAction {
    if key.kind != KeyEventKind::Press {
        return KeyAction::None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => KeyAction::Quit,
            KeyCode::Char('u') => {
                app.move_selection(-10);
                KeyAction::None
            }
            KeyCode::Char('d') => {
                app.move_selection(10);
                KeyAction::None
            }
            _ => KeyAction::None,
        };
    }
    match key.code {
        // No text input exists on the roster yet, so `q` always quits here.
        KeyCode::Char('q') => return KeyAction::Quit,
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::PageDown => app.move_selection(10),
        KeyCode::PageUp => app.move_selection(-10),
        KeyCode::Char('g') | KeyCode::Home => app.move_selection(isize::MIN),
        KeyCode::Char('G') | KeyCode::End => app.move_selection(isize::MAX),
        KeyCode::Char('s') => app.cycle_sort(),
        KeyCode::Char('S') => app.sort(app.sort_mode()),
        KeyCode::Char('r') => return KeyAction::Refresh,
        _ => {}
    }
    KeyAction::None
}

/// Apply every queued worker event. Returns `false` when the worker is gone.
fn drain_worker(app: &mut App, ctx: &mut RosterContext, worker: &WorkerHandle) -> bool {
    loop {
        match worker.rx.try_recv() {
            Ok(WorkerEvent::Roster {
                generation,
                sessions,
                warnings,
                scan,
            }) => {
                app.apply_roster_generation(generation, sessions, warnings, scan);
            }
            Ok(WorkerEvent::Warning { warning }) => app.apply_warning(warning),
            Ok(WorkerEvent::RefreshFinished { .. }) => ctx.last_refresh = Some(clock_label()),
            Ok(_) => {}
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        }
    }
}

/// Run the roster until the user quits. `respawn` restarts a stopped worker
/// when the user presses `r`.
pub fn run(
    app: &mut App,
    ctx: &mut RosterContext,
    mut worker: WorkerHandle,
    respawn: &dyn Fn() -> WorkerHandle,
) -> io::Result<()> {
    let mut guard = TerminalGuard::enter()?;
    let mut last_clock = now_millis();
    app.set_now(last_clock);
    loop {
        if ctx.error.is_none() && !drain_worker(app, ctx, &worker) {
            ctx.error = Some("worker thread exited".into());
            app.live = false;
        }
        let now = now_millis();
        if now - last_clock >= 1000 {
            last_clock = now;
            app.set_now(now);
        }
        guard
            .terminal()
            .draw(|frame| render_roster(frame, app, ctx))?;

        if !event::poll(TICK)? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            match handle_key(app, key) {
                KeyAction::Quit => break,
                KeyAction::Refresh if ctx.error.is_some() => {
                    worker = respawn();
                    ctx.error = None;
                    app.live = true;
                }
                KeyAction::Refresh => {
                    let _ = worker.tx.send(WorkerRequest::Refresh);
                }
                KeyAction::None => {}
            }
        }
    }
    drop(guard);
    worker.shutdown();
    Ok(())
}
