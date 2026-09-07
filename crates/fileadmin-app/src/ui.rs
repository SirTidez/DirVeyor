use fileadmin_domain::{AppState, DriveInfo, DriveKind, EntryKind, LoadState, PaneId, PaneState};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::time::{SystemTime, UNIX_EPOCH};
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

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);

    render_header(frame, header, app);
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
        );
        render_pane(
            frame,
            right,
            app.pane(PaneId::Right),
            app.active_pane == PaneId::Right,
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
        );
        render_pane(
            frame,
            right,
            app.pane(PaneId::Right),
            app.active_pane == PaneId::Right,
        );
    }
    render_footer(frame, footer, app);

    if app.help_visible {
        render_help(frame, area);
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &AppState) {
    let selection = app.selected_count();
    let status = app
        .notice
        .as_deref()
        .unwrap_or("Browse · filesystem writes disabled");
    let line = Line::from(vec![
        Span::styled(
            " FileAdmin ",
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

fn render_pane(frame: &mut Frame, area: Rect, pane: &PaneState, active: bool) {
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
    let title = format!(" {} ", truncate(&location, path_width));
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
                    "Enter Retry · Backspace Parent",
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
    if indices.is_empty() {
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

    let height = area.height as usize;
    let start = pane.cursor.saturating_sub(height.saturating_sub(1));
    let end = (start + height).min(indices.len());
    let name_width = area.width.saturating_sub(16) as usize;
    let mut lines = Vec::with_capacity(end - start);

    for (visible_position, &entry_index) in indices[start..end].iter().enumerate() {
        let row = start + visible_position;
        let entry = &pane.entries[entry_index];
        let focused = active && row == pane.cursor;
        let selected = pane.selected.contains(&entry.path);
        let cursor = if focused { ">" } else { " " };
        let check = if selected { "[x]" } else { "[ ]" };
        let kind = match entry.kind {
            EntryKind::Parent => "↑",
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
            .unwrap_or_else(|| "—".into());
        let line = format!("{cursor}{check} {kind}{name} {size:>8}");
        let style = if focused {
            Style::default()
                .fg(Color::Cyan)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else if selected {
            Style::default().fg(Color::Cyan)
        } else if entry.kind == EntryKind::Directory {
            Style::default().fg(Color::LightBlue)
        } else {
            Style::default()
        };
        lines.push(Line::styled(line, style));
    }
    frame.render_widget(Paragraph::new(lines), area);
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
        } else {
            lines.push(Line::raw(format!("Type: {}", kind_label(entry.kind))));
            lines.push(Line::raw(format!(
                "Size: {}",
                entry.size.map(human_size).unwrap_or_else(|| "—".into())
            )));
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
        Line::styled(
            "READ-ONLY · copy, move, rename, and delete are disabled",
            Style::default().fg(Color::Yellow),
        ),
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
        " Tab Pane  ↑↓/jk Navigate  Space Select  Enter Open  / Filter  h Hidden  s Sort  F1 Help  q Quit".into()
    } else {
        " Tab Pane  ↑↓ Navigate  Space Select  Enter Open  / Filter  F1 Help  q Quit".into()
    };
    frame.render_widget(
        Paragraph::new(truncate(&text, area.width as usize))
            .style(Style::default().fg(Color::Black).bg(Color::Gray)),
        area,
    );
}

fn render_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(72, 78, area);
    let lines = vec![
        Line::styled(
            "FileAdmin read-only controls",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw("Tab / Shift+Tab   Switch pane"),
        Line::raw("↑ ↓ or j k         Move focus"),
        Line::raw("Home / End         First / last visible item"),
        Line::raw("Space              Toggle selection"),
        Line::raw("Enter              Open folder or retry failed scan"),
        Line::raw("Backspace          Parent folder"),
        Line::raw("/                  Filter active pane"),
        Line::raw("h                  Toggle dotfiles"),
        Line::raw("s                  Cycle sort field"),
        Line::raw("q / Ctrl+C         Quit"),
        Line::raw(""),
        Line::styled(
            "Copy, move, rename, delete, queue, and external file opening are intentionally disabled.",
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

fn render_too_small(frame: &mut Frame, area: Rect) {
    let text = format!(
        "FileAdmin needs at least {MIN_WIDTH}×{MIN_HEIGHT}\nCurrent terminal: {}×{}\n\nResize the terminal to continue.",
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
    let Ok(duration) = modified.duration_since(UNIX_EPOCH) else {
        return "Before 1970".into();
    };
    format!("Unix {}", duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fileadmin_domain::{FileEntry, LoadState};
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
}
