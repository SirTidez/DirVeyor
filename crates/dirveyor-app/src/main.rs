mod ui;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use dirveyor_domain::{
    AppState, DeleteMode, EntryKind, FavoritesPanel, FileEntry, FolderSizeProgress,
    FolderSizeState, LoadState, PaneId, PreviewMatch, PreviewMode, PreviewRegion,
    PreviewSearchMode, PreviewSession, PreviewState, PreviewWindowDirection, parent_or_same,
    paths_match,
};
use dirveyor_domain::{
    ConflictAction, ConflictKind, ConflictPrompt, JobId, JobOutcome, OperationIntent,
    OperationKind, OperationPlanningProgress, OperationView, TextAction, TextPrompt,
};
use dirveyor_engine::{OperationEngine, OperationEvent, SubmitError};
use dirveyor_fs::{
    DirectoryScanner, FavoritesStore, FolderSizeScanner, FolderSizeUpdate, MAX_FAVORITES,
    PreviewLoader, PreviewWindowTarget, RequestError, ScanLocation, ScanRequest,
    user_home_directory,
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

    let launch = LaunchContext::from_args();
    let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let right = parent_or_same(&current);
    let mut app = AppState::new(
        launch.left.clone().unwrap_or(current),
        launch.right.clone().unwrap_or(right),
    );
    app.active_pane = launch.active;
    let favorites = FavoritesStore::discover();
    app.home_directory = user_home_directory();
    match favorites.load() {
        Ok(paths) => app.favorites = paths,
        Err(error) => app.notice = Some(error),
    }
    let scanner = DirectoryScanner::new();
    let folder_sizes = FolderSizeScanner::new();
    let previews = PreviewLoader::new();
    let operations = OperationEngine::new();
    queue_initial_scans(&mut app, &scanner, &launch);

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
        &favorites,
    )
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
    scanner: &DirectoryScanner,
    folder_sizes: &FolderSizeScanner,
    previews: &PreviewLoader,
    operations: &OperationEngine,
    favorites: &FavoritesStore,
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
            handle_key(app, scanner, previews, operations, favorites, key);
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

#[derive(Debug)]
struct LaunchContext {
    left: Option<PathBuf>,
    right: Option<PathBuf>,
    left_focus: Option<PathBuf>,
    right_focus: Option<PathBuf>,
    active: PaneId,
}

impl LaunchContext {
    fn from_args() -> Self {
        let mut context = Self {
            left: None,
            right: None,
            left_focus: None,
            right_focus: None,
            active: PaneId::Left,
        };
        let mut arguments = std::env::args_os().skip(1);
        while let Some(argument) = arguments.next() {
            if argument == "--resume-left" {
                context.left = arguments.next().map(PathBuf::from);
            } else if argument == "--resume-right" {
                context.right = arguments.next().map(PathBuf::from);
            } else if argument == "--resume-left-focus" {
                context.left_focus = arguments.next().map(PathBuf::from);
            } else if argument == "--resume-right-focus" {
                context.right_focus = arguments.next().map(PathBuf::from);
            } else if argument == "--resume-active" {
                context.active = match arguments.next().as_deref() {
                    Some(value) if value == "right" => PaneId::Right,
                    _ => PaneId::Left,
                };
            }
        }
        context
    }
}

fn queue_initial_scans(app: &mut AppState, scanner: &DirectoryScanner, launch: &LaunchContext) {
    for pane_id in PaneId::ALL {
        let resumed = match pane_id {
            PaneId::Left => launch.left.clone(),
            PaneId::Right => launch.right.clone(),
        };
        let focus = match pane_id {
            PaneId::Left => launch.left_focus.clone(),
            PaneId::Right => launch.right_focus.clone(),
        };
        let (generation, location) = if let Some(path) = resumed {
            let generation = app
                .pane_mut(pane_id)
                .begin_load_restoring_focus(path.clone(), focus);
            (generation, ScanLocation::Directory(path))
        } else {
            let generation = app.pane_mut(pane_id).begin_drive_list();
            (generation, ScanLocation::Drives)
        };
        let request = ScanRequest {
            pane: pane_id,
            generation,
            location,
        };
        if let Err(error) = scanner.request(request) {
            app.pane_mut(pane_id)
                .apply_error(generation, request_error_message(error));
        }
    }
}

fn drain_scan_events(app: &mut AppState, scanner: &DirectoryScanner) {
    while let Ok(event) = scanner.try_recv() {
        let browsing_drives = app.pane(event.pane).browsing_drives;
        match event.result {
            Ok(mut listing) => {
                if browsing_drives {
                    listing.entries = quick_access_entries(app, listing.entries);
                }
                let pane = app.pane_mut(event.pane);
                if pane.apply_entries(event.generation, listing.entries) {
                    pane.truncated = listing.truncated;
                }
            }
            Err(error) => {
                app.pane_mut(event.pane)
                    .apply_error(event.generation, error.message);
            }
        }
    }
}

fn quick_access_entries(app: &AppState, mut drives: Vec<FileEntry>) -> Vec<FileEntry> {
    drives.retain(|entry| entry.kind == EntryKind::Drive);
    if let Some(home) = &app.home_directory {
        drives.push(shortcut_entry(home.clone(), EntryKind::Home));
    }
    for path in &app.favorites {
        if app
            .home_directory
            .as_ref()
            .is_some_and(|home| paths_match(home, path))
        {
            continue;
        }
        drives.push(shortcut_entry(path.clone(), EntryKind::Favorite));
    }
    drives
}

fn shortcut_entry(path: PathBuf, kind: EntryKind) -> FileEntry {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Folder");
    FileEntry {
        display_name: format!("{name}  {}", path.display()),
        path,
        kind,
        size: None,
        modified: None,
        metadata_incomplete: false,
        drive_info: None,
    }
}

fn toggle_favorite(app: &mut AppState, scanner: &DirectoryScanner, store: &FavoritesStore) {
    let Some(entry) = app.active().focused() else {
        app.notice = Some("Focus a folder to add or remove a favorite".into());
        return;
    };
    if !matches!(
        entry.kind,
        EntryKind::Directory | EntryKind::Home | EntryKind::Favorite
    ) {
        app.notice = Some("Only folders can be added to Favorites".into());
        return;
    }
    let path = entry.path.clone();
    let mut updated = app.favorites.clone();
    let existing = updated
        .iter()
        .position(|favorite| paths_match(favorite, &path));
    let notice = if let Some(index) = existing {
        updated.remove(index);
        format!("Removed {} from Favorites", path.display())
    } else {
        if updated.len() >= MAX_FAVORITES {
            app.notice = Some("Favorites are limited to 256 folders".into());
            return;
        }
        updated.push(path.clone());
        format!("Added {} to Favorites", path.display())
    };
    match store.save(&updated) {
        Ok(()) => {
            app.favorites = updated;
            app.notice = Some(notice);
            refresh_drive_shortcut_panes(app, scanner);
        }
        Err(error) => app.notice = Some(error),
    }
}

fn handle_favorites_key(
    app: &mut AppState,
    scanner: &DirectoryScanner,
    store: &FavoritesStore,
    key: KeyEvent,
) {
    let length = app.favorites.len();
    let cursor = app
        .favorites_panel
        .as_ref()
        .map_or(0, |panel| panel.cursor.min(length.saturating_sub(1)));
    if (key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F')))
        || matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q')
        )
    {
        app.favorites_panel = None;
        return;
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
            if let Some(panel) = &mut app.favorites_panel {
                panel.cursor = cursor.saturating_sub(1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
            if let Some(panel) = &mut app.favorites_panel {
                panel.cursor = cursor.saturating_add(1).min(length.saturating_sub(1));
            }
        }
        KeyCode::Home => {
            if let Some(panel) = &mut app.favorites_panel {
                panel.cursor = 0;
            }
        }
        KeyCode::End => {
            if let Some(panel) = &mut app.favorites_panel {
                panel.cursor = length.saturating_sub(1);
            }
        }
        KeyCode::Enter | KeyCode::Right if length > 0 => {
            let path = app.favorites[cursor].clone();
            app.favorites_panel = None;
            navigate_to(app, scanner, path);
        }
        KeyCode::Char('f') | KeyCode::Char('F') | KeyCode::Delete if length > 0 => {
            let mut updated = app.favorites.clone();
            let removed = updated.remove(cursor);
            match store.save(&updated) {
                Ok(()) => {
                    app.favorites = updated;
                    if let Some(panel) = &mut app.favorites_panel {
                        panel.cursor = cursor.min(app.favorites.len().saturating_sub(1));
                    }
                    app.notice = Some(format!("Removed {} from Favorites", removed.display()));
                    refresh_drive_shortcut_panes(app, scanner);
                }
                Err(error) => app.notice = Some(error),
            }
        }
        _ => {}
    }
}

fn refresh_drive_shortcut_panes(app: &mut AppState, scanner: &DirectoryScanner) {
    for pane_id in PaneId::ALL {
        if !app.pane(pane_id).browsing_drives {
            continue;
        }
        let focus = app.pane(pane_id).focused().map(|entry| entry.path.clone());
        let generation = app
            .pane_mut(pane_id)
            .begin_drive_list_restoring_focus(focus);
        if let Err(error) = scanner.request(ScanRequest {
            pane: pane_id,
            generation,
            location: ScanLocation::Drives,
        }) {
            app.pane_mut(pane_id)
                .apply_error(generation, request_error_message(error));
        }
    }
}

fn handle_key(
    app: &mut AppState,
    scanner: &DirectoryScanner,
    previews: &PreviewLoader,
    operations: &OperationEngine,
    favorites: &FavoritesStore,
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

    if app.favorites_panel.is_some() {
        handle_favorites_key(app, scanner, favorites, key);
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
                let mut filter = app.active().filter().to_owned();
                filter.pop();
                app.active_mut().set_filter(filter);
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut filter = app.active().filter().to_owned();
                filter.push(character);
                app.active_mut().set_filter(filter);
            }
            _ => {}
        }
        return;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D'))
    {
        app.delete_mode = app.delete_mode.toggle();
        app.notice = Some(format!(
            "Delete mode: {} · D applies this mode",
            app.delete_mode.label()
        ));
        return;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
    {
        app.transfer_verification = app.transfer_verification.toggle();
        app.notice = Some(format!(
            "Transfer verification: {}",
            app.transfer_verification.label()
        ));
        return;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F'))
    {
        app.favorites_panel = Some(FavoritesPanel::default());
        return;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => request_quit(app, operations),
        KeyCode::Tab | KeyCode::BackTab => app.switch_pane(),
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => app.active_mut().move_cursor(-1),
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => app.active_mut().move_cursor(1),
        KeyCode::Home => app.active_mut().cursor = 0,
        KeyCode::End => {
            let last = app.active().visible_len().saturating_sub(1);
            app.active_mut().cursor = last;
        }
        KeyCode::Char(' ') => app.active_mut().toggle_focused_selection(),
        KeyCode::Enter => open_focused_or_retry(app, scanner, previews),
        KeyCode::Right => open_or_select_focused(app, scanner, previews),
        KeyCode::Left | KeyCode::Backspace => navigate_parent(app, scanner),
        KeyCode::Char('/') => {
            app.filter_mode = true;
            app.notice = Some("Type to filter this pane; Enter or Esc closes the filter".into());
        }
        KeyCode::Char('h') | KeyCode::Char('H') => {
            let shown = {
                let pane = app.active_mut();
                pane.toggle_hidden();
                pane.cursor = pane.cursor.min(pane.visible_len().saturating_sub(1));
                pane.show_hidden()
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
        KeyCode::Char('s') | KeyCode::Char('S') => {
            app.active_mut().cycle_sort();
            app.notice = Some(format!("Sorted by {}", app.active().sort().label()));
        }
        KeyCode::Char('?') | KeyCode::F(1) => app.help_visible = true,
        KeyCode::Char('c') | KeyCode::Char('C') => {
            submit_transfer(app, operations, OperationKind::Copy)
        }
        KeyCode::Char('m') | KeyCode::Char('M') => {
            submit_transfer(app, operations, OperationKind::Move)
        }
        KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Delete => submit_delete(app, operations),
        KeyCode::Char('r') | KeyCode::Char('R') | KeyCode::F(2) => begin_rename(app),
        KeyCode::Char('n') | KeyCode::Char('N') => begin_create_directory(app),
        KeyCode::Char('p') | KeyCode::Char('P') => begin_preview(app, previews),
        KeyCode::Char('f') | KeyCode::Char('F') => toggle_favorite(app, scanner, favorites),
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
            OperationEvent::Planning(progress) => {
                app.operation = OperationView::Planning(progress);
            }
            OperationEvent::PlanReady(summary) => {
                app.operation = OperationView::Review(summary);
            }
            OperationEvent::Progress(progress) => {
                app.operation = OperationView::Running(progress);
            }
            OperationEvent::Conflict(conflict) => {
                app.operation = OperationView::Conflict(ConflictPrompt {
                    conflict,
                    apply_to_all: false,
                });
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
    match app.operation.clone() {
        OperationView::Planning(_) => {
            if key.code == KeyCode::Esc {
                operations.cancel();
                app.notice = Some("Cancellation requested while planning".into());
            }
        }
        OperationView::Review(summary) => match key.code {
            KeyCode::Char(character)
                if summary.kind == OperationKind::PermanentDelete
                    && character.eq_ignore_ascii_case(&'y')
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Err(error) = operations.approve(summary.job) {
                    app.notice = Some(submit_error_message(error));
                }
            }
            KeyCode::Char(character)
                if summary.kind == OperationKind::PermanentDelete
                    && character.eq_ignore_ascii_case(&'n')
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                abandon_review(app, operations, summary.job);
            }
            KeyCode::Enter if summary.kind == OperationKind::PermanentDelete => {
                app.notice = Some("Press Y to permanently delete or N to cancel".into());
            }
            KeyCode::Enter => {
                if let Err(error) = operations.approve(summary.job) {
                    app.notice = Some(submit_error_message(error));
                }
            }
            KeyCode::Esc => {
                abandon_review(app, operations, summary.job);
            }
            _ => {}
        },
        OperationView::Running(progress) => {
            if matches!(
                key.code,
                KeyCode::Char('x')
                    | KeyCode::Char('X')
                    | KeyCode::Char('c')
                    | KeyCode::Char('C')
                    | KeyCode::Esc
            ) {
                operations.cancel();
                let mut progress = progress.clone();
                progress.phase = dirveyor_domain::JobPhase::Cancelling;
                app.operation = OperationView::Running(progress);
                app.notice = Some("Cancellation requested; finishing the current safe step".into());
            }
        }
        OperationView::Conflict(mut prompt) => {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('x') | KeyCode::Char('X')
            ) {
                operations.cancel();
                app.notice = Some("Cancellation requested at conflict".into());
                return;
            }
            if matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A')) {
                prompt.apply_to_all = !prompt.apply_to_all;
                app.operation = OperationView::Conflict(prompt);
                return;
            }
            let action = match key.code {
                KeyCode::Char('1') => Some(ConflictAction::KeepNewer),
                KeyCode::Char('2') => Some(ConflictAction::KeepOlder),
                KeyCode::Char('3') => Some(ConflictAction::KeepSource),
                KeyCode::Char('4') => Some(ConflictAction::KeepDestination),
                KeyCode::Char('5') => Some(ConflictAction::KeepBoth),
                KeyCode::Char('6') => Some(ConflictAction::Skip),
                _ => None,
            };
            let Some(action) = action else {
                return;
            };
            if prompt.conflict.kind == ConflictKind::TypeMismatch
                && !matches!(
                    action,
                    ConflictAction::KeepDestination
                        | ConflictAction::KeepBoth
                        | ConflictAction::Skip
                )
            {
                app.notice =
                    Some("Type conflicts allow 4 Keep destination, 5 Keep both, or 6 Skip".into());
                return;
            }
            if operations
                .resolve_conflict(prompt.conflict.job, action, prompt.apply_to_all)
                .is_err()
            {
                app.notice = Some("Could not submit conflict choice".into());
            }
        }
        OperationView::Finished(report) => {
            if is_elevation_shortcut(key)
                && report.kind.is_delete()
                && report
                    .failures
                    .iter()
                    .any(|failure| elevation_available(&failure.message))
            {
                app.notice = Some(match launch_elevated_dirveyor(app) {
                    Ok(()) => {
                        "Opened an elevated DirVeyor at the same locations; approve the UAC prompt"
                            .into()
                    }
                    Err(error) => error,
                });
            } else if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                app.operation = OperationView::Idle;
            }
        }
        OperationView::Error { kind, message, .. } => {
            if is_elevation_shortcut(key) && kind.is_delete() && elevation_available(&message) {
                app.notice = Some(match launch_elevated_dirveyor(app) {
                    Ok(()) => {
                        "Opened an elevated DirVeyor at the same locations; approve the UAC prompt"
                            .into()
                    }
                    Err(error) => error,
                });
            } else if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                app.operation = OperationView::Idle;
            }
        }
        OperationView::Idle => {}
    }
}

fn abandon_review(app: &mut AppState, operations: &OperationEngine, job: JobId) {
    if operations.abandon(job).is_ok() {
        app.operation = OperationView::Idle;
        app.notice = Some("Operation cancelled; no files changed".into());
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
                    app.operation = planning_view(job, kind);
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
    let source_pane = app.transfer_source_pane();
    let sources = app.transfer_sources();
    if sources.is_empty() {
        app.notice = Some("Select or focus a file or directory first".into());
        return;
    }
    let destination_pane = app.pane(source_pane.other());
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
            verification: app.transfer_verification,
        },
        OperationKind::Move => OperationIntent::Move {
            sources,
            destination,
            verification: app.transfer_verification,
        },
        _ => return,
    };
    submit_intent(app, operations, intent);
}

fn submit_delete(app: &mut AppState, operations: &OperationEngine) {
    let sources = app.operation_sources();
    if sources.is_empty() {
        app.notice = Some("Select or focus a file or directory first".into());
        return;
    }
    let intent = match app.delete_mode {
        DeleteMode::Recycle => OperationIntent::Recycle { sources },
        DeleteMode::Permanent => OperationIntent::PermanentDelete { sources },
    };
    submit_intent(app, operations, intent);
}

fn submit_intent(app: &mut AppState, operations: &OperationEngine, intent: OperationIntent) {
    let kind = intent.kind();
    match operations.submit(intent) {
        Ok(job) => app.operation = planning_view(job, kind),
        Err(error) => app.notice = Some(submit_error_message(error)),
    }
}

fn planning_view(job: JobId, kind: OperationKind) -> OperationView {
    OperationView::Planning(OperationPlanningProgress {
        job,
        kind,
        discovered_items: 0,
        discovered_files: 0,
        discovered_directories: 0,
        discovered_bytes: 0,
        current_path: None,
    })
}

fn begin_rename(app: &mut AppState) {
    let Some(entry) = app
        .active()
        .focused()
        .filter(|entry| !entry.is_virtual_location())
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
            let focus = pane.focused().map(|entry| entry.path.clone());
            let generation = app
                .pane_mut(pane_id)
                .begin_load_restoring_focus(target.clone(), focus);
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

fn is_elevation_shortcut(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('e') | KeyCode::Char('E'))
}

fn is_permission_denied_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("access is denied")
        || message.contains("access denied")
        || message.contains("permission denied")
        || message.contains("os error 5")
        || message.contains("os error 13")
}

fn elevation_available(message: &str) -> bool {
    cfg!(windows) && is_permission_denied_message(message) && !is_process_elevated()
}

#[cfg(windows)]
fn is_process_elevated() -> bool {
    use windows_sys::Win32::UI::Shell::IsUserAnAdmin;

    // SAFETY: IsUserAnAdmin takes no pointers and only queries the current
    // process token membership.
    unsafe { IsUserAnAdmin() != 0 }
}

#[cfg(not(windows))]
fn is_process_elevated() -> bool {
    false
}

#[cfg(windows)]
fn launch_elevated_dirveyor(app: &AppState) -> Result<(), String> {
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not locate DirVeyor executable — {error}"))?;
    let mut arguments = Vec::<OsString>::new();
    for pane_id in PaneId::ALL {
        let pane = app.pane(pane_id);
        if pane.browsing_drives {
            continue;
        }
        arguments.push(
            match pane_id {
                PaneId::Left => "--resume-left",
                PaneId::Right => "--resume-right",
            }
            .into(),
        );
        arguments.push(pane.location.as_os_str().to_owned());
        if let Some(focused) = pane.focused() {
            arguments.push(
                match pane_id {
                    PaneId::Left => "--resume-left-focus",
                    PaneId::Right => "--resume-right-focus",
                }
                .into(),
            );
            arguments.push(focused.path.as_os_str().to_owned());
        }
    }
    arguments.push("--resume-active".into());
    arguments.push(
        match app.active_pane {
            PaneId::Left => "left",
            PaneId::Right => "right",
        }
        .into(),
    );

    let verb: Vec<u16> = OsStr::new("runas").encode_wide().chain(Some(0)).collect();
    let executable: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let parameters = windows_command_line(&arguments);
    // SAFETY: all pointers reference valid NUL-terminated UTF-16 buffers for
    // the duration of ShellExecuteW. No window handle or working directory is
    // supplied. The elevated child performs no automatic operation; it merely
    // reopens DirVeyor at the current locations for a fresh reviewed attempt.
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            executable.as_ptr(),
            parameters.as_ptr(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize > 32 {
        Ok(())
    } else {
        Err(format!(
            "Windows did not launch elevated DirVeyor (ShellExecute code {})",
            result as isize
        ))
    }
}

#[cfg(windows)]
fn windows_command_line(arguments: &[std::ffi::OsString]) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    let mut command_line = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            command_line.push(b' ' as u16);
        }
        command_line.push(b'"' as u16);
        let mut backslashes = 0;
        for unit in argument.as_os_str().encode_wide() {
            if unit == b'\\' as u16 {
                backslashes += 1;
            } else if unit == b'"' as u16 {
                command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
                command_line.push(unit);
                backslashes = 0;
            } else {
                command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
                command_line.push(unit);
                backslashes = 0;
            }
        }
        command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
        command_line.push(b'"' as u16);
    }
    command_line.push(0);
    command_line
}

#[cfg(not(windows))]
fn launch_elevated_dirveyor(_app: &AppState) -> Result<(), String> {
    Err("In-app elevation is currently available only on Windows".into())
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
    if app
        .active()
        .focused()
        .is_some_and(|entry| entry.is_parent())
    {
        navigate_parent(app, scanner);
        return;
    }
    let target = match &app.active().load_state {
        LoadState::Failed(_) => {
            retry_active_location(app, scanner);
            return;
        }
        _ => app
            .active()
            .focused()
            .filter(|entry| entry.is_directory())
            .map(|entry| entry.path.clone()),
    };

    if let Some(target) = target {
        navigate_to(app, scanner, target);
    } else if let Some(entry) = app.active().focused() {
        let path = entry.path.clone();
        begin_preview_path(app, previews, path);
    }
}

fn open_or_select_focused(
    app: &mut AppState,
    scanner: &DirectoryScanner,
    previews: &PreviewLoader,
) {
    match app.active().focused().map(|entry| entry.kind) {
        Some(
            EntryKind::Parent
            | EntryKind::Home
            | EntryKind::Favorite
            | EntryKind::Drive
            | EntryKind::Directory,
        ) => open_focused_or_retry(app, scanner, previews),
        Some(EntryKind::File) => app.active_mut().toggle_focused_selection(),
        _ => {}
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
                session.notice = Some("File changed on disk · R Reload".into());
            }
            _ => {}
        }
    }
}

fn session_for_new_window(
    previous: PreviewSession,
    document: dirveyor_domain::PreviewDocument,
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
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q')
            ) {
                previews.cancel();
                app.preview = PreviewState::Closed;
            }
        }
        PreviewState::LoadingWindow { .. } => {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q')
            ) {
                previews.cancel();
                app.preview = PreviewState::Closed;
            }
        }
        PreviewState::Failed { path, .. } => match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                previews.cancel();
                app.preview = PreviewState::Closed;
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
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
                    KeyCode::Esc
                        | KeyCode::Char('q')
                        | KeyCode::Char('Q')
                        | KeyCode::Char('?')
                        | KeyCode::F(1)
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
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    previews.cancel();
                    app.preview = PreviewState::Closed;
                }
                KeyCode::Char('/') => {
                    session.search.editing = true;
                    recompute_preview_search(session);
                }
                KeyCode::Char('f') | KeyCode::Char('F')
                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    session.search.editing = true;
                    recompute_preview_search(session);
                }
                KeyCode::Char('n') | KeyCode::F(3)
                    if !key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    move_preview_match(session, 1)
                }
                KeyCode::Char('N') | KeyCode::F(3) => move_preview_match(session, -1),
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => {
                    scroll_preview(session, -1)
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
                    scroll_preview(session, 1)
                }
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
                KeyCode::Char('w') | KeyCode::Char('W') => session.wrap = !session.wrap,
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
                KeyCode::Char('v') | KeyCode::Char('V') => cycle_preview_mode(session),
                KeyCode::Char('r') | KeyCode::Char('R') => {
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
        KeyCode::Char('r') | KeyCode::Char('R')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            session.search.mode = match session.search.mode {
                PreviewSearchMode::Literal => PreviewSearchMode::Regex,
                PreviewSearchMode::Regex => PreviewSearchMode::Literal,
            };
            recompute_preview_search(session);
        }
        KeyCode::Char('c') | KeyCode::Char('C') if key.modifiers.contains(KeyModifiers::ALT) => {
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
            session.document.kind == dirveyor_domain::PreviewKind::Markdown
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
        (dirveyor_domain::PreviewKind::Markdown, PreviewMode::Raw) => PreviewMode::Split,
        (dirveyor_domain::PreviewKind::Markdown, PreviewMode::Split) => PreviewMode::Formatted,
        (dirveyor_domain::PreviewKind::Markdown, PreviewMode::Formatted) => PreviewMode::Raw,
        (dirveyor_domain::PreviewKind::Json, PreviewMode::Raw) => PreviewMode::Formatted,
        (dirveyor_domain::PreviewKind::Json, _) => PreviewMode::Raw,
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
    navigate_to_restoring_focus(app, scanner, target, None);
}

fn navigate_to_restoring_focus(
    app: &mut AppState,
    scanner: &DirectoryScanner,
    target: PathBuf,
    focus_after_load: Option<PathBuf>,
) {
    let pane_id = app.active_pane;
    let generation = app
        .active_mut()
        .begin_load_restoring_focus(target.clone(), focus_after_load);
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
        navigate_to_drives_restoring_focus(app, scanner, Some(current));
        #[cfg(not(windows))]
        {
            app.notice = Some("Already at the filesystem root".into());
        }
    } else {
        navigate_to_restoring_focus(app, scanner, parent, Some(current));
    }
}

fn navigate_to_drives_restoring_focus(
    app: &mut AppState,
    scanner: &DirectoryScanner,
    focus_after_load: Option<PathBuf>,
) {
    let pane_id = app.active_pane;
    let generation = app
        .active_mut()
        .begin_drive_list_restoring_focus(focus_after_load);
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

fn retry_active_location(app: &mut AppState, scanner: &DirectoryScanner) {
    let pane_id = app.active_pane;
    let browsing_drives = app.active().browsing_drives;
    let location = app.active().location.clone();
    let generation = app.active_mut().retry_load();
    let request = ScanRequest {
        pane: pane_id,
        generation,
        location: if browsing_drives {
            ScanLocation::Drives
        } else {
            ScanLocation::Directory(location)
        },
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
    use dirveyor_domain::{
        FileEntry, JobId, PlanSummary, PlannedStrategy, PreviewCompleteness, PreviewDocument,
        PreviewEncoding, PreviewKind, PreviewLine, PreviewLineStyle,
    };

    #[test]
    fn startup_places_both_panes_in_the_all_drives_view() {
        let scanner = DirectoryScanner::new();
        let mut app = AppState::new(PathBuf::from("left"), PathBuf::from("right"));

        queue_initial_scans(
            &mut app,
            &scanner,
            &LaunchContext {
                left: None,
                right: None,
                left_focus: None,
                right_focus: None,
                active: PaneId::Left,
            },
        );

        for pane_id in PaneId::ALL {
            let pane = app.pane(pane_id);
            assert!(pane.browsing_drives);
            assert!(matches!(pane.load_state, LoadState::Loading));
            assert_eq!(pane.generation, 1);
        }
    }

    #[test]
    fn ctrl_d_toggles_the_browse_delete_mode() {
        let scanner = DirectoryScanner::new();
        let previews = PreviewLoader::new();
        let operations = OperationEngine::new();
        let favorites = FavoritesStore::discover();
        let mut app = app_with_focused_file();

        handle_key(
            &mut app,
            &scanner,
            &previews,
            &operations,
            &favorites,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        );
        assert_eq!(app.delete_mode, DeleteMode::Permanent);

        handle_key(
            &mut app,
            &scanner,
            &previews,
            &operations,
            &favorites,
            KeyEvent::new(KeyCode::Char('D'), KeyModifiers::CONTROL),
        );
        assert_eq!(app.delete_mode, DeleteMode::Recycle);
    }

    #[test]
    fn permanent_delete_review_uses_yes_and_no_keys() {
        let operations = OperationEngine::new();
        let mut app = app_with_focused_file();
        app.operation = OperationView::Review(PlanSummary {
            job: JobId(99),
            kind: OperationKind::PermanentDelete,
            sources: vec![PathBuf::from("target")],
            destination: None,
            strategy: PlannedStrategy::PermanentDelete,
            item_count: 1,
            file_count: 1,
            directory_count: 0,
            total_bytes: 10,
            recursive_scope_known: true,
            conflicts: Vec::new(),
            warnings: Vec::new(),
            verification: None,
        });

        handle_operation_key(
            &mut app,
            &operations,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );

        assert!(matches!(app.operation, OperationView::Review(_)));
        assert_eq!(
            app.notice.as_deref(),
            Some("Press Y to permanently delete or N to cancel")
        );

        handle_operation_key(
            &mut app,
            &operations,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        );
        assert!(matches!(app.operation, OperationView::Idle));
        assert_eq!(
            app.notice.as_deref(),
            Some("Operation cancelled; no files changed")
        );
    }

    #[test]
    fn elevation_is_offered_only_for_permission_denied_messages() {
        assert!(is_permission_denied_message(
            "Access is denied. (os error 5)"
        ));
        assert!(is_permission_denied_message(
            "Permission denied (os error 13)"
        ));
        assert!(!is_permission_denied_message("The path was not found"));
    }

    #[cfg(windows)]
    #[test]
    fn elevated_resume_arguments_quote_spaces_and_trailing_slashes() {
        use std::ffi::OsString;

        let encoded = windows_command_line(&[
            OsString::from("--resume-left"),
            OsString::from(r"C:\folder with space\"),
        ]);
        let rendered = String::from_utf16(&encoded[..encoded.len() - 1]).unwrap();

        assert_eq!(rendered, r#""--resume-left" "C:\folder with space\\""#);
    }

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
        let favorites = FavoritesStore::discover();
        let mut app = app_with_focused_file();

        handle_key(
            &mut app,
            &scanner,
            &previews,
            &operations,
            &favorites,
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
            &favorites,
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
            &favorites,
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

    #[test]
    fn right_arrow_toggles_the_focused_file_selection() {
        let scanner = DirectoryScanner::new();
        let previews = PreviewLoader::new();
        let operations = OperationEngine::new();
        let favorites = FavoritesStore::discover();
        let mut app = app_with_focused_file();
        let path = app.active().focused().unwrap().path.clone();

        handle_key(
            &mut app,
            &scanner,
            &previews,
            &operations,
            &favorites,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        );
        assert!(app.active().selected.contains(&path));

        handle_key(
            &mut app,
            &scanner,
            &previews,
            &operations,
            &favorites,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        );
        assert!(!app.active().selected.contains(&path));
    }

    #[test]
    fn right_enters_a_directory_and_left_returns_focused_on_it() {
        let scanner = DirectoryScanner::new();
        let previews = PreviewLoader::new();
        let operations = OperationEngine::new();
        let favorites = FavoritesStore::discover();
        let root = PathBuf::from("browse-root");
        let child = root.join("child");
        let mut app = AppState::new(root.clone(), root.clone());
        app.active_mut().apply_entries(
            0,
            vec![FileEntry {
                path: child.clone(),
                display_name: "child".into(),
                kind: EntryKind::Directory,
                size: None,
                modified: None,
                metadata_incomplete: false,
                drive_info: None,
            }],
        );

        handle_key(
            &mut app,
            &scanner,
            &previews,
            &operations,
            &favorites,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        );
        assert_eq!(app.active().location, child);
        let child_generation = app.active().generation;
        app.active_mut().apply_entries(child_generation, Vec::new());

        handle_key(
            &mut app,
            &scanner,
            &previews,
            &operations,
            &favorites,
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
        );
        assert_eq!(app.active().location, root);
        let parent_generation = app.active().generation;
        app.active_mut().apply_entries(
            parent_generation,
            vec![
                FileEntry {
                    path: root.join("alpha"),
                    display_name: "alpha".into(),
                    kind: EntryKind::Directory,
                    size: None,
                    modified: None,
                    metadata_incomplete: false,
                    drive_info: None,
                },
                FileEntry {
                    path: child,
                    display_name: "child".into(),
                    kind: EntryKind::Directory,
                    size: None,
                    modified: None,
                    metadata_incomplete: false,
                    drive_info: None,
                },
            ],
        );
        assert_eq!(app.active().focused().unwrap().display_name, "child");
    }

    #[test]
    fn quick_access_contains_home_favorites_and_drives_without_home_duplication() {
        let home = PathBuf::from("users").join("tester");
        let favorite = PathBuf::from("projects");
        let mut app = AppState::new(PathBuf::from("root"), PathBuf::from("other"));
        app.home_directory = Some(home.clone());
        app.favorites = vec![home, favorite.clone()];
        let entries = quick_access_entries(
            &app,
            vec![FileEntry {
                path: PathBuf::from("D:\\"),
                display_name: "D:\\".into(),
                kind: EntryKind::Drive,
                size: None,
                modified: None,
                metadata_incomplete: false,
                drive_info: None,
            }],
        );

        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::Home)
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::Favorite)
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![favorite]
        );
        assert!(entries.iter().any(|entry| entry.kind == EntryKind::Drive));
    }

    #[test]
    fn favorite_toggle_persists_and_ctrl_f_opens_the_panel() {
        let temp = tempfile::tempdir().unwrap();
        let store = FavoritesStore::from_path(temp.path().join("favorites.json"));
        let scanner = DirectoryScanner::new();
        let previews = PreviewLoader::new();
        let operations = OperationEngine::new();
        let root = PathBuf::from("root");
        let favorite = root.join("project");
        let mut app = AppState::new(root.clone(), root);
        app.active_mut().apply_entries(
            0,
            vec![FileEntry {
                path: favorite.clone(),
                display_name: "project".into(),
                kind: EntryKind::Directory,
                size: None,
                modified: None,
                metadata_incomplete: false,
                drive_info: None,
            }],
        );

        toggle_favorite(&mut app, &scanner, &store);
        assert_eq!(app.favorites, vec![favorite.clone()]);
        assert_eq!(store.load().unwrap(), vec![favorite]);

        handle_key(
            &mut app,
            &scanner,
            &previews,
            &operations,
            &store,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        );
        assert!(app.favorites_panel.is_some());
    }

    #[test]
    fn favorites_panel_can_open_and_remove_saved_folders() {
        let temp = tempfile::tempdir().unwrap();
        let store = FavoritesStore::from_path(temp.path().join("favorites.json"));
        let scanner = DirectoryScanner::new();
        let root = PathBuf::from("root");
        let first = PathBuf::from("first");
        let second = PathBuf::from("second");
        let mut app = AppState::new(root.clone(), root);
        app.favorites = vec![first, second.clone()];
        store.save(&app.favorites).unwrap();
        app.favorites_panel = Some(FavoritesPanel { cursor: 1 });

        handle_favorites_key(
            &mut app,
            &scanner,
            &store,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(app.favorites_panel.is_none());
        assert_eq!(app.active().location, second);

        app.favorites_panel = Some(FavoritesPanel { cursor: 1 });
        handle_favorites_key(
            &mut app,
            &scanner,
            &store,
            KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
        );
        assert_eq!(app.favorites, vec![PathBuf::from("first")]);
        assert_eq!(store.load().unwrap(), app.favorites);
        assert_eq!(app.favorites_panel.unwrap().cursor, 0);
    }
}
