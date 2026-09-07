mod ui;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use fileadmin_domain::{
    AppState, EntryKind, FolderSizeProgress, FolderSizeState, LoadState, PaneId, PreviewMatch,
    PreviewMode, PreviewRegion, PreviewSearchMode, PreviewSession, PreviewState,
    PreviewWindowDirection, parent_or_same,
};
use fileadmin_domain::{
    JobOutcome, OperationIntent, OperationKind, OperationView, TextAction, TextPrompt,
};
use fileadmin_engine::{OperationEngine, OperationEvent, SubmitError};
use fileadmin_fs::{
    DirectoryScanner, FolderSizeScanner, FolderSizeUpdate, PreviewLoader, PreviewWindowTarget,
    RequestError, ScanLocation, ScanRequest,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use regex::RegexBuilder;
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
    let previews = PreviewLoader::new();
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
        &previews,
        &operations,
    )
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
    scanner: &DirectoryScanner,
    folder_sizes: &FolderSizeScanner,
    previews: &PreviewLoader,
    operations: &OperationEngine,
) -> AppResult<()> {
    while !app.should_quit {
        drain_scan_events(app, scanner);
        drain_operation_events(app, scanner, operations);
        drain_folder_size_events(app, folder_sizes);
        drain_preview_events(app, previews);
        drain_preview_change_events(app, previews);
        sync_folder_size(app, folder_sizes);
        terminal.draw(|frame| ui::render(frame, app))?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            handle_key(app, scanner, previews, operations, key);
        }
    }
    Ok(())
}

fn drain_folder_size_events(app: &mut AppState, folder_sizes: &FolderSizeScanner) {
    while let Ok(event) = folder_sizes.try_recv() {
        let is_current = matches!(
            &app.folder_size,
            FolderSizeState::Loading(progress)
                if progress.request_id == event.request_id
                    && progress.pane == event.pane
                    && progress.generation == event.generation
                    && progress.path == event.path
        );
        if !is_current {
            continue;
        }
        app.folder_size = match event.update {
            FolderSizeUpdate::Progress(progress) => FolderSizeState::Loading(progress),
            FolderSizeUpdate::Finished(Ok(summary)) => FolderSizeState::Ready(summary),
            FolderSizeUpdate::Finished(Err(message)) => FolderSizeState::Failed {
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
    if !matches!(app.operation, OperationView::Idle) || app.preview.is_open() {
        if !matches!(app.folder_size, FolderSizeState::Idle) {
            folder_sizes.cancel();
            app.folder_size = FolderSizeState::Idle;
        }
        return;
    }

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
    app.folder_size = FolderSizeState::Loading(FolderSizeProgress {
        request_id,
        pane,
        generation,
        path,
        discovered_bytes: 0,
        file_count: 0,
        directory_count: 0,
        skipped_items: 0,
        drive_total_bytes: None,
    });
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
    previews: &PreviewLoader,
    operations: &OperationEngine,
    key: KeyEvent,
) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        request_quit(app, operations);
        return;
    }

    if app.preview.is_open() {
        handle_preview_key(app, previews, key);
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
        KeyCode::Enter => open_focused_or_retry(app, scanner, previews),
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
        KeyCode::Char('p') | KeyCode::Char('P') => begin_preview(app, previews),
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

fn open_focused_or_retry(app: &mut AppState, scanner: &DirectoryScanner, previews: &PreviewLoader) {
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
        let path = entry.path.clone();
        begin_preview_path(app, previews, path);
    }
}

fn begin_preview(app: &mut AppState, previews: &PreviewLoader) {
    let Some(path) = app
        .active()
        .focused()
        .filter(|entry| !entry.is_parent() && !entry.is_drive() && !entry.is_directory())
        .map(|entry| entry.path.clone())
    else {
        app.notice = Some("Focus a file to preview".into());
        return;
    };
    begin_preview_path(app, previews, path);
}

fn begin_preview_path(app: &mut AppState, previews: &PreviewLoader, path: PathBuf) {
    previews.clear_watch();
    let request_id = previews.request(path.clone());
    app.preview = PreviewState::Loading { request_id, path };
}

fn drain_preview_events(app: &mut AppState, previews: &PreviewLoader) {
    while let Ok(event) = previews.try_recv() {
        let current = matches!(
            &app.preview,
            PreviewState::Loading { request_id, path }
                if *request_id == event.request_id && *path == event.path
        ) || matches!(
            &app.preview,
            PreviewState::LoadingWindow { request_id, path, .. }
                if *request_id == event.request_id && *path == event.path
        );
        if !current {
            continue;
        }
        let previous = std::mem::replace(&mut app.preview, PreviewState::Closed);
        app.preview = match (previous, event.result) {
            (
                PreviewState::LoadingWindow {
                    direction, session, ..
                },
                Ok(document),
            ) => {
                previews.watch(&document);
                PreviewState::Ready(Box::new(session_for_new_window(
                    *session, document, direction,
                )))
            }
            (PreviewState::LoadingWindow { mut session, .. }, Err(message)) => {
                if message.starts_with("File changed on disk") {
                    session.source_changed = true;
                    session.notice = Some(message);
                } else {
                    previews.watch(&session.document);
                    session.notice = Some(format!("Could not load adjacent window — {message}"));
                }
                PreviewState::Ready(session)
            }
            (_, Ok(document)) => {
                previews.watch(&document);
                PreviewState::Ready(Box::new(PreviewSession::new(document)))
            }
            (_, Err(message)) => PreviewState::Failed {
                request_id: event.request_id,
                path: event.path,
                message,
            },
        };
    }
}

fn drain_preview_change_events(app: &mut AppState, previews: &PreviewLoader) {
    while let Ok(event) = previews.try_recv_change() {
        match &mut app.preview {
            PreviewState::Ready(session) | PreviewState::LoadingWindow { session, .. }
                if session.document.path == event.path =>
            {
                session.source_changed = true;
                session.notice = Some("File changed on disk · r Reload".into());
            }
            _ => {}
        }
    }
}

fn session_for_new_window(
    previous: PreviewSession,
    document: fileadmin_domain::PreviewDocument,
    direction: PreviewWindowDirection,
) -> PreviewSession {
    let mut session = PreviewSession::new(document);
    session.wrap = previous.wrap;
    session.horizontal_scroll = previous.horizontal_scroll;
    session.search.query = previous.search.query;
    session.search.mode = previous.search.mode;
    session.search.case_sensitive = previous.search.case_sensitive;
    session.raw_scroll = if matches!(
        direction,
        PreviewWindowDirection::Previous | PreviewWindowDirection::Last
    ) {
        session.document.raw_lines.len().saturating_sub(20)
    } else {
        0
    };
    recompute_preview_search(&mut session);
    session
}

fn begin_preview_window(
    app: &mut AppState,
    previews: &PreviewLoader,
    direction: PreviewWindowDirection,
) {
    let previous = std::mem::replace(&mut app.preview, PreviewState::Closed);
    let PreviewState::Ready(mut session) = previous else {
        app.preview = previous;
        return;
    };
    let target = match direction {
        PreviewWindowDirection::Previous => {
            PreviewWindowTarget::EndingAt(session.document.window_start)
        }
        PreviewWindowDirection::Next => {
            PreviewWindowTarget::StartingAt(session.document.window_end)
        }
        PreviewWindowDirection::First => PreviewWindowTarget::StartingAt(0),
        PreviewWindowDirection::Last => PreviewWindowTarget::EndingAt(session.document.file_size),
    };
    let path = session.document.path.clone();
    session.notice = Some(
        match direction {
            PreviewWindowDirection::Previous => "Loading previous window…",
            PreviewWindowDirection::Next => "Loading next window…",
            PreviewWindowDirection::First => "Loading first window…",
            PreviewWindowDirection::Last => "Loading last window…",
        }
        .into(),
    );
    previews.clear_watch();
    let request_id = previews.request_window(&session.document, target);
    app.preview = PreviewState::LoadingWindow {
        request_id,
        path,
        direction,
        session,
    };
}

fn preview_window_command(
    session: &PreviewSession,
    key: KeyEvent,
) -> Option<PreviewWindowDirection> {
    if session.help_visible || session.search.editing {
        return None;
    }
    let scroll = if session.mode == PreviewMode::Raw || session.active_region == PreviewRegion::Raw
    {
        session.raw_scroll
    } else {
        session.formatted_scroll
    };
    let last = session.active_line_count().saturating_sub(1);
    match key.code {
        KeyCode::Char('[') if session.has_previous_window() => {
            Some(PreviewWindowDirection::Previous)
        }
        KeyCode::Char(']') if session.has_next_window() => Some(PreviewWindowDirection::Next),
        KeyCode::Up | KeyCode::Char('k') if scroll == 0 && session.has_previous_window() => {
            Some(PreviewWindowDirection::Previous)
        }
        KeyCode::Down | KeyCode::Char('j') if scroll >= last && session.has_next_window() => {
            Some(PreviewWindowDirection::Next)
        }
        KeyCode::PageUp if scroll == 0 && session.has_previous_window() => {
            Some(PreviewWindowDirection::Previous)
        }
        KeyCode::PageDown if scroll.saturating_add(20) >= last && session.has_next_window() => {
            Some(PreviewWindowDirection::Next)
        }
        KeyCode::Char('g') if session.has_previous_window() => Some(PreviewWindowDirection::First),
        KeyCode::Char('G') if session.has_next_window() => Some(PreviewWindowDirection::Last),
        _ => None,
    }
}

fn handle_preview_key(app: &mut AppState, previews: &PreviewLoader, key: KeyEvent) {
    if let PreviewState::Ready(session) = &app.preview
        && let Some(direction) = preview_window_command(session, key)
    {
        begin_preview_window(app, previews, direction);
        return;
    }
    match &mut app.preview {
        PreviewState::Closed => {}
        PreviewState::Loading { .. } => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                previews.cancel();
                app.preview = PreviewState::Closed;
            }
        }
        PreviewState::LoadingWindow { .. } => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                previews.cancel();
                app.preview = PreviewState::Closed;
            }
        }
        PreviewState::Failed { path, .. } => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                previews.cancel();
                app.preview = PreviewState::Closed;
            }
            KeyCode::Char('r') => {
                let path = path.clone();
                let request_id = previews.request(path.clone());
                app.preview = PreviewState::Loading { request_id, path };
            }
            _ => {}
        },
        PreviewState::Ready(session) => {
            if session.help_visible {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::F(1)
                ) {
                    session.help_visible = false;
                }
                return;
            }
            if session.search.editing {
                handle_preview_search_key(session, key);
                return;
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    previews.cancel();
                    app.preview = PreviewState::Closed;
                }
                KeyCode::Char('/') => {
                    session.search.editing = true;
                    recompute_preview_search(session);
                }
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    session.search.editing = true;
                    recompute_preview_search(session);
                }
                KeyCode::Char('n') | KeyCode::F(3)
                    if !key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    move_preview_match(session, 1)
                }
                KeyCode::Char('N') | KeyCode::F(3) => move_preview_match(session, -1),
                KeyCode::Up | KeyCode::Char('k') => scroll_preview(session, -1),
                KeyCode::Down | KeyCode::Char('j') => scroll_preview(session, 1),
                KeyCode::PageUp => scroll_preview(session, -20),
                KeyCode::PageDown => scroll_preview(session, 20),
                KeyCode::Home | KeyCode::Char('g') => *session.active_scroll_mut() = 0,
                KeyCode::End | KeyCode::Char('G') => {
                    let last = session.active_line_count().saturating_sub(1);
                    *session.active_scroll_mut() = last;
                }
                KeyCode::Left if !session.wrap => {
                    session.horizontal_scroll = session.horizontal_scroll.saturating_sub(4)
                }
                KeyCode::Right if !session.wrap => {
                    session.horizontal_scroll = session.horizontal_scroll.saturating_add(4)
                }
                KeyCode::Char('w') => session.wrap = !session.wrap,
                KeyCode::Tab if session.mode == PreviewMode::Split => {
                    session.active_region = match session.active_region {
                        PreviewRegion::Raw => PreviewRegion::Formatted,
                        PreviewRegion::Formatted => PreviewRegion::Raw,
                    };
                    session.search.matches.clear();
                    session.search.current = None;
                    recompute_preview_search(session);
                }
                KeyCode::Char('1') => set_preview_mode(session, PreviewMode::Raw),
                KeyCode::Char('2') => set_preview_mode(session, PreviewMode::Split),
                KeyCode::Char('3') => set_preview_mode(session, PreviewMode::Formatted),
                KeyCode::Char('v') => cycle_preview_mode(session),
                KeyCode::Char('r') => {
                    let path = session.document.path.clone();
                    previews.clear_watch();
                    let request_id = previews.request(path.clone());
                    app.preview = PreviewState::Loading { request_id, path };
                }
                KeyCode::Char('?') | KeyCode::F(1) => session.help_visible = true,
                _ => {}
            }
        }
    }
}

fn handle_preview_search_key(session: &mut PreviewSession, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => session.search.editing = false,
        KeyCode::Enter => {
            session.search.editing = false;
            if session.search.current.is_none() && !session.search.matches.is_empty() {
                session.search.current = Some(0);
                jump_to_current_match(session);
            }
        }
        KeyCode::Backspace => {
            session.search.query.pop();
            recompute_preview_search(session);
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            session.search.mode = match session.search.mode {
                PreviewSearchMode::Literal => PreviewSearchMode::Regex,
                PreviewSearchMode::Regex => PreviewSearchMode::Literal,
            };
            recompute_preview_search(session);
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::ALT) => {
            session.search.case_sensitive = !session.search.case_sensitive;
            recompute_preview_search(session);
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            session.search.query.push(character);
            recompute_preview_search(session);
        }
        _ => {}
    }
}

fn set_preview_mode(session: &mut PreviewSession, mode: PreviewMode) {
    let supported = match mode {
        PreviewMode::Raw => true,
        PreviewMode::Split => {
            session.document.kind == fileadmin_domain::PreviewKind::Markdown
                && session.document.formatted_lines.is_some()
        }
        PreviewMode::Formatted => session.document.formatted_lines.is_some(),
    };
    if supported {
        session.mode = mode;
        session.active_region = if mode == PreviewMode::Raw {
            PreviewRegion::Raw
        } else {
            PreviewRegion::Formatted
        };
        session.search.matches.clear();
        session.search.current = None;
        recompute_preview_search(session);
    } else {
        session.notice = Some("That representation is unavailable for this file".into());
    }
}

fn cycle_preview_mode(session: &mut PreviewSession) {
    let next = match (session.document.kind, session.mode) {
        (fileadmin_domain::PreviewKind::Markdown, PreviewMode::Raw) => PreviewMode::Split,
        (fileadmin_domain::PreviewKind::Markdown, PreviewMode::Split) => PreviewMode::Formatted,
        (fileadmin_domain::PreviewKind::Markdown, PreviewMode::Formatted) => PreviewMode::Raw,
        (fileadmin_domain::PreviewKind::Json, PreviewMode::Raw) => PreviewMode::Formatted,
        (fileadmin_domain::PreviewKind::Json, _) => PreviewMode::Raw,
        _ => PreviewMode::Raw,
    };
    set_preview_mode(session, next);
}

fn scroll_preview(session: &mut PreviewSession, delta: isize) {
    let last = session.active_line_count().saturating_sub(1);
    let scroll = session.active_scroll_mut();
    *scroll = scroll.saturating_add_signed(delta).min(last);
}

fn recompute_preview_search(session: &mut PreviewSession) {
    const MAX_PATTERN_BYTES: usize = 1024;
    const MAX_MATCHES: usize = 10_000;
    let query = session.search.query.clone();
    if query.is_empty() {
        session.search.matches.clear();
        session.search.current = None;
        session.search.error = None;
        return;
    }
    if query.len() > MAX_PATTERN_BYTES {
        session.search.error = Some("Search pattern is limited to 1,024 bytes".into());
        return;
    }
    let pattern = match session.search.mode {
        PreviewSearchMode::Literal => regex::escape(&query),
        PreviewSearchMode::Regex => query,
    };
    let expression = match RegexBuilder::new(&pattern)
        .case_insensitive(!session.search.case_sensitive)
        .multi_line(false)
        .size_limit(2 * 1024 * 1024)
        .dfa_size_limit(2 * 1024 * 1024)
        .build()
    {
        Ok(expression) => expression,
        Err(error) => {
            session.search.error = Some(format!("Invalid search: {error}"));
            return;
        }
    };
    let lines = session.active_lines();
    let mut matches = Vec::new();
    'lines: for (line, text) in lines.into_iter().enumerate() {
        for found in expression.find_iter(text) {
            matches.push(PreviewMatch {
                line,
                start: found.start(),
                end: found.end(),
            });
            if matches.len() == MAX_MATCHES {
                session.notice = Some("Search stopped at the 10,000-match safety limit".into());
                break 'lines;
            }
        }
    }
    session.search.matches = matches;
    session.search.current = (!session.search.matches.is_empty()).then_some(0);
    session.search.error = None;
    jump_to_current_match(session);
}

fn move_preview_match(session: &mut PreviewSession, delta: isize) {
    if session.search.matches.is_empty() {
        session.notice = Some("No search matches".into());
        return;
    }
    let length = session.search.matches.len();
    let current = session.search.current.unwrap_or(0);
    let wrapped;
    let next = if delta < 0 {
        if current == 0 {
            wrapped = true;
            length - 1
        } else {
            wrapped = false;
            current - 1
        }
    } else {
        wrapped = current + 1 >= length;
        (current + 1) % length
    };
    session.search.current = Some(next);
    session.notice = wrapped.then(|| {
        if delta < 0 {
            "Wrapped to end".into()
        } else {
            "Wrapped to start".into()
        }
    });
    jump_to_current_match(session);
}

fn jump_to_current_match(session: &mut PreviewSession) {
    let Some(index) = session.search.current else {
        return;
    };
    let Some(found) = session.search.matches.get(index) else {
        return;
    };
    let line = found.line;
    *session.active_scroll_mut() = line.saturating_sub(2);
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

#[cfg(test)]
mod preview_tests {
    use super::*;
    use fileadmin_domain::{
        FileEntry, PreviewCompleteness, PreviewDocument, PreviewEncoding, PreviewKind, PreviewLine,
        PreviewLineStyle,
    };

    fn app_with_focused_file() -> AppState {
        let root = PathBuf::from(r"C:\preview-route-test");
        let mut app = AppState::new(root.clone(), root.clone());
        let generation = app.active().generation;
        app.active_mut().apply_entries(
            generation,
            vec![FileEntry {
                path: root.join("Latest.log"),
                display_name: "Latest.log".into(),
                kind: EntryKind::File,
                size: Some(20),
                modified: None,
                metadata_incomplete: false,
                drive_info: None,
            }],
        );
        app
    }

    fn session(lines: &[&str]) -> PreviewSession {
        PreviewSession::new(PreviewDocument {
            request_id: 1,
            path: PathBuf::from("test.log"),
            file_size: 20,
            modified: None,
            kind: PreviewKind::Log,
            encoding: PreviewEncoding::Utf8,
            completeness: PreviewCompleteness::Complete,
            window_start: 0,
            window_end: 20,
            raw_lines: lines.iter().map(|line| (*line).to_owned()).collect(),
            formatted_lines: None,
            format_error: None,
        })
    }

    #[test]
    fn literal_search_is_case_insensitive_and_navigable() {
        let mut session = session(&["Error one", "ok", "ERROR two"]);
        session.search.query = "error".into();

        recompute_preview_search(&mut session);
        assert_eq!(session.search.matches.len(), 2);
        assert_eq!(session.search.current, Some(0));

        move_preview_match(&mut session, 1);
        assert_eq!(session.search.current, Some(1));
        assert_eq!(session.raw_scroll, 0);
    }

    #[test]
    fn regex_search_reports_invalid_patterns_without_losing_valid_results() {
        let mut session = session(&["item-12", "item-xx"]);
        session.search.mode = PreviewSearchMode::Regex;
        session.search.query = r"item-\d+".into();
        recompute_preview_search(&mut session);
        assert_eq!(session.search.matches.len(), 1);

        session.search.query = "[".into();
        recompute_preview_search(&mut session);
        assert!(session.search.error.is_some());
        assert_eq!(session.search.matches.len(), 1);
    }

    #[test]
    fn markdown_modes_require_a_formatted_representation() {
        let mut session = session(&["plain"]);
        set_preview_mode(&mut session, PreviewMode::Split);
        assert_eq!(session.mode, PreviewMode::Raw);

        session.document.kind = PreviewKind::Markdown;
        session.document.formatted_lines = Some(vec![PreviewLine {
            text: "plain".into(),
            style: PreviewLineStyle::Normal,
        }]);
        set_preview_mode(&mut session, PreviewMode::Split);
        assert_eq!(session.mode, PreviewMode::Split);
    }

    #[test]
    fn p_uppercase_p_and_enter_route_a_focused_file_to_preview() {
        let scanner = DirectoryScanner::new();
        let previews = PreviewLoader::new();
        let operations = OperationEngine::new();
        let mut app = app_with_focused_file();

        handle_key(
            &mut app,
            &scanner,
            &previews,
            &operations,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
        );
        assert!(matches!(app.preview, PreviewState::Loading { .. }));

        previews.cancel();
        app.preview = PreviewState::Closed;
        handle_key(
            &mut app,
            &scanner,
            &previews,
            &operations,
            KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT),
        );
        assert!(matches!(app.preview, PreviewState::Loading { .. }));

        previews.cancel();
        app.preview = PreviewState::Closed;
        handle_key(
            &mut app,
            &scanner,
            &previews,
            &operations,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(matches!(app.preview, PreviewState::Loading { .. }));
    }

    #[test]
    fn search_navigation_announces_wrapping() {
        let mut session = session(&["match", "match"]);
        session.search.query = "match".into();
        recompute_preview_search(&mut session);

        move_preview_match(&mut session, -1);

        assert_eq!(session.search.current, Some(1));
        assert_eq!(session.notice.as_deref(), Some("Wrapped to end"));
    }

    #[test]
    fn preview_edges_request_adjacent_and_file_end_windows() {
        let mut session = session(&["first", "second"]);
        session.document.file_size = 4 * 1024 * 1024;
        session.document.window_start = 1024 * 1024;
        session.document.window_end = 2 * 1024 * 1024;
        session.document.completeness = PreviewCompleteness::MiddleWindow;

        assert_eq!(
            preview_window_command(&session, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Some(PreviewWindowDirection::Previous)
        );
        assert_eq!(
            preview_window_command(
                &session,
                KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE)
            ),
            Some(PreviewWindowDirection::First)
        );

        session.raw_scroll = 1;
        assert_eq!(
            preview_window_command(&session, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Some(PreviewWindowDirection::Next)
        );
        assert_eq!(
            preview_window_command(
                &session,
                KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)
            ),
            Some(PreviewWindowDirection::Last)
        );
    }

    #[test]
    fn loading_a_new_window_preserves_find_preferences() {
        let mut previous = session(&["old"]);
        previous.search.query = "needle".into();
        previous.search.mode = PreviewSearchMode::Regex;
        previous.search.case_sensitive = true;
        previous.wrap = true;
        let mut document = previous.document.clone();
        document.window_start = 1024;
        document.window_end = 2048;
        document.file_size = 4096;
        document.completeness = PreviewCompleteness::MiddleWindow;
        document.raw_lines = vec!["needle".into()];

        let loaded = session_for_new_window(previous, document, PreviewWindowDirection::Next);

        assert_eq!(loaded.search.query, "needle");
        assert_eq!(loaded.search.mode, PreviewSearchMode::Regex);
        assert!(loaded.search.case_sensitive);
        assert!(loaded.wrap);
        assert_eq!(loaded.search.matches.len(), 1);
    }
}
