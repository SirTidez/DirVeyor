use fileadmin_domain::{
    PreviewCompleteness, PreviewDocument, PreviewEncoding, PreviewKind, PreviewLine,
    PreviewLineStyle,
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

const RICH_PREVIEW_LIMIT: u64 = 8 * 1024 * 1024;
const TEXT_WINDOW_BYTES: u64 = 1024 * 1024;
const MAX_PREVIEW_LINES: usize = 100_000;
const MAX_FORMATTED_BYTES: usize = 16 * 1024 * 1024;
const RESULT_CAPACITY: usize = 2;
const CHANGE_CHECK_INTERVAL: Duration = Duration::from_millis(750);

#[derive(Debug)]
pub struct PreviewEvent {
    pub request_id: u64,
    pub path: PathBuf,
    pub result: Result<PreviewDocument, String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewChangeEvent {
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreviewWatch {
    path: PathBuf,
    file_size: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Debug)]
struct PreviewRequest {
    request_id: u64,
    path: PathBuf,
    target: PreviewWindowTarget,
    expected_identity: Option<(u64, Option<SystemTime>)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewWindowTarget {
    Initial,
    StartingAt(u64),
    EndingAt(u64),
}

pub struct PreviewLoader {
    pending: Arc<(Mutex<Option<PreviewRequest>>, Condvar)>,
    results: Receiver<PreviewEvent>,
    current_request: Arc<AtomicU64>,
    next_request: AtomicU64,
    watch: Arc<(Mutex<Option<PreviewWatch>>, Condvar)>,
    changes: Receiver<PreviewChangeEvent>,
}

impl PreviewLoader {
    pub fn new() -> Self {
        let pending = Arc::new((Mutex::new(None), Condvar::new()));
        let current_request = Arc::new(AtomicU64::new(0));
        let (result_tx, result_rx) = mpsc::sync_channel(RESULT_CAPACITY);
        let worker_pending = Arc::clone(&pending);
        let worker_current = Arc::clone(&current_request);
        thread::Builder::new()
            .name("fileadmin-preview".into())
            .spawn(move || preview_worker(worker_pending, result_tx, worker_current))
            .expect("failed to start preview reader");
        let watch = Arc::new((Mutex::new(None), Condvar::new()));
        let worker_watch = Arc::clone(&watch);
        let (change_tx, change_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("fileadmin-preview-watch".into())
            .spawn(move || preview_watch_worker(worker_watch, change_tx))
            .expect("failed to start preview change watcher");
        Self {
            pending,
            results: result_rx,
            current_request,
            next_request: AtomicU64::new(1),
            watch,
            changes: change_rx,
        }
    }

    pub fn request(&self, path: PathBuf) -> u64 {
        self.enqueue(path, PreviewWindowTarget::Initial, None)
    }

    pub fn request_window(&self, document: &PreviewDocument, target: PreviewWindowTarget) -> u64 {
        self.enqueue(
            document.path.clone(),
            target,
            Some((document.file_size, document.modified)),
        )
    }

    fn enqueue(
        &self,
        path: PathBuf,
        target: PreviewWindowTarget,
        expected_identity: Option<(u64, Option<SystemTime>)>,
    ) -> u64 {
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        self.current_request.store(request_id, Ordering::Release);
        let (pending, ready) = &*self.pending;
        *pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(PreviewRequest {
            request_id,
            path,
            target,
            expected_identity,
        });
        ready.notify_one();
        request_id
    }

    pub fn cancel(&self) {
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        self.current_request.store(request_id, Ordering::Release);
        let (pending, _) = &*self.pending;
        *pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.clear_watch();
    }

    pub fn try_recv(&self) -> Result<PreviewEvent, TryRecvError> {
        self.results.try_recv()
    }

    pub fn watch(&self, document: &PreviewDocument) {
        let (watch, changed) = &*self.watch;
        *watch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(PreviewWatch {
            path: document.path.clone(),
            file_size: document.file_size,
            modified: document.modified,
        });
        changed.notify_one();
    }

    pub fn clear_watch(&self) {
        let (watch, changed) = &*self.watch;
        *watch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        changed.notify_one();
    }

    pub fn try_recv_change(&self) -> Result<PreviewChangeEvent, TryRecvError> {
        self.changes.try_recv()
    }
}

impl Default for PreviewLoader {
    fn default() -> Self {
        Self::new()
    }
}

fn preview_worker(
    pending: Arc<(Mutex<Option<PreviewRequest>>, Condvar)>,
    results: SyncSender<PreviewEvent>,
    current_request: Arc<AtomicU64>,
) {
    loop {
        let request = {
            let (pending, ready) = &*pending;
            let mut slot = pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while slot.is_none() {
                slot = ready
                    .wait(slot)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            slot.take().expect("preview request disappeared")
        };
        let result = load_preview(&request, &current_request);
        if current_request.load(Ordering::Acquire) != request.request_id {
            continue;
        }
        if results
            .send(PreviewEvent {
                request_id: request.request_id,
                path: request.path,
                result,
            })
            .is_err()
        {
            break;
        }
    }
}

fn preview_watch_worker(
    watch: Arc<(Mutex<Option<PreviewWatch>>, Condvar)>,
    changes: SyncSender<PreviewChangeEvent>,
) {
    loop {
        let current = {
            let (watch, changed) = &*watch;
            let mut slot = watch
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while slot.is_none() {
                slot = changed
                    .wait(slot)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            let (slot_after_wait, _) = changed
                .wait_timeout(slot, CHANGE_CHECK_INTERVAL)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(current) = slot_after_wait.clone() else {
                continue;
            };
            current
        };
        let changed_on_disk = fs::metadata(&current.path).map_or(true, |metadata| {
            metadata.len() != current.file_size || metadata.modified().ok() != current.modified
        });
        if !changed_on_disk {
            continue;
        }
        let (slot, _) = &*watch;
        let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let is_current = slot.as_ref().is_some_and(|watched| *watched == current);
        if !is_current {
            continue;
        }
        *slot = None;
        drop(slot);
        match changes.try_send(PreviewChangeEvent { path: current.path }) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
}

fn load_preview(
    request: &PreviewRequest,
    current_request: &AtomicU64,
) -> Result<PreviewDocument, String> {
    if current_request.load(Ordering::Acquire) != request.request_id {
        return Err("Preview cancelled".into());
    }
    let metadata = fs::symlink_metadata(&request.path)
        .map_err(|error| format!("Could not inspect file — {error}"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || is_windows_reparse(&metadata) {
        return Err("Preview supports ordinary files only".into());
    }
    if let Some((expected_size, expected_modified)) = request.expected_identity
        && (metadata.len() != expected_size || metadata.modified().ok() != expected_modified)
    {
        return Err("File changed on disk · r Reload".into());
    }
    let kind = classify_path(&request.path)?;
    let file_size = metadata.len();
    let mut file =
        File::open(&request.path).map_err(|error| format!("Could not open file — {error}"))?;
    let mut sample = vec![0; file_size.min(8192) as usize];
    file.read_exact(&mut sample)
        .map_err(|error| format!("Could not sample file — {error}"))?;
    let encoding = detect_encoding(&sample)?;

    let complete = file_size <= RICH_PREVIEW_LIMIT;
    let (start, end) = select_window(
        &mut file,
        file_size,
        encoding,
        kind,
        request.target,
        complete,
    )?;
    let length = end.saturating_sub(start);
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("Could not seek file — {error}"))?;
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("Could not read file — {error}"))?;
    if current_request.load(Ordering::Acquire) != request.request_id {
        return Err("Preview cancelled".into());
    }
    let (text, encoding) = decode_text(&bytes, encoding, start == 0)?;
    let text = sanitize_text(&text);
    let line_truncated = text.split('\n').nth(MAX_PREVIEW_LINES).is_some();
    let tail = end == file_size && start > 0;
    let raw_lines = split_lines(&text, tail);
    let completeness = if complete && !line_truncated {
        PreviewCompleteness::Complete
    } else if start == 0 {
        PreviewCompleteness::HeadWindow
    } else if end == file_size {
        PreviewCompleteness::TailWindow
    } else {
        PreviewCompleteness::MiddleWindow
    };

    let (formatted_lines, format_error) = if complete && !line_truncated {
        match kind {
            PreviewKind::Markdown => (Some(render_markdown(&text)), None),
            PreviewKind::Json => match pretty_json(&text) {
                Ok(lines) => (Some(lines), None),
                Err(error) => (None, Some(error)),
            },
            _ => (None, None),
        }
    } else {
        (
            None,
            matches!(kind, PreviewKind::Markdown | PreviewKind::Json).then(|| {
                if line_truncated {
                    "Rich formatting is disabled beyond the 100,000-line preview limit".into()
                } else {
                    "Rich formatting is disabled for files larger than 8 MiB".into()
                }
            }),
        )
    };

    Ok(PreviewDocument {
        request_id: request.request_id,
        path: request.path.clone(),
        file_size,
        modified: metadata.modified().ok(),
        kind,
        encoding,
        completeness,
        window_start: start,
        window_end: end,
        raw_lines,
        formatted_lines,
        format_error,
    })
}

fn select_window(
    file: &mut File,
    file_size: u64,
    encoding: PreviewEncoding,
    kind: PreviewKind,
    target: PreviewWindowTarget,
    complete: bool,
) -> Result<(u64, u64), String> {
    if complete {
        return Ok((0, file_size));
    }
    let target = match target {
        PreviewWindowTarget::Initial if kind == PreviewKind::Log => {
            PreviewWindowTarget::EndingAt(file_size)
        }
        PreviewWindowTarget::Initial => PreviewWindowTarget::StartingAt(0),
        target => target,
    };
    let (nominal_start, nominal_end) = match target {
        PreviewWindowTarget::Initial => unreachable!("initial target was resolved above"),
        PreviewWindowTarget::StartingAt(start) => {
            let start = start.min(file_size);
            (
                start,
                start.saturating_add(TEXT_WINDOW_BYTES).min(file_size),
            )
        }
        PreviewWindowTarget::EndingAt(end) => {
            let end = end.min(file_size);
            (end.saturating_sub(TEXT_WINDOW_BYTES), end)
        }
    };
    let start = align_boundary(file, nominal_start, file_size, encoding)?;
    let end = align_boundary(file, nominal_end, file_size, encoding)?;
    if start >= end && start < file_size {
        return Ok((
            start,
            file_size.min(start.saturating_add(TEXT_WINDOW_BYTES)),
        ));
    }
    Ok((start, end))
}

fn align_boundary(
    file: &mut File,
    offset: u64,
    file_size: u64,
    encoding: PreviewEncoding,
) -> Result<u64, String> {
    let offset = offset.min(file_size);
    if offset == 0 || offset == file_size {
        return Ok(offset);
    }
    if matches!(
        encoding,
        PreviewEncoding::Utf16Le | PreviewEncoding::Utf16Be
    ) {
        return Ok(offset - (offset % 2));
    }
    let probe_start = offset.saturating_sub(3);
    let mut probe = [0_u8; 4];
    file.seek(SeekFrom::Start(probe_start))
        .map_err(|error| format!("Could not seek file boundary — {error}"))?;
    let count = file
        .read(&mut probe)
        .map_err(|error| format!("Could not inspect file boundary — {error}"))?;
    let relative = (offset - probe_start) as usize;
    let mut boundary = relative.min(count);
    while boundary > 0 && boundary < count && probe[boundary] & 0b1100_0000 == 0b1000_0000 {
        boundary -= 1;
    }
    Ok(probe_start + boundary as u64)
}

fn classify_path(path: &Path) -> Result<PreviewKind, String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        extension.as_str(),
        "exe"
            | "dll"
            | "com"
            | "bin"
            | "obj"
            | "lib"
            | "pdb"
            | "class"
            | "jar"
            | "wasm"
            | "zip"
            | "7z"
            | "rar"
            | "gz"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "bmp"
            | "ico"
            | "pdf"
            | "mp3"
            | "wav"
            | "mp4"
            | "mkv"
    ) {
        return Err("This appears to be a compiled or binary file".into());
    }
    let kind = if matches!(extension.as_str(), "md" | "markdown" | "mdown") {
        PreviewKind::Markdown
    } else if extension == "json" {
        PreviewKind::Json
    } else if matches!(extension.as_str(), "log" | "out" | "err") {
        PreviewKind::Log
    } else if matches!(
        extension.as_str(),
        "conf"
            | "config"
            | "cfg"
            | "ini"
            | "toml"
            | "yaml"
            | "yml"
            | "properties"
            | "env"
            | "editorconfig"
            | "jsonc"
    ) || matches!(
        name.as_str(),
        ".env" | ".editorconfig" | ".gitignore" | ".gitattributes"
    ) {
        PreviewKind::Config
    } else if matches!(
        extension.as_str(),
        "rs" | "c"
            | "h"
            | "cc"
            | "cpp"
            | "cxx"
            | "hpp"
            | "cs"
            | "java"
            | "kt"
            | "kts"
            | "go"
            | "swift"
            | "py"
            | "rb"
            | "php"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "sh"
            | "bash"
            | "zsh"
            | "ps1"
            | "bat"
            | "cmd"
            | "sql"
            | "html"
            | "htm"
            | "xml"
            | "css"
            | "scss"
            | "sass"
            | "lua"
            | "proto"
    ) {
        PreviewKind::Source
    } else {
        PreviewKind::Text
    };
    Ok(kind)
}

fn detect_encoding(sample: &[u8]) -> Result<PreviewEncoding, String> {
    if sample.starts_with(&[0xff, 0xfe]) {
        return Ok(PreviewEncoding::Utf16Le);
    }
    if sample.starts_with(&[0xfe, 0xff]) {
        return Ok(PreviewEncoding::Utf16Be);
    }
    if sample.contains(&0) {
        return Err("This file contains binary NUL bytes".into());
    }
    let controls = sample
        .iter()
        .filter(|byte| **byte < 0x09 || (**byte > 0x0d && **byte < 0x20))
        .count();
    if !sample.is_empty() && controls * 20 > sample.len() {
        return Err("This file contains too many binary control bytes".into());
    }
    Ok(if std::str::from_utf8(sample).is_ok() {
        PreviewEncoding::Utf8
    } else {
        PreviewEncoding::LossyUtf8
    })
}

fn decode_text(
    bytes: &[u8],
    encoding: PreviewEncoding,
    starts_at_file_beginning: bool,
) -> Result<(String, PreviewEncoding), String> {
    match encoding {
        PreviewEncoding::Utf16Le | PreviewEncoding::Utf16Be => {
            let bytes = if starts_at_file_beginning && bytes.len() >= 2 {
                &bytes[2..]
            } else {
                bytes
            };
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| match encoding {
                    PreviewEncoding::Utf16Le => u16::from_le_bytes([pair[0], pair[1]]),
                    _ => u16::from_be_bytes([pair[0], pair[1]]),
                })
                .collect();
            String::from_utf16(&units)
                .map(|text| (text, encoding))
                .map_err(|_| "The UTF-16 file contains invalid text".into())
        }
        PreviewEncoding::Utf8 => {
            let bytes = if starts_at_file_beginning {
                bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes)
            } else {
                bytes
            };
            match String::from_utf8(bytes.to_vec()) {
                Ok(text) => Ok((text, PreviewEncoding::Utf8)),
                Err(error) => Ok((
                    String::from_utf8_lossy(error.as_bytes()).into_owned(),
                    PreviewEncoding::LossyUtf8,
                )),
            }
        }
        PreviewEncoding::LossyUtf8 => Ok((
            String::from_utf8_lossy(bytes).into_owned(),
            PreviewEncoding::LossyUtf8,
        )),
    }
}

fn sanitize_text(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\n' | '\r' => character,
            '\t' => ' ',
            character if character.is_control() || is_bidi_control(character) => {
                char::REPLACEMENT_CHARACTER
            }
            character => character,
        })
        .collect()
}

fn split_lines(text: &str, tail: bool) -> Vec<String> {
    let clean = |line: &str| line.strip_suffix('\r').unwrap_or(line).to_owned();
    let mut lines: Vec<_> = if tail {
        let mut lines: Vec<_> = text
            .rsplit('\n')
            .take(MAX_PREVIEW_LINES)
            .map(clean)
            .collect();
        lines.reverse();
        lines
    } else {
        text.split('\n')
            .take(MAX_PREVIEW_LINES)
            .map(clean)
            .collect()
    };
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn pretty_json(text: &str) -> Result<Vec<PreviewLine>, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|error| {
        format!(
            "Invalid JSON at line {}, column {}: {}",
            error.line(),
            error.column(),
            error
        )
    })?;
    let pretty = serde_json::to_string_pretty(&value)
        .map_err(|error| format!("Could not format JSON: {error}"))?;
    if pretty.len() > MAX_FORMATTED_BYTES {
        return Err("Pretty JSON exceeds the 16 MiB formatted-output limit".into());
    }
    Ok(split_lines(&pretty, false)
        .into_iter()
        .map(|text| PreviewLine {
            text,
            style: PreviewLineStyle::Code,
        })
        .collect())
}

fn render_markdown(text: &str) -> Vec<PreviewLine> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES;
    let mut output = Vec::new();
    let mut current = String::new();
    let mut style = PreviewLineStyle::Normal;
    let mut list_depth = 0_usize;
    for event in Parser::new_ext(text, options) {
        if output.len() >= MAX_PREVIEW_LINES {
            break;
        }
        match event {
            Event::Start(Tag::Heading { .. }) => style = PreviewLineStyle::Heading,
            Event::Start(Tag::BlockQuote(_)) => {
                style = PreviewLineStyle::Quote;
                current.push_str("│ ");
            }
            Event::Start(Tag::CodeBlock(_)) => style = PreviewLineStyle::Code,
            Event::Start(Tag::List(_)) => list_depth = list_depth.saturating_add(1),
            Event::Start(Tag::Item) => {
                style = PreviewLineStyle::List;
                current.push_str(&"  ".repeat(list_depth.saturating_sub(1)));
                current.push_str("• ");
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                current.push_str("[Image: ");
                current.push_str(&dest_url);
                current.push_str("] ");
            }
            Event::Text(value) | Event::Html(value) | Event::InlineHtml(value) => {
                current.push_str(&value)
            }
            Event::Code(value) => {
                current.push('`');
                current.push_str(&value);
                current.push('`');
            }
            Event::SoftBreak => current.push(' '),
            Event::HardBreak => flush_markdown_line(&mut output, &mut current, style),
            Event::Rule => output.push(PreviewLine {
                text: "────────────────────────────────────────".into(),
                style: PreviewLineStyle::Rule,
            }),
            Event::TaskListMarker(checked) => {
                current.push_str(if checked { "[x] " } else { "[ ] " })
            }
            Event::End(TagEnd::Heading(_))
            | Event::End(TagEnd::Paragraph)
            | Event::End(TagEnd::Item)
            | Event::End(TagEnd::CodeBlock)
            | Event::End(TagEnd::TableRow) => {
                flush_markdown_line(&mut output, &mut current, style);
                style = PreviewLineStyle::Normal;
            }
            Event::End(TagEnd::List(_)) => list_depth = list_depth.saturating_sub(1),
            _ => {}
        }
    }
    flush_markdown_line(&mut output, &mut current, style);
    output.truncate(MAX_PREVIEW_LINES);
    if output.is_empty() {
        output.push(PreviewLine {
            text: String::new(),
            style: PreviewLineStyle::Normal,
        });
    }
    output
}

fn flush_markdown_line(
    output: &mut Vec<PreviewLine>,
    current: &mut String,
    style: PreviewLineStyle,
) {
    if !current.is_empty() {
        let text = std::mem::take(current);
        for line in text.split('\n') {
            if !line.is_empty() {
                output.push(PreviewLine {
                    text: line.to_owned(),
                    style,
                });
            }
        }
    }
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(windows)]
fn is_windows_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_windows_reparse(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn classifies_requested_text_families() {
        assert_eq!(
            classify_path(Path::new("README.md")).unwrap(),
            PreviewKind::Markdown
        );
        assert_eq!(
            classify_path(Path::new("data.json")).unwrap(),
            PreviewKind::Json
        );
        assert_eq!(
            classify_path(Path::new("Latest.log")).unwrap(),
            PreviewKind::Log
        );
        assert_eq!(
            classify_path(Path::new("main.cpp")).unwrap(),
            PreviewKind::Source
        );
        assert_eq!(
            classify_path(Path::new("settings.conf")).unwrap(),
            PreviewKind::Config
        );
        assert!(classify_path(Path::new("app.exe")).is_err());
    }

    #[test]
    fn markdown_rendering_keeps_structure_as_text() {
        let lines = render_markdown("# Heading\n\n- one\n- two\n\n```rs\nlet x = 1;\n```");
        assert!(
            lines
                .iter()
                .any(|line| line.text == "Heading" && line.style == PreviewLineStyle::Heading)
        );
        assert!(lines.iter().any(|line| line.text.contains("• one")));
        assert!(lines.iter().any(|line| line.text.contains("let x = 1;")));
    }

    #[test]
    fn json_reports_location_and_pretty_prints_valid_input() {
        let lines = pretty_json("{\"value\":1}").unwrap();
        assert!(lines.iter().any(|line| line.text.contains("\"value\": 1")));
        assert!(pretty_json("{]").unwrap_err().contains("line 1"));
    }

    #[test]
    fn control_sequences_are_neutralized() {
        assert_eq!(
            sanitize_text("safe\u{1b}[31m\u{202e}name"),
            "safe�[31m�name"
        );
    }

    #[test]
    fn loads_a_log_as_read_only_text() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Latest.log");
        fs::write(&path, b"INFO started\nERROR stopped\n").unwrap();
        let request = PreviewRequest {
            request_id: 5,
            path,
            target: PreviewWindowTarget::Initial,
            expected_identity: None,
        };
        let current = AtomicU64::new(5);

        let document = load_preview(&request, &current).unwrap();

        assert_eq!(document.kind, PreviewKind::Log);
        assert_eq!(document.completeness, PreviewCompleteness::Complete);
        assert_eq!(document.raw_lines[1], "ERROR stopped");
    }

    #[test]
    fn large_logs_open_as_a_bounded_tail_window() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("large.log");
        let mut file = File::create(&path).unwrap();
        let block = vec![b'x'; 8192];
        for _ in 0..=(RICH_PREVIEW_LIMIT / block.len() as u64) {
            file.write_all(&block).unwrap();
        }
        file.seek(SeekFrom::End(-12)).unwrap();
        file.write_all(b"\nTAIL marker").unwrap();
        let request = PreviewRequest {
            request_id: 6,
            path,
            target: PreviewWindowTarget::Initial,
            expected_identity: None,
        };
        let current = AtomicU64::new(6);

        let document = load_preview(&request, &current).unwrap();

        assert_eq!(document.completeness, PreviewCompleteness::TailWindow);
        assert!(document.raw_lines.iter().any(|line| line == "TAIL marker"));
        assert!(document.raw_lines.len() < 10);

        let middle = PreviewRequest {
            request_id: 6,
            path: document.path.clone(),
            target: PreviewWindowTarget::StartingAt(TEXT_WINDOW_BYTES),
            expected_identity: Some((document.file_size, document.modified)),
        };
        let middle = load_preview(&middle, &current).unwrap();
        assert_eq!(middle.completeness, PreviewCompleteness::MiddleWindow);
        assert_eq!(middle.window_start, TEXT_WINDOW_BYTES);
        assert_eq!(middle.window_end, TEXT_WINDOW_BYTES * 2);
    }

    #[test]
    fn adjacent_utf8_windows_share_a_safe_character_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("unicode.txt");
        let mut bytes = vec![b'a'; TEXT_WINDOW_BYTES as usize - 1];
        bytes.extend_from_slice("€after".as_bytes());
        fs::write(&path, &bytes).unwrap();
        let mut file = File::open(&path).unwrap();

        let first = select_window(
            &mut file,
            bytes.len() as u64,
            PreviewEncoding::Utf8,
            PreviewKind::Text,
            PreviewWindowTarget::StartingAt(0),
            false,
        )
        .unwrap();
        let second = select_window(
            &mut file,
            bytes.len() as u64,
            PreviewEncoding::Utf8,
            PreviewKind::Text,
            PreviewWindowTarget::StartingAt(first.1),
            false,
        )
        .unwrap();

        assert_eq!(first.1, second.0);
        assert_eq!(first.1, TEXT_WINDOW_BYTES - 1);
        file.seek(SeekFrom::Start(second.0)).unwrap();
        let mut second_bytes = vec![0; (second.1 - second.0) as usize];
        file.read_exact(&mut second_bytes).unwrap();
        assert!(std::str::from_utf8(&second_bytes).unwrap().starts_with('€'));
    }

    #[test]
    fn utf16_windows_are_aligned_to_complete_code_units() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("unicode.log");
        fs::write(&path, vec![0_u8; 64]).unwrap();
        let mut file = File::open(&path).unwrap();

        let window = select_window(
            &mut file,
            64,
            PreviewEncoding::Utf16Le,
            PreviewKind::Log,
            PreviewWindowTarget::StartingAt(7),
            false,
        )
        .unwrap();

        assert_eq!(window.0, 6);
        assert_eq!(window.0 % 2, 0);
        assert_eq!(window.1 % 2, 0);
    }

    #[test]
    fn adjacent_window_rejects_a_changed_source_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("changed.log");
        fs::write(&path, b"current contents").unwrap();
        let request = PreviewRequest {
            request_id: 9,
            path,
            target: PreviewWindowTarget::StartingAt(0),
            expected_identity: Some((1, None)),
        };

        let error = load_preview(&request, &AtomicU64::new(9)).unwrap_err();

        assert_eq!(error, "File changed on disk · r Reload");
    }

    #[test]
    fn watcher_reports_a_changed_source_without_reading_its_contents() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("watched.log");
        fs::write(&path, b"before").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let loader = PreviewLoader::new();
        loader.watch(&PreviewDocument {
            request_id: 1,
            path: path.clone(),
            file_size: metadata.len(),
            modified: metadata.modified().ok(),
            kind: PreviewKind::Log,
            encoding: PreviewEncoding::Utf8,
            completeness: PreviewCompleteness::Complete,
            window_start: 0,
            window_end: metadata.len(),
            raw_lines: Vec::new(),
            formatted_lines: None,
            format_error: None,
        });
        fs::write(&path, b"after with a different length").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let change = loop {
            match loader.try_recv_change() {
                Ok(change) => break change,
                Err(TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(25));
                }
                result => panic!("change event was not received: {result:?}"),
            }
        };
        assert_eq!(change.path, path);
    }
}
