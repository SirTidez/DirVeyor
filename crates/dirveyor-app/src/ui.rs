use dirveyor_domain::{
    AppState, ConflictKind, ConflictPrompt, DriveInfo, DriveKind, EntryKind, FolderSizeState,
    JobOutcome, JobPhase, LoadState, OperationKind, OperationView, PaneId, PaneState, PlanSummary,
    PreviewLine, PreviewLineStyle, PreviewMode, PreviewRegion, PreviewSession, PreviewState,
    TextAction, TextPrompt, VersionRelation,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::time::{SystemTime, UNIX_EPOCH};
use time::{OffsetDateTime, UtcOffset};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MIN_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 24;
const INSPECTOR_WIDTH_THRESHOLD: u16 = 110;

pub fn render(frame: &mut Frame, app: &AppState) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_too_small(frame, area);
        return;
    }

    if app.preview.is_open() {
        render_preview(frame, area, &app.preview);
        return;
    }

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);

    render_header(frame, header, app);
    let transfer_source = app.transfer_source_pane();
    if body.width >= INSPECTOR_WIDTH_THRESHOLD {
        let [left, right, inspector] = Layout::horizontal([
            Constraint::Percentage(38),
            Constraint::Percentage(38),
            Constraint::Percentage(24),
        ])
        .areas(body);
        render_pane(
            frame,
            left,
            app.pane(PaneId::Left),
            app.active_pane == PaneId::Left,
            PaneId::Left,
            transfer_source,
        );
        render_pane(
            frame,
            right,
            app.pane(PaneId::Right),
            app.active_pane == PaneId::Right,
            PaneId::Right,
            transfer_source,
        );
        render_inspector(frame, inspector, app);
    } else {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(body);
        render_pane(
            frame,
            left,
            app.pane(PaneId::Left),
            app.active_pane == PaneId::Left,
            PaneId::Left,
            transfer_source,
        );
        render_pane(
            frame,
            right,
            app.pane(PaneId::Right),
            app.active_pane == PaneId::Right,
            PaneId::Right,
            transfer_source,
        );
    }
    render_footer(frame, footer, app);

    if app.help_visible {
        render_help(frame, area);
    } else if app.favorites_panel.is_some() {
        render_favorites_panel(frame, area, app);
    } else if let Some(prompt) = &app.text_prompt {
        render_text_prompt(frame, area, prompt);
    } else if !matches!(app.operation, OperationView::Idle) {
        render_operation(frame, area, app);
    }
}

fn render_preview(frame: &mut Frame, area: Rect, preview: &PreviewState) {
    match preview {
        PreviewState::Closed => {}
        PreviewState::Loading { path, .. } => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " Preview · {} ",
                    safe_text(&path.to_string_lossy())
                ))
                .border_style(Style::default().fg(Color::Cyan));
            frame.render_widget(
                Paragraph::new(format!(
                    "\n\n{} Loading text preview…",
                    folder_size_spinner()
                ))
                .block(block)
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::Cyan)),
                area,
            );
        }
        PreviewState::Failed { path, message, .. } => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " Preview unavailable · {} ",
                    safe_text(&path.to_string_lossy())
                ))
                .border_style(Style::default().fg(Color::Red));
            let lines = vec![
                Line::styled(
                    "Preview could not be opened",
                    Style::default().fg(Color::Red),
                ),
                Line::raw(""),
                Line::raw(safe_text(message)),
                Line::raw(""),
                Line::styled("R Retry · Esc/Q Back", Style::default().fg(Color::Cyan)),
            ];
            frame.render_widget(
                Paragraph::new(lines)
                    .block(block)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
        PreviewState::Ready(session) | PreviewState::LoadingWindow { session, .. } => {
            render_preview_session(frame, area, session)
        }
    }
}

fn render_preview_session(frame: &mut Frame, area: Rect, session: &PreviewSession) {
    let [header, content, status, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let document = &session.document;
    let name = document
        .path
        .file_name()
        .map(|name| safe_text(&name.to_string_lossy()))
        .unwrap_or_else(|| safe_text(&document.path.to_string_lossy()));
    let window = if document.completeness.is_complete() {
        String::new()
    } else {
        format!(
            " · bytes {}–{}",
            document.window_start.saturating_add(1),
            document.window_end
        )
    };
    let header_text = format!(
        " Preview · {name} · {} · {} · {} · {} · {}{} ",
        document.kind.label(),
        session.mode.label(document.kind),
        document.encoding.label(),
        human_size(document.file_size),
        document.completeness.label(),
        window
    );
    frame.render_widget(
        Paragraph::new(truncate(&header_text, header.width as usize)).style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        header,
    );

    match session.mode {
        PreviewMode::Raw => render_raw_preview(frame, content, session, false),
        PreviewMode::Formatted => render_formatted_preview(frame, content, session, false),
        PreviewMode::Split => {
            let [raw, formatted] =
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(content);
            render_raw_preview(frame, raw, session, true);
            render_formatted_preview(frame, formatted, session, true);
        }
    }

    frame.render_widget(
        Paragraph::new(preview_status(session)).style(Style::default().fg(Color::Yellow)),
        status,
    );
    frame.render_widget(
        Paragraph::new(truncate(&preview_footer(session), footer.width as usize))
            .style(Style::default().fg(Color::Black).bg(Color::Gray)),
        footer,
    );

    if session.help_visible {
        render_preview_help(frame, area);
    }
}

fn render_raw_preview(frame: &mut Frame, area: Rect, session: &PreviewSession, split: bool) {
    let active = session.mode != PreviewMode::Split || session.active_region == PreviewRegion::Raw;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(if split { " Raw " } else { " Text " })
        .border_style(Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let source = &session.document.raw_lines;
    let end = (session.raw_scroll + inner.height as usize).min(source.len());
    let number_width = source.len().max(1).to_string().len();
    let lines = source[session.raw_scroll.min(source.len())..end]
        .iter()
        .enumerate()
        .map(|(offset, text)| {
            preview_display_line(
                session.raw_scroll + offset,
                text,
                PreviewLineStyle::Normal,
                number_width,
                session,
                PreviewRegion::Raw,
            )
        })
        .collect::<Vec<_>>();
    let paragraph = Paragraph::new(lines);
    let paragraph = if session.wrap {
        paragraph.wrap(Wrap { trim: false })
    } else {
        paragraph
    };
    frame.render_widget(paragraph, inner);
}

fn render_formatted_preview(frame: &mut Frame, area: Rect, session: &PreviewSession, split: bool) {
    let active =
        session.mode != PreviewMode::Split || session.active_region == PreviewRegion::Formatted;
    let title = if session.document.kind == dirveyor_domain::PreviewKind::Json {
        " Pretty JSON "
    } else {
        " Rendered "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(if split { title } else { " Preview " })
        .border_style(Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let fallback = vec![PreviewLine {
        text: session
            .document
            .format_error
            .clone()
            .unwrap_or_else(|| "Formatted representation unavailable".into()),
        style: PreviewLineStyle::Normal,
    }];
    let source = session
        .document
        .formatted_lines
        .as_ref()
        .unwrap_or(&fallback);
    let lines = preview_visible_lines(
        source,
        session.formatted_scroll,
        inner.height as usize,
        session,
        PreviewRegion::Formatted,
    );
    let paragraph = Paragraph::new(lines);
    let paragraph = if session.wrap {
        paragraph.wrap(Wrap { trim: false })
    } else {
        paragraph
    };
    frame.render_widget(paragraph, inner);
}

fn preview_visible_lines(
    source: &[PreviewLine],
    scroll: usize,
    height: usize,
    session: &PreviewSession,
    region: PreviewRegion,
) -> Vec<Line<'static>> {
    let end = (scroll + height).min(source.len());
    let number_width = source.len().max(1).to_string().len();
    source[scroll.min(source.len())..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| {
            preview_display_line(
                scroll + offset,
                &line.text,
                line.style,
                number_width,
                session,
                region,
            )
        })
        .collect()
}

fn preview_display_line(
    line_index: usize,
    text: &str,
    line_style: PreviewLineStyle,
    number_width: usize,
    session: &PreviewSession,
    region: PreviewRegion,
) -> Line<'static> {
    let base = preview_line_style(line_style);
    let horizontal = if session.wrap {
        0
    } else {
        session.horizontal_scroll
    };
    let (visible, byte_offset) = skip_characters(text, horizontal);
    let mut spans = vec![Span::styled(
        format!("{:>number_width$} │ ", line_index + 1),
        Style::default().fg(Color::DarkGray),
    )];
    let search_is_for_region =
        session.mode != PreviewMode::Split || session.active_region == region;
    if !search_is_for_region || session.search.matches.is_empty() {
        spans.push(Span::styled(visible.to_owned(), base));
        return Line::from(spans);
    }
    let mut cursor = byte_offset;
    for (match_index, found) in session.search.matches.iter().enumerate() {
        if found.line != line_index || found.end <= byte_offset {
            continue;
        }
        let start = found.start.max(byte_offset).min(text.len());
        let end = found.end.max(start).min(text.len());
        if start > cursor {
            spans.push(Span::styled(text[cursor..start].to_owned(), base));
        }
        let highlight = if session.search.current == Some(match_index) {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Black).bg(Color::DarkGray)
        };
        spans.push(Span::styled(text[start..end].to_owned(), highlight));
        cursor = end;
    }
    if cursor < text.len() {
        spans.push(Span::styled(text[cursor..].to_owned(), base));
    }
    Line::from(spans)
}

fn skip_characters(value: &str, count: usize) -> (&str, usize) {
    let offset = value
        .char_indices()
        .nth(count)
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    (&value[offset..], offset)
}

fn preview_line_style(style: PreviewLineStyle) -> Style {
    match style {
        PreviewLineStyle::Normal => Style::default(),
        PreviewLineStyle::Heading => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        PreviewLineStyle::Quote => Style::default().fg(Color::LightBlue),
        PreviewLineStyle::Code => Style::default().fg(Color::Green),
        PreviewLineStyle::List => Style::default().fg(Color::White),
        PreviewLineStyle::Rule => Style::default().fg(Color::DarkGray),
    }
}

fn preview_status(session: &PreviewSession) -> String {
    if session.search.editing {
        let case = if session.search.case_sensitive {
            "Case sensitive"
        } else {
            "Ignore case"
        };
        return format!(
            " Find: {}_ · {} · {case} · Ctrl+R Mode · Alt+C Case",
            safe_text(&session.search.query),
            session.search.mode.label()
        );
    }
    if session.source_changed {
        return "File changed on disk · R Reload".into();
    }
    if let Some(error) = &session.search.error {
        return safe_text(error);
    }
    if !session.search.query.is_empty() {
        let position = session.search.current.map(|index| index + 1).unwrap_or(0);
        let scope = if session.document.completeness.is_complete() {
            ""
        } else {
            " · matches in loaded window"
        };
        let notice = session
            .notice
            .as_deref()
            .map(|notice| format!(" · {}", safe_text(notice)))
            .unwrap_or_default();
        return format!(
            " Find: {} · {position}/{}{}{}",
            safe_text(&session.search.query),
            session.search.matches.len(),
            scope,
            notice
        );
    }
    session
        .notice
        .clone()
        .or_else(|| session.document.format_error.clone())
        .unwrap_or_else(|| {
            if session.document.completeness.is_complete() {
                safe_text(&session.document.path.to_string_lossy())
            } else {
                format!(
                    "Loaded bytes {}–{} of {} · [/] Previous/next window",
                    session.document.window_start.saturating_add(1),
                    session.document.window_end,
                    session.document.file_size
                )
            }
        })
}

fn preview_footer(session: &PreviewSession) -> String {
    let modes = match session.document.kind {
        dirveyor_domain::PreviewKind::Markdown => "1 Raw  2 Split  3 Preview  ",
        dirveyor_domain::PreviewKind::Json if session.document.formatted_lines.is_some() => {
            "1 Raw  3 Pretty  "
        }
        _ => "",
    };
    format!(
        " Esc/Q Back  ↑↓ Scroll  PgUp/PgDn  [/] Window  G/Shift+G First/Last  {modes}/ Find  N/Shift+N Match  R Reload"
    )
}

fn render_preview_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(76, 88, area);
    let lines = vec![
        Line::styled(
            "Full-screen Preview controls",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw("Esc / Q            Return to Browse"),
        Line::raw("↑ ↓ / J K          Scroll"),
        Line::raw("PageUp / PageDown  Scroll by page"),
        Line::raw("Home / End          Start/end of loaded window"),
        Line::raw("[ / ]               Previous / next file window"),
        Line::raw("G / Shift+G         First / last file window"),
        Line::raw("← →                 Horizontal scroll"),
        Line::raw("W                   Toggle wrapping"),
        Line::raw("1 / 2 / 3           Raw / Split / Preview"),
        Line::raw("Tab                 Focus split region"),
        Line::raw("/ / Ctrl+F          Find"),
        Line::raw("Ctrl+R in Find      Literal / Regex"),
        Line::raw("Alt+C in Find       Toggle case matching"),
        Line::raw("N / Shift+N / F3    Next / previous match"),
        Line::raw("R                   Reload"),
        Line::raw(""),
        Line::styled("Esc / F1 Close help", Style::default().fg(Color::Cyan)),
    ];
    render_modal(frame, popup, " Preview help ", lines, Color::Cyan);
}

fn render_header(frame: &mut Frame, area: Rect, app: &AppState) {
    let selection = app.selected_count();
    let operation_status = match &app.operation {
        OperationView::Idle => None,
        OperationView::Planning(progress) => Some(format!("Planning {}…", progress.kind.label())),
        OperationView::Review(summary) => {
            Some(format!("Review {} before execution", summary.kind.label()))
        }
        OperationView::Running(progress) => Some(progress_status(progress)),
        OperationView::Conflict(_) => Some("Transfer waiting for a conflict choice".into()),
        OperationView::Finished(report) => Some(format!(
            "{} {}",
            outcome_label(report.outcome),
            report.kind.label()
        )),
        OperationView::Error { kind, .. } => Some(format!("{} plan failed", kind.label())),
    };
    let status = operation_status
        .as_deref()
        .or(app.notice.as_deref())
        .unwrap_or("Ready");
    let line = Line::from(vec![
        Span::styled(
            " DirVeyor ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Browse", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!("  ·  {selection} selected  ·  ")),
        Span::styled(status, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_pane(
    frame: &mut Frame,
    area: Rect,
    pane: &PaneState,
    active: bool,
    pane_id: PaneId,
    transfer_source: PaneId,
) {
    let border_style = if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let path_width = area.width.saturating_sub(8) as usize;
    let location = if pane.browsing_drives {
        "All drives".into()
    } else {
        safe_text(&pane.location.to_string_lossy())
    };
    let route = match (pane_id, transfer_source) {
        (PaneId::Left, PaneId::Left) => " [SOURCE →]",
        (PaneId::Right, PaneId::Left) => " [→ DESTINATION]",
        (PaneId::Right, PaneId::Right) => " [← SOURCE]",
        (PaneId::Left, PaneId::Right) => " [DESTINATION ←]",
    };
    let title = format!(
        " {}{} ",
        truncate(&location, path_width.saturating_sub(route.len())),
        route
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match &pane.load_state {
        LoadState::Loading => {
            frame.render_widget(
                Paragraph::new("◌ Discovering items…")
                    .style(Style::default().fg(Color::Cyan))
                    .alignment(Alignment::Center),
                inner,
            );
        }
        LoadState::Failed(message) => {
            let text = vec![
                Line::styled("! Folder unavailable", Style::default().fg(Color::Yellow)),
                Line::raw(""),
                Line::raw(safe_text(message)),
                Line::raw(""),
                Line::styled(
                    "Enter Retry · ←/Backspace Parent",
                    Style::default().fg(Color::Cyan),
                ),
            ];
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), inner);
        }
        LoadState::Ready => render_entries(frame, inner, pane, active),
    }
}

fn render_entries(frame: &mut Frame, area: Rect, pane: &PaneState, active: bool) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let indices = pane.visible_indices();
    if indices.is_empty() && !pane.browsing_drives {
        let message = if pane.filter.is_empty() {
            "This folder is empty"
        } else {
            "No items match the filter"
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Center),
            area,
        );
        return;
    }

    let rows = browser_rows(pane, &indices);
    let height = area.height as usize;
    let focused_row = rows
        .iter()
        .position(|row| matches!(row, BrowserRow::Entry { visible_position, .. } if *visible_position == pane.cursor))
        .unwrap_or(0);
    let start = focused_row.saturating_sub(height.saturating_sub(1));
    let end = (start + height).min(rows.len());
    let name_width = area.width.saturating_sub(16) as usize;
    let mut lines = Vec::with_capacity(end - start);

    for row in &rows[start..end] {
        let BrowserRow::Entry {
            visible_position,
            entry_index,
        } = row
        else {
            let (text, style) = match row {
                BrowserRow::Heading(text) => (
                    *text,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                BrowserRow::Empty(text) => (*text, Style::default().fg(Color::DarkGray)),
                BrowserRow::Entry { .. } => unreachable!(),
            };
            lines.push(Line::styled(text, style));
            continue;
        };
        let entry = &pane.entries[*entry_index];
        let focused = active && *visible_position == pane.cursor;
        let selected = pane.selected.contains(&entry.path);
        let cursor = if focused { ">" } else { " " };
        let check = if selected { "[x]" } else { "[ ]" };
        let kind = match entry.kind {
            EntryKind::Parent => "↑",
            EntryKind::Home => "⌂",
            EntryKind::Favorite => "★",
            EntryKind::Drive => "◆",
            EntryKind::Directory => "/",
            EntryKind::Symlink => "@",
            EntryKind::File => " ",
            EntryKind::Other => "?",
        };
        let name = pad_to_width(&truncate(&entry.display_name, name_width), name_width);
        let size = entry
            .drive_info
            .as_ref()
            .and_then(DriveInfo::used_percent)
            .map(|percent| format!("{percent}% used"))
            .or_else(|| entry.size.map(human_size))
            .or_else(|| {
                matches!(entry.kind, EntryKind::Home | EntryKind::Favorite)
                    .then(|| "Shortcut".into())
            })
            .unwrap_or_else(|| "—".into());
        let line = format!("{cursor}{check} {kind}{name} {size:>8}");
        let style = if focused {
            Style::default()
                .fg(Color::Cyan)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else if selected {
            Style::default().fg(Color::Cyan)
        } else if matches!(
            entry.kind,
            EntryKind::Directory | EntryKind::Home | EntryKind::Favorite
        ) {
            Style::default().fg(Color::LightBlue)
        } else {
            Style::default()
        };
        lines.push(Line::styled(line, style));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

enum BrowserRow {
    Heading(&'static str),
    Empty(&'static str),
    Entry {
        visible_position: usize,
        entry_index: usize,
    },
}

fn browser_rows(pane: &PaneState, indices: &[usize]) -> Vec<BrowserRow> {
    if !pane.browsing_drives {
        return indices
            .iter()
            .enumerate()
            .map(|(visible_position, entry_index)| BrowserRow::Entry {
                visible_position,
                entry_index: *entry_index,
            })
            .collect();
    }
    let mut rows = Vec::new();
    append_drive_group(
        &mut rows,
        pane,
        indices,
        "── User folder ──",
        EntryKind::Home,
        "  User folder unavailable",
    );
    append_drive_group(
        &mut rows,
        pane,
        indices,
        "── Favorites · F Add/remove ──",
        EntryKind::Favorite,
        "  No favorite folders yet",
    );
    append_drive_group(
        &mut rows,
        pane,
        indices,
        "── Drives ──",
        EntryKind::Drive,
        "  No drives available",
    );
    rows
}

fn append_drive_group(
    rows: &mut Vec<BrowserRow>,
    pane: &PaneState,
    indices: &[usize],
    heading: &'static str,
    kind: EntryKind,
    empty: &'static str,
) {
    rows.push(BrowserRow::Heading(heading));
    let before = rows.len();
    for (visible_position, entry_index) in indices.iter().enumerate() {
        if pane.entries[*entry_index].kind == kind {
            rows.push(BrowserRow::Entry {
                visible_position,
                entry_index: *entry_index,
            });
        }
    }
    if rows.len() == before {
        rows.push(BrowserRow::Empty(empty));
    }
}

fn render_inspector(frame: &mut Frame, area: Rect, app: &AppState) {
    let pane = app.active();
    let selection_size = human_size(pane.selected_bytes());
    let mut lines = vec![
        Line::styled("Selection", Style::default().fg(Color::DarkGray)),
        Line::styled(
            format!("{} items · {selection_size}", pane.selected.len()),
            Style::default().fg(Color::Cyan),
        ),
        Line::raw(""),
        Line::styled("Focused item", Style::default().fg(Color::DarkGray)),
    ];
    if let Some(entry) = pane.focused() {
        lines.push(Line::styled(
            entry.display_name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        if let Some(drive) = &entry.drive_info {
            append_drive_details(&mut lines, drive);
        } else if matches!(entry.kind, EntryKind::Home | EntryKind::Favorite) {
            lines.push(Line::raw(format!("Type: {}", kind_label(entry.kind))));
            lines.push(Line::raw(format!("Target: {}", entry.path.display())));
            lines.push(Line::styled(
                "F Add/remove favorite",
                Style::default().fg(Color::Cyan),
            ));
        } else {
            lines.push(Line::raw(format!("Type: {}", kind_label(entry.kind))));
            if entry.kind == EntryKind::Directory {
                append_folder_size_details(&mut lines, &app.folder_size, &entry.path);
            } else {
                lines.push(Line::raw(format!(
                    "Size: {}",
                    entry.size.map(human_size).unwrap_or_else(|| "—".into())
                )));
            }
            lines.push(Line::raw(format!(
                "Modified: {}",
                modified_label(entry.modified)
            )));
        }
        if entry.metadata_incomplete {
            lines.push(Line::styled(
                "Some metadata unavailable",
                Style::default().fg(Color::Yellow),
            ));
        }
    } else {
        lines.push(Line::raw("No item focused"));
    }
    let delete_action = match app.delete_mode {
        dirveyor_domain::DeleteMode::Recycle => "D Recycle · Ctrl+D Change mode",
        dirveyor_domain::DeleteMode::Permanent => "D DELETE · Ctrl+D Change mode",
    };
    lines.extend([
        Line::raw(""),
        Line::styled("Pane", Style::default().fg(Color::DarkGray)),
        Line::raw(format!("Sort: {}", pane.sort.label())),
        Line::raw(format!(
            "Hidden: {}",
            if pane.show_hidden { "shown" } else { "hidden" }
        )),
        Line::raw(format!(
            "Filter: {}",
            if pane.filter.is_empty() {
                "—"
            } else {
                &pane.filter
            }
        )),
        if pane.truncated {
            Line::styled(
                "Showing the first 50,000 entries",
                Style::default().fg(Color::Yellow),
            )
        } else {
            Line::raw(format!("Loaded: {} items", pane.entries.len()))
        },
        Line::raw(""),
        Line::styled("← Back · → Open/select", Style::default().fg(Color::Cyan)),
        Line::styled(
            "P Preview · C Copy · M Move",
            Style::default().fg(Color::Cyan),
        ),
        Line::styled(
            format!("Ctrl+V Verify: {}", app.transfer_verification.label()),
            Style::default().fg(Color::Cyan),
        ),
        Line::styled(delete_action, Style::default().fg(Color::Cyan)),
        Line::styled("R Rename · N Folder", Style::default().fg(Color::Cyan)),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Inspector ")
        .border_style(Style::default().fg(Color::DarkGray));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn append_folder_size_details(
    lines: &mut Vec<Line<'static>>,
    state: &FolderSizeState,
    focused_path: &std::path::Path,
) {
    match state {
        FolderSizeState::Loading(progress) if progress.path == focused_path => {
            lines.push(Line::styled(
                format!(
                    "{} Contained size: {} so far",
                    folder_size_spinner(),
                    human_size(progress.discovered_bytes)
                ),
                Style::default().fg(Color::Cyan),
            ));
            lines.push(Line::raw(match progress.drive_total_bytes {
                Some(total) => format!(
                    "Drive share: {} so far",
                    drive_share_label(progress.discovered_bytes, Some(total))
                ),
                None => "Drive share: Discovering…".into(),
            }));
            lines.push(Line::raw(format!(
                "Scanned: {} files · {} folders",
                progress.file_count,
                progress.directory_count.saturating_sub(1)
            )));
        }
        FolderSizeState::Ready(summary) if summary.path == focused_path => {
            lines.push(Line::raw(format!(
                "Contained size: {}",
                human_size(summary.total_bytes)
            )));
            lines.push(Line::raw(format!(
                "Drive share: {}",
                drive_share_label(summary.total_bytes, summary.drive_total_bytes)
            )));
            lines.push(Line::raw(format!(
                "Contents: {} files · {} folders",
                summary.file_count,
                summary.directory_count.saturating_sub(1)
            )));
            if summary.skipped_items > 0 {
                lines.push(Line::styled(
                    format!("{} inaccessible/link items skipped", summary.skipped_items),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }
        FolderSizeState::Failed { path, message, .. } if path == focused_path => {
            lines.push(Line::styled(
                "Contained size: Unavailable",
                Style::default().fg(Color::Yellow),
            ));
            lines.push(Line::styled(
                safe_text(message),
                Style::default().fg(Color::Yellow),
            ));
        }
        _ => {
            lines.push(Line::styled(
                "Contained size: Waiting…",
                Style::default().fg(Color::DarkGray),
            ));
            lines.push(Line::raw("Drive share: Waiting…"));
        }
    }
}

fn folder_size_spinner() -> &'static str {
    const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
    let tick = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() / 150)
        .unwrap_or(0);
    FRAMES[(tick as usize) % FRAMES.len()]
}

fn drive_share_label(folder_bytes: u64, drive_total_bytes: Option<u64>) -> String {
    let Some(drive_total_bytes) = drive_total_bytes.filter(|total| *total > 0) else {
        return "Unavailable".into();
    };
    if folder_bytes == 0 {
        return format!("0% of {}", human_size(drive_total_bytes));
    }
    let percent = folder_bytes as f64 * 100.0 / drive_total_bytes as f64;
    let percentage = if percent < 0.001 {
        "<0.001%".into()
    } else if percent < 1.0 {
        format!("{percent:.3}%")
    } else {
        format!("{percent:.2}%")
    };
    format!("{percentage} of {}", human_size(drive_total_bytes))
}

fn append_drive_details(lines: &mut Vec<Line<'static>>, drive: &DriveInfo) {
    lines.push(Line::raw(format!("Type: {}", drive_kind_label(drive.kind))));
    if let Some(filesystem) = &drive.filesystem {
        lines.push(Line::raw(format!("Filesystem: {filesystem}")));
    }
    lines.push(Line::raw(format!(
        "Capacity: {}",
        drive
            .total_bytes
            .map(human_size)
            .unwrap_or_else(|| "Unavailable".into())
    )));
    lines.push(Line::raw(format!(
        "Used: {}",
        drive
            .used_bytes()
            .map(human_size)
            .unwrap_or_else(|| "Unavailable".into())
    )));
    lines.push(Line::raw(format!(
        "Available: {}",
        drive
            .available_bytes
            .map(human_size)
            .unwrap_or_else(|| "Unavailable".into())
    )));
    if let Some(percent) = drive.used_percent() {
        lines.push(Line::styled(
            drive_usage_bar(percent, 18),
            Style::default().fg(if percent >= 90 {
                Color::Red
            } else if percent >= 75 {
                Color::Yellow
            } else {
                Color::Cyan
            }),
        ));
    }
}

fn drive_usage_bar(percent: u8, width: usize) -> String {
    let filled = (usize::from(percent) * width + 50) / 100;
    format!(
        "[{}{}] {percent}%",
        "█".repeat(filled.min(width)),
        "░".repeat(width.saturating_sub(filled))
    )
}

fn drive_kind_label(kind: DriveKind) -> &'static str {
    match kind {
        DriveKind::Fixed => "Fixed disk",
        DriveKind::Removable => "Removable",
        DriveKind::Network => "Network drive",
        DriveKind::Optical => "Optical drive",
        DriveKind::RamDisk => "RAM disk",
        DriveKind::Unknown => "Unknown",
    }
}

fn render_footer(frame: &mut Frame, area: Rect, app: &AppState) {
    let text = if app.filter_mode {
        format!(
            " / {}_    Enter Apply  Esc Close  Backspace Delete",
            app.active().filter
        )
    } else if area.width >= 110 {
        format!(
            " Tab Pane  ↑↓ Move  ← Back  → Open/Select  Enter Open/Preview  C Copy  M Move  D {}  Ctrl+D Mode  F1 Help",
            match app.delete_mode {
                dirveyor_domain::DeleteMode::Recycle => "Recycle",
                dirveyor_domain::DeleteMode::Permanent => "DELETE",
            }
        )
    } else {
        " ↑↓ Move  ← Back  → Open/Select  Enter Open  P Preview  F1 Help".into()
    };
    frame.render_widget(
        Paragraph::new(truncate(&text, area.width as usize))
            .style(Style::default().fg(Color::Black).bg(Color::Gray)),
        area,
    );
}

fn render_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(72, 92, area);
    let lines = vec![
        Line::styled(
            "DirVeyor controls",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw("Tab / Shift+Tab   Switch pane"),
        Line::raw("↑ ↓ or J K         Move focus"),
        Line::raw("← / Backspace      Parent folder"),
        Line::raw("→                   Open folder / toggle file selection"),
        Line::raw("Home / End         First / last item"),
        Line::raw("Space              Toggle selection"),
        Line::raw("Enter              Open folder or preview file"),
        Line::raw("P                   Preview focused text file"),
        Line::raw("/                  Filter active pane"),
        Line::raw("H / S              Toggle dotfiles / cycle sort"),
        Line::raw("C / M              Plan copy / move to other pane"),
        Line::raw("Ctrl+V             Toggle Full / Fast verification"),
        Line::raw("D / Delete         Plan using the current delete mode"),
        Line::raw("Ctrl+D             Toggle Recycle / permanent delete"),
        Line::raw("R / F2 / N         Rename item / create folder"),
        Line::raw("F / Ctrl+F         Add favorite / open Favorites"),
        Line::raw("Q / Ctrl+C         Quit"),
        Line::raw(""),
        Line::styled(
            "All mutations are reviewed first. Transfer conflicts pause for a source/destination choice.",
            Style::default().fg(Color::Yellow),
        ),
        Line::raw(""),
        Line::styled("Esc / ? / F1 closes help", Style::default().fg(Color::Cyan)),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn render_favorites_panel(frame: &mut Frame, area: Rect, app: &AppState) {
    let popup = centered_rect(86, 80, area);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Favorites · {} folders ", app.favorites.len()))
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [content, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);
    if app.favorites.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    "No favorite folders yet",
                    Style::default().fg(Color::DarkGray),
                ),
                Line::raw(""),
                Line::raw("Close this panel, focus a folder, and press F to add it."),
            ])
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false }),
            content,
        );
    } else {
        let cursor = app
            .favorites_panel
            .as_ref()
            .map_or(0, |panel| panel.cursor.min(app.favorites.len() - 1));
        let height = content.height as usize;
        let start = cursor.saturating_sub(height.saturating_sub(1));
        let lines = app.favorites[start..]
            .iter()
            .take(height)
            .enumerate()
            .map(|(offset, path)| {
                let row = start + offset;
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("Folder");
                let marker = if row == cursor { ">" } else { " " };
                let text = truncate(
                    &format!("{marker} ★ {name}  {}", safe_text(&path.to_string_lossy())),
                    content.width as usize,
                );
                Line::styled(
                    text,
                    if row == cursor {
                        Style::default()
                            .fg(Color::Cyan)
                            .bg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                )
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), content);
    }
    frame.render_widget(
        Paragraph::new("↑↓ Move · Enter/→ Open · F/Delete Remove · Ctrl+F/Esc Close")
            .style(Style::default().fg(Color::Black).bg(Color::Gray))
            .wrap(Wrap { trim: false }),
        footer,
    );
}

fn render_text_prompt(frame: &mut Frame, area: Rect, prompt: &TextPrompt) {
    let popup = centered_rect(72, 34, area);
    let (title, action, context) = match &prompt.action {
        TextAction::Rename { source } => (
            " Rename ",
            "Enter Plan rename",
            format!("Current: {}", safe_text(&source.to_string_lossy())),
        ),
        TextAction::CreateDirectory { parent } => (
            " New folder ",
            "Enter Plan creation",
            format!("Inside: {}", safe_text(&parent.to_string_lossy())),
        ),
    };
    let mut lines = vec![
        Line::raw(context),
        Line::raw(""),
        Line::from(vec![
            Span::styled("Name: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}_", safe_text(&prompt.value)),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ];
    if let Some(error) = &prompt.error {
        lines.push(Line::styled(
            safe_text(error),
            Style::default().fg(Color::Red),
        ));
    } else {
        lines.push(Line::raw(""));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("{action}   Esc Cancel"),
        Style::default().fg(Color::Cyan),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn render_operation(frame: &mut Frame, area: Rect, app: &AppState) {
    match &app.operation {
        OperationView::Idle => {}
        OperationView::Planning(progress) => render_planning(frame, area, progress),
        OperationView::Review(summary) => render_review(frame, area, summary),
        OperationView::Running(progress) => render_running(frame, area, progress),
        OperationView::Conflict(prompt) => render_conflict(frame, area, prompt),
        OperationView::Finished(report) => render_finished(frame, area, report),
        OperationView::Error { kind, message, .. } => {
            render_operation_error(frame, area, *kind, message)
        }
    }
}

fn render_conflict(frame: &mut Frame, area: Rect, prompt: &ConflictPrompt) {
    let conflict = &prompt.conflict;
    let relation = match conflict.relation {
        VersionRelation::SourceNewer => "SOURCE is newer",
        VersionRelation::DestinationNewer => "DESTINATION is newer",
        VersionRelation::SameTimestamp => {
            "Same timestamp; contents differ or comparison is unresolved"
        }
        VersionRelation::Unknown => "Older/newer is unknown",
    };
    let mut lines = vec![
        Line::styled(
            "A destination item already exists",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(relation),
        Line::raw(""),
        Line::styled("SOURCE", Style::default().fg(Color::Cyan)),
        Line::raw(safe_text(&conflict.source.to_string_lossy())),
        Line::raw(format!(
            "{} · Modified {}",
            human_size(conflict.source_bytes),
            modified_label(conflict.source_modified)
        )),
        Line::raw(""),
        Line::styled("DESTINATION", Style::default().fg(Color::Cyan)),
        Line::raw(safe_text(&conflict.destination.to_string_lossy())),
        Line::raw(format!(
            "{} · Modified {}",
            human_size(conflict.destination_bytes),
            modified_label(conflict.destination_modified)
        )),
        Line::raw(""),
    ];
    if conflict.kind == ConflictKind::TypeMismatch {
        lines.push(Line::styled(
            "File/folder type mismatch: only choices 4, 5, and 6 are available",
            Style::default().fg(Color::Red),
        ));
    } else {
        lines.push(Line::raw("1 Keep newer       2 Keep older"));
        lines.push(Line::raw("3 Keep source      4 Keep destination"));
    }
    lines.push(Line::raw(
        "5 Keep both        6 Skip (leave source unchanged)",
    ));
    lines.push(Line::styled(
        "During Move, choosing destination/older/newer may discard the losing source; Skip retains it",
        Style::default().fg(Color::Yellow),
    ));
    lines.push(Line::styled(
        format!(
            "A Apply to all {} conflicts: {}",
            if conflict.kind == ConflictKind::FileToFile {
                "file/file"
            } else {
                "type"
            },
            if prompt.apply_to_all { "ON" } else { "OFF" }
        ),
        Style::default().fg(if prompt.apply_to_all {
            Color::Green
        } else {
            Color::DarkGray
        }),
    ));
    lines.push(Line::styled(
        "Esc / X Cancel safely",
        Style::default().fg(Color::Yellow),
    ));
    render_modal(
        frame,
        centered_rect(88, 78, area),
        " Resolve conflict ",
        lines,
        Color::Yellow,
    );
}

fn render_planning(
    frame: &mut Frame,
    area: Rect,
    progress: &dirveyor_domain::OperationPlanningProgress,
) {
    let popup = centered_rect(74, 28, area);
    let mut lines = vec![
        Line::styled(
            format!(
                "{} Counting exact scope for {}…",
                folder_size_spinner(),
                progress.kind.label()
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw(format!(
            "Found: {} files · {} folders · {}",
            progress.discovered_files,
            progress.discovered_directories,
            human_size(progress.discovered_bytes)
        )),
        Line::raw(format!("Entries inspected: {}", progress.discovered_items)),
        Line::raw("Metadata-only scan · no transfer has started."),
        Line::raw("No files have changed."),
    ];
    if let Some(path) = &progress.current_path {
        lines.push(Line::raw(format!(
            "Scanning: {}",
            safe_text(&path.to_string_lossy())
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Esc Request cancellation",
        Style::default().fg(Color::DarkGray),
    ));
    render_modal(
        frame,
        popup,
        &format!(" Plan {} ", progress.kind.label()),
        lines,
        Color::Cyan,
    );
}

fn render_review(frame: &mut Frame, area: Rect, summary: &PlanSummary) {
    let popup = centered_rect(84, 82, area);
    let mut lines = vec![
        Line::styled(
            if summary.recursive_scope_known {
                format!(
                    "{} selected · {} files · {} folders · {} total",
                    summary.sources.len(),
                    summary.file_count,
                    summary.directory_count,
                    human_size(summary.total_bytes)
                )
            } else {
                format!(
                    "{} selected roots · directory contents not pre-enumerated",
                    summary.item_count
                )
            },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(format!("Strategy: {}", summary.strategy.label())),
    ];
    if let Some(destination) = &summary.destination {
        lines.push(Line::raw(format!(
            "Destination: {}",
            safe_text(&destination.to_string_lossy())
        )));
    }
    if let Some(verification) = summary.verification {
        lines.push(Line::raw(format!(
            "Verification: {} · Ctrl+V changes the default before planning",
            verification.label()
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Sources",
        Style::default().fg(Color::DarkGray),
    ));
    for source in summary.sources.iter().take(4) {
        lines.push(Line::raw(format!(
            "  {}",
            safe_text(&source.to_string_lossy())
        )));
    }
    if summary.sources.len() > 4 {
        lines.push(Line::raw(format!(
            "  … and {} more",
            summary.sources.len() - 4
        )));
    }
    if !summary.conflicts.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!(
                "{} name conflicts will use numbered Keep both names",
                summary.conflicts.len()
            ),
            Style::default().fg(Color::Yellow),
        ));
        for conflict in summary.conflicts.iter().take(2) {
            lines.push(Line::raw(format!(
                "  → {}",
                safe_text(&conflict.resolved_destination.to_string_lossy())
            )));
        }
    }
    for warning in &summary.warnings {
        lines.push(Line::styled(
            format!("! {}", safe_text(warning)),
            Style::default().fg(Color::Yellow),
        ));
    }
    lines.push(Line::raw(""));
    if summary.kind == OperationKind::PermanentDelete {
        lines.push(Line::styled(
            "This permanently removes the reviewed items and cannot be undone.",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(
            "Y Yes, permanently delete",
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(
            "N No, return without deleting",
            Style::default().fg(Color::Cyan),
        ));
        lines.push(Line::styled(
            "Esc Back — no files will change",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        lines.push(Line::styled(
            review_confirmation(summary.kind),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(
            "Esc Back — no files will change",
            Style::default().fg(Color::DarkGray),
        ));
    }
    render_modal(
        frame,
        popup,
        &format!(" Review {} ", summary.kind.label()),
        lines,
        match summary.kind {
            OperationKind::Recycle => Color::Yellow,
            OperationKind::PermanentDelete => Color::Red,
            _ => Color::Cyan,
        },
    );
}

fn render_running(frame: &mut Frame, area: Rect, progress: &dirveyor_domain::OperationProgress) {
    let popup = centered_rect(78, 42, area);
    let percent = if !progress.scope_complete {
        None
    } else if progress.kind.is_delete() {
        progress_percent(progress.completed_items, progress.total_items)
    } else {
        progress_percent(progress.completed_bytes, progress.total_bytes)
            .or_else(|| progress_percent(progress.completed_items, progress.total_items))
    };
    let mut lines = vec![
        Line::styled(
            format!(
                "{} · {}",
                progress.kind.label(),
                phase_label(progress.phase)
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        if let Some(percent) = percent {
            Line::raw(progress_bar(percent, 36))
        } else {
            Line::styled(
                format!("{} Streaming · totals still growing", folder_size_spinner()),
                Style::default().fg(Color::Cyan),
            )
        },
    ];
    if progress.kind.is_delete() {
        lines.push(Line::raw(format!(
            "Removed: {} / {}",
            human_size(progress.completed_bytes),
            human_size(progress.total_bytes)
        )));
        lines.push(Line::raw(format!(
            "Files: {} / {}",
            progress.completed_files, progress.total_files
        )));
        lines.push(Line::raw(format!(
            "Folders: {} / {}",
            progress.completed_directories, progress.total_directories
        )));
        lines.push(Line::raw(format!(
            "All entries: {} / {}",
            progress.completed_items, progress.total_items
        )));
        if progress.kind == OperationKind::Recycle && progress.total_directories > 0 {
            lines.push(Line::styled(
                "Windows reports each selected tree only after its Recycle Bin handoff completes",
                Style::default().fg(Color::DarkGray),
            ));
        }
    } else {
        lines.push(Line::raw(if progress.scope_complete {
            format!(
                "Items: {} / {}",
                progress.completed_items, progress.total_items
            )
        } else {
            format!(
                "Entries: {} completed · {} discovered",
                progress.completed_items, progress.total_items
            )
        }));
        lines.push(Line::raw(format!(
            "Files: {} / {}",
            progress.completed_files, progress.total_files
        )));
        lines.push(Line::raw(format!(
            "Folders: {} / {}",
            progress.completed_directories, progress.total_directories
        )));
        lines.push(Line::raw(format!(
            "Transferred: {} / {}",
            human_size(progress.completed_bytes),
            human_size(progress.total_bytes)
        )));
    }
    if let Some(path) = &progress.current_path {
        lines.push(Line::raw(format!(
            "Current: {}",
            safe_text(&path.to_string_lossy())
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        if progress.phase == JobPhase::Cancelling {
            "Cancellation requested · waiting for the current safe step"
        } else {
            "X / C / Esc  Cancel safely"
        },
        Style::default().fg(Color::Yellow),
    ));
    render_modal(frame, popup, " Operation in progress ", lines, Color::Cyan);
}

fn render_finished(frame: &mut Frame, area: Rect, report: &dirveyor_domain::OperationReport) {
    let popup = centered_rect(78, 58, area);
    let color = match report.outcome {
        JobOutcome::Completed => Color::Green,
        JobOutcome::Partial => Color::Yellow,
        JobOutcome::Cancelled => Color::DarkGray,
        JobOutcome::Failed => Color::Red,
    };
    let mut lines = vec![
        Line::styled(
            format!("{} {}", outcome_label(report.outcome), report.kind.label()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Line::raw(if report.kind.is_delete() {
            format!(
                "{} / {} files · {} / {} folders · {} / {} removed",
                report.completed_files,
                report.total_files,
                report.completed_directories,
                report.total_directories,
                human_size(report.completed_bytes),
                human_size(report.total_bytes)
            )
        } else {
            format!(
                "{} of {} items · {} transferred",
                report.completed_items,
                report.total_items,
                human_size(report.completed_bytes)
            )
        }),
    ];
    if report.failures.is_empty() {
        lines.push(Line::raw("Affected panes have been refreshed."));
    } else {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Details",
            Style::default().fg(Color::DarkGray),
        ));
        for failure in report.failures.iter().take(4) {
            let path = failure
                .path
                .as_ref()
                .map(|path| safe_text(&path.to_string_lossy()))
                .unwrap_or_else(|| "Operation".into());
            lines.push(Line::styled(
                format!("{path}: {}", safe_text(&failure.message)),
                Style::default().fg(Color::Red),
            ));
        }
        if report.kind.is_delete()
            && report
                .failures
                .iter()
                .any(|failure| super::elevation_available(&failure.message))
        {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Ctrl+E Open elevated DirVeyor for a new reviewed attempt",
                Style::default().fg(Color::Yellow),
            ));
        } else if cfg!(windows)
            && report.kind.is_delete()
            && super::is_process_elevated()
            && report
                .failures
                .iter()
                .any(|failure| super::is_permission_denied_message(&failure.message))
        {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Already elevated: inspect the path's ownership, ACLs, or filesystem health",
                Style::default().fg(Color::Yellow),
            ));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Enter / Esc  Return to browser",
        Style::default().fg(Color::Cyan),
    ));
    render_modal(frame, popup, " Operation result ", lines, color);
}

fn render_operation_error(frame: &mut Frame, area: Rect, kind: OperationKind, message: &str) {
    let popup = centered_rect(76, 42, area);
    let mut lines = vec![
        Line::styled(
            format!("Could not plan {}", kind.label()),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw(safe_text(message)),
        Line::raw(""),
        Line::styled("No files were changed.", Style::default().fg(Color::Green)),
        Line::raw(""),
        Line::styled("Enter / Esc  Return", Style::default().fg(Color::Cyan)),
    ];
    if kind.is_delete() && super::elevation_available(message) {
        lines.push(Line::styled(
            "Ctrl+E Open elevated DirVeyor for a new reviewed attempt",
            Style::default().fg(Color::Yellow),
        ));
    } else if cfg!(windows)
        && kind.is_delete()
        && super::is_permission_denied_message(message)
        && super::is_process_elevated()
    {
        lines.push(Line::styled(
            "Already elevated: inspect the path's ownership, ACLs, or filesystem health",
            Style::default().fg(Color::Yellow),
        ));
    }
    render_modal(frame, popup, " Operation blocked ", lines, Color::Red);
}

fn render_modal(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    lines: Vec<Line<'static>>,
    color: Color,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_owned())
        .border_style(Style::default().fg(color));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn review_confirmation(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::Copy => "Enter Execute copy",
        OperationKind::Move => "Enter Execute move",
        OperationKind::Recycle => "Enter Move to Recycle Bin / Trash",
        OperationKind::PermanentDelete => "Y Permanently delete · N Cancel",
        OperationKind::Rename => "Enter Execute rename",
        OperationKind::CreateDirectory => "Enter Create folder",
    }
}

fn phase_label(phase: JobPhase) -> &'static str {
    match phase {
        JobPhase::Planning => "planning",
        JobPhase::AwaitingReview => "awaiting review",
        JobPhase::Running => "running",
        JobPhase::Cancelling => "cancelling",
        JobPhase::Verifying => "verifying",
        JobPhase::Finalizing => "finalizing",
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

fn progress_status(progress: &dirveyor_domain::OperationProgress) -> String {
    let percent = if !progress.scope_complete {
        None
    } else if progress.kind.is_delete() {
        progress_percent(progress.completed_items, progress.total_items)
    } else {
        progress_percent(progress.completed_bytes, progress.total_bytes)
            .or_else(|| progress_percent(progress.completed_items, progress.total_items))
    };
    match percent {
        Some(percent) => format!(
            "{} {}% · {}",
            progress.kind.label(),
            percent,
            phase_label(progress.phase)
        ),
        None => format!(
            "{} · {}",
            progress.kind.label(),
            phase_label(progress.phase)
        ),
    }
}

fn progress_percent(completed: u64, total: u64) -> Option<u8> {
    if total == 0 {
        None
    } else {
        Some(((completed as u128 * 100) / total as u128).min(100) as u8)
    }
}

fn progress_bar(percent: u8, width: usize) -> String {
    let filled = (usize::from(percent) * width + 50) / 100;
    format!(
        "[{}{}] {percent}%",
        "█".repeat(filled.min(width)),
        "░".repeat(width.saturating_sub(filled))
    )
}

fn render_too_small(frame: &mut Frame, area: Rect) {
    let text = format!(
        "DirVeyor needs at least {MIN_WIDTH}×{MIN_HEIGHT}\nCurrent terminal: {}×{}\n\nResize the terminal to continue.",
        area.width, area.height
    );
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::Yellow)),
        area,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn safe_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() || is_bidirectional_control(character) {
                char::REPLACEMENT_CHARACTER
            } else {
                character
            }
        })
        .collect()
}

fn is_bidirectional_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

fn truncate(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let suffix = "…";
    let suffix_width = UnicodeWidthStr::width(suffix);
    let target = max_width.saturating_sub(suffix_width);
    let mut width = 0;
    let mut output = String::new();
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if width + character_width > target {
            break;
        }
        output.push(character);
        width += character_width;
    }
    output.push_str(suffix);
    output
}

fn pad_to_width(value: &str, width: usize) -> String {
    let current = UnicodeWidthStr::width(value);
    let mut output = String::with_capacity(value.len() + width.saturating_sub(current));
    output.push_str(value);
    output.extend(std::iter::repeat_n(' ', width.saturating_sub(current)));
    output
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn kind_label(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Parent => "Parent directory",
        EntryKind::Home => "User folder shortcut",
        EntryKind::Favorite => "Favorite folder shortcut",
        EntryKind::Drive => "Drive",
        EntryKind::Directory => "Directory",
        EntryKind::File => "File",
        EntryKind::Symlink => "Symbolic link",
        EntryKind::Other => "Other",
    }
}

fn modified_label(modified: Option<SystemTime>) -> String {
    let Some(modified) = modified else {
        return "—".into();
    };
    let utc = OffsetDateTime::from(modified);
    match UtcOffset::current_local_offset() {
        Ok(offset) => format_datetime(utc.to_offset(offset), ""),
        Err(_) => format_datetime(utc, " UTC"),
    }
}

fn format_datetime(date_time: OffsetDateTime, suffix: &str) -> String {
    let hour = date_time.hour();
    let (hour, period) = match hour {
        0 => (12, "AM"),
        1..=11 => (hour, "AM"),
        12 => (12, "PM"),
        _ => (hour - 12, "PM"),
    };
    format!(
        "{} {}, {} · {}:{:02} {period}{suffix}",
        month_label(date_time.month()),
        date_time.day(),
        date_time.year(),
        hour,
        date_time.minute(),
    )
}

fn month_label(month: time::Month) -> &'static str {
    match month {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dirveyor_domain::{
        FileEntry, JobId, LoadState, OperationPlanningProgress, OperationProgress, PlannedStrategy,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn populated_app() -> AppState {
        let mut app = AppState::new(PathBuf::from("left"), PathBuf::from("right"));
        for pane_id in PaneId::ALL {
            let pane = app.pane_mut(pane_id);
            pane.load_state = LoadState::Ready;
            pane.entries = vec![FileEntry {
                path: pane.location.join("example.txt"),
                display_name: "example.txt".into(),
                kind: EntryKind::File,
                size: Some(1_024),
                modified: None,
                metadata_incomplete: false,
                drive_info: None,
            }];
        }
        app
    }

    fn rendered_screen(app: &AppState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn renders_required_layout_sizes_without_panicking() {
        for (width, height) in [(80, 24), (120, 30), (160, 45)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let app = populated_app();
            terminal.draw(|frame| render(frame, &app)).unwrap();
        }
    }

    #[test]
    fn browse_footer_and_help_advertise_spatial_navigation() {
        let mut app = populated_app();
        let footer = rendered_screen(&app, 120, 30);
        assert!(footer.contains("← Back"));
        assert!(footer.contains("→ Open/Select"));
        assert!(footer.contains("C Copy"));
        assert!(!footer.contains("c Copy"));

        app.help_visible = true;
        let help = rendered_screen(&app, 80, 24);
        assert!(help.contains("← / Backspace"));
        assert!(help.contains("→                   Open folder"));
        assert!(help.contains("F / Ctrl+F"));
    }

    #[test]
    fn inactive_selection_marks_the_source_and_active_destination() {
        let mut app = populated_app();
        app.pane_mut(PaneId::Left)
            .selected
            .insert(PathBuf::from("left").join("example.txt"));
        app.active_pane = PaneId::Right;

        let screen = rendered_screen(&app, 160, 35);
        assert!(screen.contains("[SOURCE →]"));
        assert!(screen.contains("[→ DESTINATION]"));
        assert!(screen.contains("Ctrl+V Verify: Full (SHA-256)"));
    }

    #[test]
    fn file_conflict_declares_versions_and_all_six_choices() {
        let mut app = populated_app();
        app.operation = OperationView::Conflict(ConflictPrompt {
            conflict: dirveyor_domain::TransferConflict {
                job: JobId(44),
                kind: ConflictKind::FileToFile,
                source: PathBuf::from("left/newer.dat"),
                destination: PathBuf::from("right/newer.dat"),
                source_bytes: 2_048,
                destination_bytes: 1_024,
                source_modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2)),
                destination_modified: Some(
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1),
                ),
                relation: VersionRelation::SourceNewer,
            },
            apply_to_all: true,
        });

        let screen = rendered_screen(&app, 160, 40);
        assert!(screen.contains("SOURCE is newer"));
        assert!(screen.contains("1 Keep newer"));
        assert!(screen.contains("2 Keep older"));
        assert!(screen.contains("3 Keep source"));
        assert!(screen.contains("4 Keep destination"));
        assert!(screen.contains("5 Keep both"));
        assert!(screen.contains("6 Skip"));
        assert!(screen.contains("Apply to all file/file conflicts: ON"));
    }

    #[test]
    fn renders_minimum_size_message_for_small_terminal() {
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = populated_app();
        terminal.draw(|frame| render(frame, &app)).unwrap();
    }

    #[test]
    fn truncation_observes_terminal_cell_width() {
        assert_eq!(truncate("ab界cd", 5), "ab界…");
        assert_eq!(UnicodeWidthStr::width(truncate("ab界cd", 5).as_str()), 5);
        assert_eq!(truncate("hello", 0), "");
        assert_eq!(UnicodeWidthStr::width(pad_to_width("界", 4).as_str()), 4);
    }

    #[test]
    fn safe_text_replaces_bidirectional_controls() {
        assert_eq!(safe_text("report\u{202e}fdp.exe"), "report�fdp.exe");
    }

    #[test]
    fn drive_usage_is_calculated_and_rendered() {
        let drive = DriveInfo {
            kind: DriveKind::Fixed,
            label: Some("Data".into()),
            filesystem: Some("NTFS".into()),
            total_bytes: Some(1_000),
            available_bytes: Some(250),
        };

        assert_eq!(drive.used_bytes(), Some(750));
        assert_eq!(drive.used_percent(), Some(75));
        assert_eq!(drive_usage_bar(75, 4), "[███░] 75%");
    }

    #[test]
    fn wide_drive_view_renders_capacity_details_in_inspector() {
        let mut app = populated_app();
        let pane = app.pane_mut(PaneId::Left);
        pane.browsing_drives = true;
        pane.entries = vec![FileEntry {
            path: PathBuf::from("D:\\"),
            display_name: "D:\\  Data".into(),
            kind: EntryKind::Drive,
            size: None,
            modified: None,
            metadata_incomplete: false,
            drive_info: Some(DriveInfo {
                kind: DriveKind::Fixed,
                label: Some("Data".into()),
                filesystem: Some("NTFS".into()),
                total_bytes: Some(1_000),
                available_bytes: Some(250),
            }),
        }];
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Filesystem: NTFS"));
        assert!(screen.contains("Capacity: 1000 B"));
        assert!(screen.contains("Available: 250 B"));
        assert!(screen.contains("75%"));
    }

    #[test]
    fn operation_review_names_the_destination_and_requires_confirmation() {
        let mut app = populated_app();
        app.operation = OperationView::Review(PlanSummary {
            job: JobId(7),
            kind: OperationKind::Move,
            sources: vec![PathBuf::from("left").join("example.txt")],
            destination: Some(PathBuf::from("right")),
            strategy: PlannedStrategy::CopyVerifyRemove,
            item_count: 1,
            file_count: 1,
            directory_count: 0,
            total_bytes: 1_024,
            recursive_scope_known: true,
            conflicts: Vec::new(),
            warnings: vec!["Sources remain until verification succeeds".into()],
            verification: Some(dirveyor_domain::VerificationMode::Full),
        });

        let screen = rendered_screen(&app, 160, 35);
        assert!(screen.contains("Review move"));
        assert!(screen.contains("Destination: right"));
        assert!(screen.contains("Enter Execute move"));
        assert!(screen.contains("Esc Back — no files will change"));
    }

    #[test]
    fn permanent_delete_mode_and_yes_no_confirmation_are_unmistakable() {
        let mut app = populated_app();
        app.delete_mode = dirveyor_domain::DeleteMode::Permanent;
        let browse = rendered_screen(&app, 160, 35);
        assert!(browse.contains("D DELETE"));
        assert!(browse.contains("Ctrl+D Mode"));

        app.operation = OperationView::Review(PlanSummary {
            job: JobId(17),
            kind: OperationKind::PermanentDelete,
            sources: vec![PathBuf::from("left").join("example.txt")],
            destination: None,
            strategy: PlannedStrategy::PermanentDelete,
            item_count: 1,
            file_count: 1,
            directory_count: 0,
            total_bytes: 1_024,
            recursive_scope_known: true,
            conflicts: Vec::new(),
            warnings: vec!["This operation cannot be undone".into()],
            verification: None,
        });

        let review = rendered_screen(&app, 160, 35);
        assert!(review.contains("Review permanent delete"));
        assert!(review.contains("cannot be undone"));
        assert!(review.contains("Y Yes, permanently delete"));
        assert!(review.contains("N No, return without deleting"));
        assert!(!review.contains("Type DELETE"));
    }

    #[test]
    fn operation_progress_exposes_cancel_and_completion_counts() {
        let mut app = populated_app();
        app.operation = OperationView::Running(OperationProgress {
            job: JobId(8),
            kind: OperationKind::Copy,
            phase: JobPhase::Running,
            completed_items: 2,
            total_items: 4,
            completed_files: 2,
            total_files: 4,
            completed_directories: 0,
            total_directories: 0,
            completed_bytes: 512,
            total_bytes: 1_024,
            scope_complete: true,
            current_path: Some(PathBuf::from("right").join("example.txt")),
        });

        let screen = rendered_screen(&app, 120, 30);
        assert!(screen.contains("Operation in progress"));
        assert!(screen.contains("Items: 2 / 4"));
        assert!(screen.contains("Files: 2 / 4"));
        assert!(screen.contains("Folders: 0 / 0"));
        assert!(screen.contains("50%"));
        assert!(screen.contains("Cancel safely"));
    }

    #[test]
    fn permanent_delete_scan_and_progress_show_recursive_scope() {
        let mut app = populated_app();
        app.operation = OperationView::Planning(OperationPlanningProgress {
            job: JobId(18),
            kind: OperationKind::PermanentDelete,
            discovered_items: 1_212,
            discovered_files: 1_000,
            discovered_directories: 212,
            discovered_bytes: 8 * 1024 * 1024,
            current_path: Some(PathBuf::from("large-tree").join("content")),
        });
        let scan = rendered_screen(&app, 140, 35);
        assert!(scan.contains("Found: 1000 files · 212 folders · 8.0 MB"));
        assert!(scan.contains("Entries inspected: 1212"));
        assert!(scan.contains("No files have changed"));

        app.operation = OperationView::Running(OperationProgress {
            job: JobId(18),
            kind: OperationKind::PermanentDelete,
            phase: JobPhase::Running,
            completed_items: 500,
            total_items: 1_212,
            completed_files: 500,
            total_files: 1_000,
            completed_directories: 0,
            total_directories: 212,
            completed_bytes: 4 * 1024 * 1024,
            total_bytes: 8 * 1024 * 1024,
            scope_complete: true,
            current_path: Some(PathBuf::from("large-tree").join("content.bin")),
        });
        let running = rendered_screen(&app, 140, 35);
        assert!(running.contains("Removed: 4.0 MB / 8.0 MB"));
        assert!(running.contains("Files: 500 / 1000"));
        assert!(running.contains("Folders: 0 / 212"));
        assert!(running.contains("All entries: 500 / 1212"));
        assert!(running.contains("41%"));
    }

    #[test]
    fn rename_prompt_shows_editable_name_and_non_mutating_escape_hint() {
        let mut app = populated_app();
        app.text_prompt = Some(TextPrompt {
            action: TextAction::Rename {
                source: PathBuf::from("left").join("example.txt"),
            },
            value: "renamed.txt".into(),
            error: None,
        });

        let screen = rendered_screen(&app, 120, 30);
        assert!(screen.contains("Rename"));
        assert!(screen.contains("renamed.txt"));
        assert!(screen.contains("Esc Cancel"));
    }

    #[test]
    fn focused_folder_renders_contained_size_and_drive_share() {
        let mut app = populated_app();
        let folder_path = PathBuf::from("left").join("folder");
        app.pane_mut(PaneId::Left).entries = vec![FileEntry {
            path: folder_path.clone(),
            display_name: "folder".into(),
            kind: EntryKind::Directory,
            size: None,
            modified: None,
            metadata_incomplete: false,
            drive_info: None,
        }];
        app.folder_size = FolderSizeState::Ready(dirveyor_domain::FolderSizeSummary {
            request_id: 3,
            pane: PaneId::Left,
            generation: 0,
            path: folder_path,
            total_bytes: 250_000_000,
            file_count: 42,
            directory_count: 5,
            skipped_items: 0,
            drive_total_bytes: Some(1_000_000_000),
        });

        let screen = rendered_screen(&app, 160, 35);
        assert!(screen.contains("Contained size: 238 MB"));
        assert!(screen.contains("Drive share: 25.00% of 954 MB"));
        assert!(screen.contains("Contents: 42 files · 4 folders"));
    }

    #[test]
    fn tiny_folder_drive_share_remains_visible() {
        assert_eq!(
            drive_share_label(1, Some(1_000_000_000)),
            "<0.001% of 954 MB"
        );
        assert_eq!(drive_share_label(0, Some(1_024)), "0% of 1.0 KB");
        assert_eq!(drive_share_label(1, None), "Unavailable");
    }

    #[test]
    fn folder_size_progress_renders_live_counts_and_discovered_size() {
        let mut app = populated_app();
        let folder_path = PathBuf::from("left").join("folder");
        app.pane_mut(PaneId::Left).entries = vec![FileEntry {
            path: folder_path.clone(),
            display_name: "folder".into(),
            kind: EntryKind::Directory,
            size: None,
            modified: None,
            metadata_incomplete: false,
            drive_info: None,
        }];
        app.folder_size = FolderSizeState::Loading(dirveyor_domain::FolderSizeProgress {
            request_id: 9,
            pane: PaneId::Left,
            generation: 0,
            path: folder_path,
            discovered_bytes: 524_288,
            file_count: 120,
            directory_count: 8,
            skipped_items: 0,
            drive_total_bytes: Some(1_073_741_824),
        });

        let screen = rendered_screen(&app, 160, 35);
        assert!(screen.contains("Contained size: 512 KB so far"));
        assert!(screen.contains("Drive share: 0.049% of 1.0 GB so far"));
        assert!(screen.contains("Scanned: 120 files · 7 folders"));
    }

    #[test]
    fn modified_time_uses_a_human_readable_calendar_format() {
        let epoch = OffsetDateTime::from_unix_timestamp(0).unwrap();
        assert_eq!(format_datetime(epoch, " UTC"), "Jan 1, 1970 · 12:00 AM UTC");
    }

    #[test]
    fn preview_replaces_the_complete_browse_layout() {
        let mut app = populated_app();
        app.preview = PreviewState::Ready(Box::new(PreviewSession::new(
            dirveyor_domain::PreviewDocument {
                request_id: 1,
                path: PathBuf::from("Latest.log"),
                file_size: 18,
                modified: None,
                kind: dirveyor_domain::PreviewKind::Log,
                encoding: dirveyor_domain::PreviewEncoding::Utf8,
                completeness: dirveyor_domain::PreviewCompleteness::Complete,
                window_start: 0,
                window_end: 18,
                raw_lines: vec!["INFO ready".into(), "ERROR stopped".into()],
                formatted_lines: None,
                format_error: None,
            },
        )));

        let screen = rendered_screen(&app, 120, 30);
        assert!(screen.contains("Preview · Latest.log · Log · Raw · UTF-8"));
        assert!(screen.contains("ERROR stopped"));
        assert!(!screen.contains("Inspector"));
        assert!(!screen.contains("example.txt"));
    }

    #[test]
    fn markdown_split_renders_raw_and_preview_regions() {
        let mut app = populated_app();
        let document = dirveyor_domain::PreviewDocument {
            request_id: 2,
            path: PathBuf::from("README.md"),
            file_size: 12,
            modified: None,
            kind: dirveyor_domain::PreviewKind::Markdown,
            encoding: dirveyor_domain::PreviewEncoding::Utf8,
            completeness: dirveyor_domain::PreviewCompleteness::Complete,
            window_start: 0,
            window_end: 12,
            raw_lines: vec!["# Heading".into()],
            formatted_lines: Some(vec![PreviewLine {
                text: "Heading".into(),
                style: PreviewLineStyle::Heading,
            }]),
            format_error: None,
        };
        let mut session = PreviewSession::new(document);
        session.mode = PreviewMode::Split;
        app.preview = PreviewState::Ready(Box::new(session));

        let screen = rendered_screen(&app, 120, 30);
        assert!(screen.contains("Raw"));
        assert!(screen.contains("Rendered"));
        assert!(screen.contains("# Heading"));
        assert!(screen.contains("Heading"));
    }

    #[test]
    fn windowed_preview_shows_byte_range_and_changed_notice() {
        let mut app = populated_app();
        let document = dirveyor_domain::PreviewDocument {
            request_id: 3,
            path: PathBuf::from("large.log"),
            file_size: 4 * 1024 * 1024,
            modified: None,
            kind: dirveyor_domain::PreviewKind::Log,
            encoding: dirveyor_domain::PreviewEncoding::Utf8,
            completeness: dirveyor_domain::PreviewCompleteness::MiddleWindow,
            window_start: 1024 * 1024,
            window_end: 2 * 1024 * 1024,
            raw_lines: vec!["middle".into()],
            formatted_lines: None,
            format_error: None,
        };
        let mut session = PreviewSession::new(document);
        app.preview = PreviewState::Ready(Box::new(session.clone()));
        let screen = rendered_screen(&app, 160, 30);
        assert!(screen.contains("Middle window · bytes 1048577–2097152"));

        session.source_changed = true;
        app.preview = PreviewState::Ready(Box::new(session));
        let screen = rendered_screen(&app, 160, 30);
        assert!(screen.contains("File changed on disk · R Reload"));
    }

    #[test]
    fn all_drives_renders_user_favorites_and_drive_sections() {
        let mut app = populated_app();
        let pane = app.pane_mut(PaneId::Left);
        pane.browsing_drives = true;
        pane.entries = vec![
            FileEntry {
                path: PathBuf::from(r"C:\Users\tester"),
                display_name: r"tester  C:\Users\tester".into(),
                kind: EntryKind::Home,
                size: None,
                modified: None,
                metadata_incomplete: false,
                drive_info: None,
            },
            FileEntry {
                path: PathBuf::from(r"D:\Projects"),
                display_name: r"Projects  D:\Projects".into(),
                kind: EntryKind::Favorite,
                size: None,
                modified: None,
                metadata_incomplete: false,
                drive_info: None,
            },
            FileEntry {
                path: PathBuf::from("D:\\"),
                display_name: "D:\\".into(),
                kind: EntryKind::Drive,
                size: None,
                modified: None,
                metadata_incomplete: true,
                drive_info: None,
            },
        ];

        let screen = rendered_screen(&app, 120, 30);

        assert!(screen.contains("User folder"));
        assert!(screen.contains("Favorites · F Add/remove"));
        assert!(screen.contains("Projects"));
        assert!(screen.contains("Drives"));
    }

    #[test]
    fn dedicated_favorites_panel_lists_saved_paths_and_controls() {
        let mut app = populated_app();
        app.favorites = vec![PathBuf::from(r"D:\Projects")];
        app.favorites_panel = Some(dirveyor_domain::FavoritesPanel::default());

        let screen = rendered_screen(&app, 120, 30);

        assert!(screen.contains("Favorites · 1 folders"));
        assert!(screen.contains("★ Projects"));
        assert!(screen.contains("F/Delete Remove"));
        assert!(screen.contains("Ctrl+F/Esc Close"));
    }
}
