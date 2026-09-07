mod ui;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use fileadmin_domain::{AppState, EntryKind, FolderSizeState, LoadState, PaneId, parent_or_same};
use fileadmin_domain::{
    JobOutcome, OperationIntent, OperationKind, OperationView, TextAction, TextPrompt,
};
use fileadmin_engine::{OperationEngine, OperationEvent, SubmitError};
use fileadmin_fs::{DirectoryScanner, FolderSizeScanner, RequestError, ScanLocation, ScanRequest};
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
    let folder_sizes = FolderSizeScanner::new();
    let operations = OperationEngine::new();
    queue_initial_scans(&mut app, &scanner);

    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    run(
        &mut terminal,
        &mut app,
        &scanner,
        &folder_sizes,
        &operations,
    )
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
    scanner: &DirectoryScanner,
    folder_sizes: &FolderSizeScanner,
    operations: &OperationEngine,
) -> AppResult<()> {
    while !app.should_quit {
        drain_scan_events(app, scanner);
        drain_operation_events(app, scanner, operations);
        drain_folder_size_events(app, folder_sizes);
        sync_folder_size(app, folder_sizes);
        terminal.draw(|frame| ui::render(frame, app))?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            handle_key(app, scanner, operations, key);
        }
    }
    Ok(())
}

fn drain_folder_size_events(app: &mut AppState, folder_sizes: &FolderSizeScanner) {
    while let Ok(event) = folder_sizes.try_recv() {
        let is_current = matches!(
            &app.folder_size,
            FolderSizeState::Loading {
                request_id,
                pane,
                generation,
                path,
            } if *request_id == event.request_id
                && *pane == event.pane
                && *generation == event.generation
                && *path == event.path
        );
        if !is_current {
            continue;
        }
        app.folder_size = match event.result {
            Ok(summary) => FolderSizeState::Ready(summary),
            Err(message) => FolderSizeState::Failed {
                request_id: event.request_id,
                pane: event.pane,
                generation: event.generation,
                path: event.path,
                message,
            },
        };
    }
}

fn sync_folder_size(app: &mut AppState, folder_sizes: &FolderSizeScanner) {
    let pane = app.active_pane;
    let generation = app.active().generation;
    let path = app
        .active()
        .focused()
        .filter(|entry| entry.kind == EntryKind::Directory)
        .map(|entry| entry.path.clone());

    let Some(path) = path else {
        if !matches!(app.folder_size, FolderSizeState::Idle) {
            folder_sizes.cancel();
            app.folder_size = FolderSizeState::Idle;
        }
        return;
    };
    if app.folder_size.matches(pane, generation, &path) {
        return;
    }

    let request_id = folder_sizes.request(pane, generation, path.clone());
    app.folder_size = FolderSizeState::Loading {
        request_id,
        pane,
        generation,
        path,
    };
}

fn queue_initial_scans(app: &mut AppState, scanner: &DirectoryScanner) {
    for pane_id in PaneId::ALL {
        let pane = app.pane(pane_id);
        let request = ScanRequest {
            pane: pane_id,
            generation: pane.generation,
            location: ScanLocation::Directory(pane.location.clone()),
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

fn handle_key(
    app: &mut AppState,
    scanner: &DirectoryScanner,
    operations: &OperationEngine,
    key: KeyEvent,
) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        request_quit(app, operations);
        return;
    }

    if app.text_prompt.is_some() {
        handle_text_prompt(app, operations, key);
        return;
    }

    if !matches!(app.operation, OperationView::Idle) {
        handle_operation_key(app, operations, key);
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
        KeyCode::Char('q') => request_quit(app, operations),
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
        KeyCode::Backspace => navigate_parent(app, scanner),
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
        KeyCode::Char('c') => submit_transfer(app, operations, OperationKind::Copy),
        KeyCode::Char('m') => submit_transfer(app, operations, OperationKind::Move),
        KeyCode::Char('d') | KeyCode::Delete => submit_recycle(app, operations),
        KeyCode::Char('r') | KeyCode::F(2) => begin_rename(app),
        KeyCode::Char('n') => begin_create_directory(app),
        _ => {}
    }
}

fn drain_operation_events(
    app: &mut AppState,
    scanner: &DirectoryScanner,
    operations: &OperationEngine,
) {
    while let Ok(event) = operations.try_recv() {
        match event {
            OperationEvent::Planning { job, kind } => {
                app.operation = OperationView::Planning { job, kind };
            }
            OperationEvent::PlanReady(summary) => {
                app.operation = OperationView::Review(summary);
            }
            OperationEvent::Progress(progress) => {
                app.operation = OperationView::Running(progress);
            }
            OperationEvent::Finished(report) => {
                let outcome = report.outcome;
                let kind = report.kind;
                refresh_affected_panes(app, scanner, &report.affected_directories);
                app.notice = Some(format!(
                    "{} {}: {} of {} items",
                    outcome_label(outcome),
                    kind.label(),
                    report.completed_items,
                    report.total_items
                ));
                app.operation = OperationView::Finished(report);
            }
            OperationEvent::Failed { job, kind, message } => {
                app.operation = OperationView::Error { job, kind, message };
            }
        }
    }
}

fn handle_operation_key(app: &mut AppState, operations: &OperationEngine, key: KeyEvent) {
    match &app.operation {
        OperationView::Planning { .. } => {
            if key.code == KeyCode::Esc {
                operations.cancel();
                app.notice = Some("Cancellation requested while planning".into());
            }
        }
        OperationView::Review(summary) => match key.code {
            KeyCode::Enter => {
                if let Err(error) = operations.approve(summary.job) {
                    app.notice = Some(submit_error_message(error));
                }
            }
            KeyCode::Esc => {
                let job = summary.job;
                if operations.abandon(job).is_ok() {
                    app.operation = OperationView::Idle;
                    app.notice = Some("Operation cancelled; no files changed".into());
                }
            }
            _ => {}
        },
        OperationView::Running(progress) => {
            if matches!(
                key.code,
                KeyCode::Char('x') | KeyCode::Char('c') | KeyCode::Esc
            ) {
                operations.cancel();
                let mut progress = progress.clone();
                progress.phase = fileadmin_domain::JobPhase::Cancelling;
                app.operation = OperationView::Running(progress);
                app.notice = Some("Cancellation requested; finishing the current safe step".into());
            }
        }
        OperationView::Finished(_) | OperationView::Error { .. } => {
            if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                app.operation = OperationView::Idle;
            }
        }
        OperationView::Idle => {}
    }
}

fn handle_text_prompt(app: &mut AppState, operations: &OperationEngine, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.text_prompt = None,
        KeyCode::Backspace => {
            if let Some(prompt) = &mut app.text_prompt {
                prompt.value.pop();
                prompt.error = None;
            }
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if let Some(prompt) = &mut app.text_prompt {
                prompt.value.push(character);
                prompt.error = None;
            }
        }
        KeyCode::Enter => {
            let Some(prompt) = app.text_prompt.clone() else {
                return;
            };
            if prompt.value.trim().is_empty() {
                if let Some(prompt) = &mut app.text_prompt {
                    prompt.error = Some("A name is required".into());
                }
                return;
            }
            let kind = match &prompt.action {
                TextAction::Rename { .. } => OperationKind::Rename,
                TextAction::CreateDirectory { .. } => OperationKind::CreateDirectory,
            };
            let intent = match prompt.action {
                TextAction::Rename { source } => OperationIntent::Rename {
                    source,
                    new_name: prompt.value.into(),
                },
                TextAction::CreateDirectory { parent } => OperationIntent::CreateDirectory {
                    parent,
                    name: prompt.value.into(),
                },
            };
            match operations.submit(intent) {
                Ok(job) => {
                    app.operation = OperationView::Planning { job, kind };
                    app.text_prompt = None;
                }
                Err(error) => {
                    if let Some(prompt) = &mut app.text_prompt {
                        prompt.error = Some(submit_error_message(error));
                    }
                }
            }
        }
        _ => {}
    }
}

fn submit_transfer(app: &mut AppState, operations: &OperationEngine, kind: OperationKind) {
    let sources = app.operation_sources();
    if sources.is_empty() {
        app.notice = Some("Select or focus a file or directory first".into());
        return;
    }
    let destination_pane = app.pane(app.active_pane.other());
    if destination_pane.browsing_drives || !matches!(destination_pane.load_state, LoadState::Ready)
    {
        app.notice = Some("Open a destination directory in the other pane first".into());
        return;
    }
    let destination = destination_pane.location.clone();
    let intent = match kind {
        OperationKind::Copy => OperationIntent::Copy {
            sources,
            destination,
        },
        OperationKind::Move => OperationIntent::Move {
            sources,
            destination,
        },
        _ => return,
    };
    submit_intent(app, operations, intent);
}

fn submit_recycle(app: &mut AppState, operations: &OperationEngine) {
    let sources = app.operation_sources();
    if sources.is_empty() {
        app.notice = Some("Select or focus a file or directory first".into());
        return;
    }
    submit_intent(app, operations, OperationIntent::Recycle { sources });
}

fn submit_intent(app: &mut AppState, operations: &OperationEngine, intent: OperationIntent) {
    let kind = intent.kind();
    match operations.submit(intent) {
        Ok(job) => app.operation = OperationView::Planning { job, kind },
        Err(error) => app.notice = Some(submit_error_message(error)),
    }
}

fn begin_rename(app: &mut AppState) {
    let Some(entry) = app
        .active()
        .focused()
        .filter(|entry| !entry.is_parent() && !entry.is_drive())
    else {
        app.notice = Some("Focus a file or directory to rename".into());
        return;
    };
    app.text_prompt = Some(TextPrompt {
        action: TextAction::Rename {
            source: entry.path.clone(),
        },
        value: entry.display_name.clone(),
        error: None,
    });
}

fn begin_create_directory(app: &mut AppState) {
    if app.active().browsing_drives || !matches!(app.active().load_state, LoadState::Ready) {
        app.notice = Some("Open a directory before creating a folder".into());
        return;
    }
    app.text_prompt = Some(TextPrompt {
        action: TextAction::CreateDirectory {
            parent: app.active().location.clone(),
        },
        value: String::new(),
        error: None,
    });
}

fn refresh_affected_panes(app: &mut AppState, scanner: &DirectoryScanner, affected: &[PathBuf]) {
    for pane_id in PaneId::ALL {
        let pane = app.pane(pane_id);
        if pane.browsing_drives {
            continue;
        }
        if affected.iter().any(|path| path == &pane.location) {
            let target = pane.location.clone();
            let generation = app.pane_mut(pane_id).begin_load(target.clone());
            let _ = scanner.request(ScanRequest {
                pane: pane_id,
                generation,
                location: ScanLocation::Directory(target),
            });
        }
    }
}

fn request_quit(app: &mut AppState, operations: &OperationEngine) {
    if operations.is_busy() || app.operation.is_busy() {
        operations.cancel();
        app.notice = Some("An operation is active; cancellation requested before exit".into());
    } else {
        app.should_quit = true;
    }
}

fn submit_error_message(error: SubmitError) -> String {
    match error {
        SubmitError::Busy => "Another operation is already active".into(),
        SubmitError::Closed => "The operation engine is unavailable".into(),
    }
}

fn outcome_label(outcome: JobOutcome) -> &'static str {
    match outcome {
        JobOutcome::Completed => "Completed",
        JobOutcome::Partial => "Partially completed",
        JobOutcome::Cancelled => "Cancelled",
        JobOutcome::Failed => "Failed",
    }
}

fn open_focused_or_retry(app: &mut AppState, scanner: &DirectoryScanner) {
    let target = match &app.active().load_state {
        LoadState::Failed(_) if app.active().browsing_drives => {
            navigate_to_drives(app, scanner);
            return;
        }
        LoadState::Failed(_) => Some(app.active().location.clone()),
        _ => app
            .active()
            .focused()
            .filter(|entry| entry.is_directory())
            .map(|entry| entry.path.clone()),
    };

    if let Some(target) = target {
        if target.as_os_str().is_empty()
            && app
                .active()
                .focused()
                .is_some_and(|entry| entry.is_parent())
        {
            navigate_to_drives(app, scanner);
        } else {
            navigate_to(app, scanner, target);
        }
    } else if let Some(entry) = app.active().focused() {
        app.notice = Some(format!(
            "Inspecting {} (opening files is not enabled)",
            entry.display_name
        ));
    }
}

fn navigate_to(app: &mut AppState, scanner: &DirectoryScanner, target: PathBuf) {
    let pane_id = app.active_pane;
    let generation = app.active_mut().begin_load(target.clone());
    let request = ScanRequest {
        pane: pane_id,
        generation,
        location: ScanLocation::Directory(target),
    };
    if let Err(error) = scanner.request(request) {
        app.active_mut()
            .apply_error(generation, request_error_message(error));
    }
}

fn navigate_parent(app: &mut AppState, scanner: &DirectoryScanner) {
    if app.active().browsing_drives {
        app.notice = Some("Already viewing all available drives".into());
        return;
    }
    let current = app.active().location.clone();
    let parent = parent_or_same(&current);
    if parent == current {
        #[cfg(windows)]
        navigate_to_drives(app, scanner);
        #[cfg(not(windows))]
        {
            app.notice = Some("Already at the filesystem root".into());
        }
    } else {
        navigate_to(app, scanner, parent);
    }
}

fn navigate_to_drives(app: &mut AppState, scanner: &DirectoryScanner) {
    let pane_id = app.active_pane;
    let generation = app.active_mut().begin_drive_list();
    let request = ScanRequest {
        pane: pane_id,
        generation,
        location: ScanLocation::Drives,
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
