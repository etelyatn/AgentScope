//! Terminal UI: roster rendering, terminal lifecycle guard, and the event loop.
//!
//! The UI thread only polls terminal input and applies worker events; every
//! file/database read happens in the worker thread.

mod details;
mod inspector;
mod panels;
mod roster;
pub mod widgets;

use std::io::{self, Stdout};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::ThreadId;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::{App, Panel, Screen};
use crate::domain::Timestamp;
use crate::worker::{WorkerEvent, WorkerHandle, WorkerRequest};

pub use inspector::{REASONING_LABEL, render_inspector};
pub use roster::{MIN_HEIGHT, MIN_WIDTH, render_roster};

/// Draw whichever screen is active, then any panel on top of it.
pub fn render(frame: &mut ratatui::Frame, app: &App, ctx: &RosterContext) {
    match app.screen {
        Screen::Roster => render_roster(frame, app, ctx),
        Screen::Inspector => render_inspector(frame, app, ctx),
        Screen::Details => details::render_details(frame, app, ctx),
        Screen::Source => details::render_source(frame, app, ctx),
        Screen::Diagnostics => details::render_diagnostics(frame, app, ctx),
    }
    if app.panel() != Panel::None {
        panels::render_panel(frame, app, ctx);
    }
}

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
/// Most queued key presses handled before the next redraw.
const MAX_KEYS_PER_FRAME: usize = 64;

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
        install_panic_hook(std::thread::current().id(), restore_terminal);
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
pub enum KeyAction {
    None,
    Quit,
    Refresh,
}

/// Apply one key press to the app. Pure state change: no I/O.
///
/// Text input comes first: while a search is being edited every printable key,
/// `q` included, is text, and only `Ctrl+c` still quits.
pub fn handle_key(app: &mut App, key: KeyEvent) -> KeyAction {
    if key.kind != KeyEventKind::Press {
        return KeyAction::None;
    }
    // Windows reports AltGr as Ctrl+Alt; that is a character, not a chord.
    let chord =
        key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT);
    let editing = app.is_editing();
    if chord {
        return match key.code {
            KeyCode::Char('c') => KeyAction::Quit,
            KeyCode::Char('u') if editing => {
                app.search_clear();
                KeyAction::None
            }
            _ if editing => KeyAction::None,
            KeyCode::Char('u') => {
                page_move(app, -10);
                KeyAction::None
            }
            KeyCode::Char('d') => {
                page_move(app, 10);
                KeyAction::None
            }
            _ => KeyAction::None,
        };
    }
    if editing {
        return handle_search_key(app, key.code);
    }
    if app.panel() != Panel::None {
        return handle_panel_key(app, key.code);
    }
    match key.code {
        KeyCode::Char('?') => {
            app.open_help();
            return KeyAction::None;
        }
        KeyCode::Char('p')
            if matches!(
                app.screen,
                Screen::Roster | Screen::Inspector | Screen::Details
            ) =>
        {
            app.open_processes();
            return KeyAction::None;
        }
        KeyCode::Tab => {
            app.focus_next();
            return KeyAction::None;
        }
        KeyCode::BackTab => {
            app.focus_prev();
            return KeyAction::None;
        }
        _ => {}
    }
    if app.screen == Screen::Roster {
        handle_roster_key(app, key.code)
    } else {
        handle_inspector_key(app, key.code)
    }
}

fn handle_roster_key(app: &mut App, code: KeyCode) -> KeyAction {
    match code {
        KeyCode::Char('q') => return KeyAction::Quit,
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::PageDown => app.move_selection(10),
        KeyCode::PageUp => app.move_selection(-10),
        KeyCode::Char('g') | KeyCode::Home => app.move_selection(isize::MIN),
        KeyCode::Char('G') | KeyCode::End => app.move_selection(isize::MAX),
        KeyCode::Enter => {
            app.open_inspector();
        }
        KeyCode::Char('/') => {
            app.begin_search();
        }
        KeyCode::Char('f') => {
            app.open_filter();
        }
        KeyCode::Char('s') => app.cycle_sort(),
        KeyCode::Char('S') => app.sort(app.sort_mode()),
        KeyCode::Char('r') => return KeyAction::Refresh,
        _ => {}
    }
    KeyAction::None
}

/// Keys while a search is being edited: everything printable is text.
fn handle_search_key(app: &mut App, code: KeyCode) -> KeyAction {
    match code {
        KeyCode::Esc => app.search_cancel(),
        KeyCode::Enter => app.search_commit(),
        KeyCode::Backspace => app.search_backspace(),
        KeyCode::Char(c) => app.search_push(c),
        _ => {}
    }
    KeyAction::None
}

/// Keys inside the help, process, and filter panels. `q` is not a text key in
/// the filter panel, so there it does nothing rather than quit.
fn handle_panel_key(app: &mut App, code: KeyCode) -> KeyAction {
    let panel = app.panel();
    match code {
        KeyCode::Char('q') if panel == Panel::Filter => {}
        KeyCode::Char('q') => return KeyAction::Quit,
        KeyCode::Esc | KeyCode::Backspace => app.close_panel(),
        KeyCode::Char('?') if panel == Panel::Help => app.close_panel(),
        KeyCode::Char('p') if panel == Panel::Processes => app.close_panel(),
        KeyCode::Char('f') if panel == Panel::Filter => app.close_panel(),
        KeyCode::Enter | KeyCode::Char(' ') if panel == Panel::Filter => app.panel_activate(),
        KeyCode::Char('c') if panel == Panel::Filter => app.clear_filters(),
        KeyCode::Char('j') | KeyCode::Down => app.panel_move(1),
        KeyCode::Char('k') | KeyCode::Up => app.panel_move(-1),
        KeyCode::PageDown => app.panel_move(10),
        KeyCode::PageUp => app.panel_move(-10),
        KeyCode::Char('g') | KeyCode::Home => app.panel_move(isize::MIN),
        KeyCode::Char('G') | KeyCode::End => app.panel_move(isize::MAX),
        _ => {}
    }
    KeyAction::None
}

fn page_move(app: &mut App, delta: isize) {
    if app.panel() != Panel::None {
        return app.panel_move(delta);
    }
    match app.screen {
        Screen::Details | Screen::Source => app.overlay_scroll(delta),
        Screen::Diagnostics => app.diagnostics_move(delta),
        Screen::Inspector => app.inspector_move(delta),
        Screen::Roster => app.move_selection(delta),
    }
}

fn handle_inspector_key(app: &mut App, code: KeyCode) -> KeyAction {
    match app.screen {
        Screen::Details | Screen::Source => return handle_scroll_overlay_key(app, code),
        Screen::Diagnostics => return handle_diagnostics_key(app, code),
        _ => {}
    }
    match code {
        KeyCode::Char('q') => return KeyAction::Quit,
        KeyCode::Char('j') | KeyCode::Down => app.inspector_move(1),
        KeyCode::Char('k') | KeyCode::Up => app.inspector_move(-1),
        KeyCode::PageDown => app.inspector_move(10),
        KeyCode::PageUp => app.inspector_move(-10),
        KeyCode::Char('g') | KeyCode::Home => app.inspector_first(),
        KeyCode::Char('G') | KeyCode::End => app.inspector_last(),
        KeyCode::Enter => app.inspector_toggle(),
        KeyCode::Char('/') => {
            app.begin_search();
        }
        KeyCode::Char('n') => app.search_step(true),
        KeyCode::Char('N') => app.search_step(false),
        KeyCode::Char('d') => {
            app.open_details();
        }
        KeyCode::Char('v') => {
            app.open_selected_source();
        }
        KeyCode::Char('D') => {
            app.open_diagnostics();
        }
        KeyCode::Char('x') => app.toggle_redaction(),
        KeyCode::Esc | KeyCode::Backspace => app.close_inspector(),
        KeyCode::Char('r') => {
            app.retry_transcript();
            return KeyAction::Refresh;
        }
        _ => {}
    }
    KeyAction::None
}

/// Details and source overlays: scroll, and the source/redaction/diagnostics keys.
fn handle_scroll_overlay_key(app: &mut App, code: KeyCode) -> KeyAction {
    match code {
        KeyCode::Char('q') => return KeyAction::Quit,
        KeyCode::Char('j') | KeyCode::Down => app.overlay_scroll(1),
        KeyCode::Char('k') | KeyCode::Up => app.overlay_scroll(-1),
        KeyCode::PageDown => app.overlay_scroll(10),
        KeyCode::PageUp => app.overlay_scroll(-10),
        KeyCode::Home => app.overlay_scroll(isize::MIN),
        KeyCode::End => app.overlay_scroll(isize::MAX),
        KeyCode::Char('x') => app.toggle_redaction(),
        KeyCode::Char('v') if app.screen == Screen::Details => {
            app.open_selected_source();
        }
        KeyCode::Char('D') if app.screen == Screen::Details => {
            app.open_diagnostics();
        }
        KeyCode::Esc | KeyCode::Backspace => {
            app.close_overlay();
        }
        _ => {}
    }
    KeyAction::None
}

fn handle_diagnostics_key(app: &mut App, code: KeyCode) -> KeyAction {
    match code {
        KeyCode::Char('q') => return KeyAction::Quit,
        KeyCode::Char('j') | KeyCode::Down => app.diagnostics_move(1),
        KeyCode::Char('k') | KeyCode::Up => app.diagnostics_move(-1),
        KeyCode::PageDown => app.diagnostics_move(10),
        KeyCode::PageUp => app.diagnostics_move(-10),
        KeyCode::Home => app.diagnostics_move(isize::MIN),
        KeyCode::End => app.diagnostics_move(isize::MAX),
        KeyCode::Enter | KeyCode::Char('v') => {
            app.open_diagnostic_source();
        }
        KeyCode::Char('x') => app.toggle_redaction(),
        KeyCode::Esc | KeyCode::Backspace => {
            app.close_overlay();
        }
        _ => {}
    }
    KeyAction::None
}

/// Send queued inspector requests to the worker. Never blocks: the request
/// channel is unbounded, and a stopped worker simply drops them.
pub fn flush_requests(app: &mut App, worker: &WorkerHandle) {
    for request in app.take_requests() {
        let _ = worker.tx.send(request);
    }
}

/// Apply one worker event to the app and presentation context.
pub fn apply_worker_event(app: &mut App, ctx: &mut RosterContext, event: WorkerEvent) {
    match event {
        WorkerEvent::Roster {
            generation,
            sessions,
            warnings,
            scan,
        } => {
            app.apply_roster_generation(generation, sessions, warnings, scan);
        }
        WorkerEvent::Warning { warning } => app.apply_warning(warning),
        WorkerEvent::LoadFailed { request_id } => app.transcript_load_failed(request_id),
        WorkerEvent::EventsAfter { request_id, count } => {
            app.apply_events_after(request_id, count);
        }
        WorkerEvent::RefreshFinished { .. } => ctx.last_refresh = Some(clock_label()),
        WorkerEvent::Transcript { request_id, page } => {
            app.apply_transcript(request_id, page);
        }
        WorkerEvent::TranscriptInvalidated { key } => app.invalidate_transcript(&key),
        WorkerEvent::Source { request_id, record } => {
            app.apply_source(request_id, record);
        }
        WorkerEvent::Processes { snapshot } => app.apply_processes(snapshot),
    }
}

/// Apply every queued worker event. Returns `false` when the worker is gone.
pub fn drain_worker(app: &mut App, ctx: &mut RosterContext, worker: &WorkerHandle) -> bool {
    loop {
        match worker.rx.try_recv() {
            Ok(event) => apply_worker_event(app, ctx, event),
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        }
    }
}

/// The worker thread is gone: show `Live: paused` until `r` replaces it.
pub fn worker_stopped(app: &mut App, ctx: &mut RosterContext) {
    ctx.error = Some("worker thread exited".into());
    app.live = false;
}

/// A replacement worker plus the channel on which its (worker-thread) reader
/// construction reports startup problems.
pub struct Respawned {
    pub worker: WorkerHandle,
    pub notes: Receiver<Vec<String>>,
}

/// Run the roster until the user quits. `respawn` restarts a stopped worker
/// when the user presses `r`; it must not block on provider I/O.
pub fn run(
    app: &mut App,
    ctx: &mut RosterContext,
    mut worker: WorkerHandle,
    respawn: &dyn Fn() -> Respawned,
) -> io::Result<()> {
    let mut guard = TerminalGuard::enter()?;
    let mut notes_rx: Option<Receiver<Vec<String>>> = None;
    let mut last_clock = now_millis();
    app.set_now(last_clock);
    loop {
        if let Some(rx) = &notes_rx
            && let Ok(notes) = rx.try_recv()
        {
            ctx.notes = notes;
        }
        if ctx.error.is_none() && !drain_worker(app, ctx, &worker) {
            worker_stopped(app, ctx);
        }
        flush_requests(app, &worker);
        let now = now_millis();
        if now - last_clock >= 1000 {
            last_clock = now;
            app.set_now(now);
        }
        guard.terminal().draw(|frame| render(frame, app, ctx))?;

        if !event::poll(TICK)? {
            continue;
        }
        // Handle this key and any already queued, then redraw once.
        let mut quit = false;
        let mut handled = 0;
        loop {
            if let Event::Key(key) = event::read()? {
                match handle_key(app, key) {
                    KeyAction::Quit => {
                        quit = true;
                        break;
                    }
                    KeyAction::Refresh if ctx.error.is_some() => {
                        let replacement = respawn();
                        worker = replacement.worker;
                        notes_rx = Some(replacement.notes);
                        reset_for_new_worker(app, ctx);
                    }
                    KeyAction::Refresh => {
                        let _ = worker.tx.send(WorkerRequest::Refresh);
                    }
                    KeyAction::None => {}
                }
            }
            handled += 1;
            if handled >= MAX_KEYS_PER_FRAME || !event::poll(Duration::ZERO)? {
                break;
            }
        }
        if quit {
            break;
        }
        flush_requests(app, &worker);
    }
    drop(guard);
    worker.shutdown();
    Ok(())
}

/// State that belongs to the previous worker: its generation numbering, its
/// startup notes, its error, and its refresh time.
pub fn reset_for_new_worker(app: &mut App, ctx: &mut RosterContext) {
    app.reset_generation();
    app.restart_transcript();
    app.live = true;
    ctx.error = None;
    ctx.notes.clear();
    ctx.last_refresh = None;
}

/// Install a panic hook that restores the terminal only for panics on `owner`
/// (the UI thread), then forwards to the previous hook. Panics on other
/// threads, such as caught worker panics, leave the TUI intact.
fn install_panic_hook(owner: ThreadId, restore: fn()) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == owner {
            restore();
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static RESTORES: AtomicUsize = AtomicUsize::new(0);

    fn count() {
        RESTORES.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn panic_hook_restores_only_on_the_owner_thread() {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        install_panic_hook(std::thread::current().id(), count);

        let other = std::thread::spawn(|| catch_unwind(|| panic!("worker")).is_err());
        assert!(other.join().unwrap());
        assert_eq!(RESTORES.load(Ordering::SeqCst), 0);

        assert!(catch_unwind(AssertUnwindSafe(|| panic!("ui"))).is_err());
        assert_eq!(RESTORES.load(Ordering::SeqCst), 1);

        std::panic::set_hook(original);
    }
}
