mod ui;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use fileadmin_domain::{AppState, LoadState, PaneId, parent_or_same};
use fileadmin_fs::{DirectoryScanner, RequestError, ScanRequest};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::error::Error;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::Duration;

type AppResult<T> = Result<T, Box<dyn Error>>;

fn main() -> AppResult<()> {
    install_terminal_panic_hook();

    let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let right = parent_or_same(&current);
    let mut app = AppState::new(current, right);
    let scanner = DirectoryScanner::new();
    queue_initial_scans(&mut app, &scanner);

    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    run(&mut terminal, &mut app, &scanner)
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
    scanner: &DirectoryScanner,
) -> AppResult<()> {
    while !app.should_quit {
        drain_scan_events(app, scanner);
        terminal.draw(|frame| ui::render(frame, app))?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            handle_key(app, scanner, key);
        }
    }
    Ok(())
}

fn queue_initial_scans(app: &mut AppState, scanner: &DirectoryScanner) {
    for pane_id in PaneId::ALL {
        let pane = app.pane(pane_id);
        let request = ScanRequest {
            pane: pane_id,
            generation: pane.generation,
            path: pane.location.clone(),
        };
        if let Err(error) = scanner.request(request) {
            let generation = app.pane(pane_id).generation;
            app.pane_mut(pane_id)
                .apply_error(generation, request_error_message(error));
        }
    }
}

fn drain_scan_events(app: &mut AppState, scanner: &DirectoryScanner) {
    while let Ok(event) = scanner.try_recv() {
        let pane = app.pane_mut(event.pane);
        match event.result {
            Ok(listing) => {
                if pane.apply_entries(event.generation, listing.entries) {
                    pane.truncated = listing.truncated;
                }
            }
            Err(error) => {
                pane.apply_error(event.generation, error.message);
            }
        }
    }
}

fn handle_key(app: &mut AppState, scanner: &DirectoryScanner, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    if app.help_visible {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::F(1)) {
            app.help_visible = false;
        }
        return;
    }

    if app.filter_mode {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => app.filter_mode = false,
            KeyCode::Backspace => {
                let mut filter = app.active().filter.clone();
                filter.pop();
                app.active_mut().set_filter(filter);
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut filter = app.active().filter.clone();
                filter.push(character);
                app.active_mut().set_filter(filter);
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Tab | KeyCode::BackTab => app.switch_pane(),
        KeyCode::Up | KeyCode::Char('k') => app.active_mut().move_cursor(-1),
        KeyCode::Down | KeyCode::Char('j') => app.active_mut().move_cursor(1),
        KeyCode::Home => app.active_mut().cursor = 0,
        KeyCode::End => {
            let last = app.active().visible_len().saturating_sub(1);
            app.active_mut().cursor = last;
        }
        KeyCode::Char(' ') => app.active_mut().toggle_focused_selection(),
        KeyCode::Enter => open_focused_or_retry(app, scanner),
        KeyCode::Backspace => navigate_to(app, scanner, parent_or_same(&app.active().location)),
        KeyCode::Char('/') => {
            app.filter_mode = true;
            app.notice = Some("Type to filter this pane; Enter or Esc closes the filter".into());
        }
        KeyCode::Char('h') => {
            let shown = {
                let pane = app.active_mut();
                pane.show_hidden = !pane.show_hidden;
                pane.cursor = pane.cursor.min(pane.visible_len().saturating_sub(1));
                pane.show_hidden
            };
            app.notice = Some(
                if shown {
                    "Hidden dotfiles shown"
                } else {
                    "Hidden dotfiles concealed"
                }
                .into(),
            );
        }
        KeyCode::Char('s') => {
            app.active_mut().cycle_sort();
            app.notice = Some(format!("Sorted by {}", app.active().sort.label()));
        }
        KeyCode::Char('?') | KeyCode::F(1) => app.help_visible = true,
        KeyCode::Char('c' | 'm' | 'd' | 'r' | 'p') => {
            app.notice = Some("Unavailable in the read-only prototype; no files changed".into());
        }
        _ => {}
    }
}

fn open_focused_or_retry(app: &mut AppState, scanner: &DirectoryScanner) {
    let target = match &app.active().load_state {
        LoadState::Failed(_) => Some(app.active().location.clone()),
        _ => app
            .active()
            .focused()
            .filter(|entry| entry.is_directory())
            .map(|entry| entry.path.clone()),
    };

    if let Some(target) = target {
        navigate_to(app, scanner, target);
    } else if let Some(entry) = app.active().focused() {
        app.notice = Some(format!(
            "Inspecting {} (opening files is not enabled)",
            entry.display_name
        ));
    }
}

fn navigate_to(app: &mut AppState, scanner: &DirectoryScanner, target: PathBuf) {
    if target == app.active().location && target.parent().is_none() {
        app.notice = Some("Already at the filesystem root".into());
        return;
    }

    let pane_id = app.active_pane;
    let generation = app.active_mut().begin_load(target.clone());
    let request = ScanRequest {
        pane: pane_id,
        generation,
        path: target,
    };
    if let Err(error) = scanner.request(request) {
        app.active_mut()
            .apply_error(generation, request_error_message(error));
    }
}

fn request_error_message(error: RequestError) -> String {
    match error {
        RequestError::Busy => "Directory scanner is busy — try again".into(),
        RequestError::Closed => "Directory scanner stopped unexpectedly".into(),
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

fn install_terminal_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
        original(panic_info);
    }));
}
