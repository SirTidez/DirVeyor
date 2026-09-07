use fileadmin_domain::{
    PreviewCompleteness, PreviewDocument, PreviewEncoding, PreviewKind, PreviewLine,
    PreviewLineStyle,
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

const RICH_PREVIEW_LIMIT: u64 = 8 * 1024 * 1024;
const TEXT_WINDOW_BYTES: u64 = 1024 * 1024;
const MAX_PREVIEW_LINES: usize = 100_000;
const MAX_FORMATTED_BYTES: usize = 16 * 1024 * 1024;
const RESULT_CAPACITY: usize = 2;

#[derive(Debug)]
pub struct PreviewEvent {
    pub request_id: u64,
    pub path: PathBuf,
    pub result: Result<PreviewDocument, String>,
}

#[derive(Clone, Debug)]
struct PreviewRequest {
    request_id: u64,
    path: PathBuf,
}

pub struct PreviewLoader {
    pending: Arc<(Mutex<Option<PreviewRequest>>, Condvar)>,
    results: Receiver<PreviewEvent>,
    current_request: Arc<AtomicU64>,
    next_request: AtomicU64,
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
        Self {
            pending,
            results: result_rx,
            current_request,
            next_request: AtomicU64::new(1),
        }
    }

    pub fn request(&self, path: PathBuf) -> u64 {
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        self.current_request.store(request_id, Ordering::Release);
        let (pending, ready) = &*self.pending;
        *pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(PreviewRequest { request_id, path });
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
    }

    pub fn try_recv(&self) -> Result<PreviewEvent, TryRecvError> {
        self.results.try_recv()
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
    let kind = classify_path(&request.path)?;
    let file_size = metadata.len();
    let mut file =
        File::open(&request.path).map_err(|error| format!("Could not open file — {error}"))?;
    let mut sample = vec![0; file_size.min(8192) as usize];
    file.read_exact(&mut sample)
        .map_err(|error| format!("Could not sample file — {error}"))?;
    let encoding = detect_encoding(&sample)?;

    let complete = file_size <= RICH_PREVIEW_LIMIT;
    let tail = !complete && kind == PreviewKind::Log;
    let start = if tail {
        file_size.saturating_sub(TEXT_WINDOW_BYTES)
    } else {
        0
    };
    let length = if complete {
        file_size
    } else {
        TEXT_WINDOW_BYTES.min(file_size.saturating_sub(start))
    };
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
    let text = if tail {
        text.split_once('\n')
            .map(|(_, remainder)| remainder.to_owned())
            .unwrap_or(text)
    } else {
        text
    };
    let line_truncated = text.split('\n').nth(MAX_PREVIEW_LINES).is_some();
    let raw_lines = split_lines(&text, tail);
    let completeness = if complete && !line_truncated {
        PreviewCompleteness::Complete
    } else if tail {
        PreviewCompleteness::TailWindow
    } else {
        PreviewCompleteness::HeadWindow
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
        raw_lines,
        formatted_lines,
        format_error,
    })
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
            let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
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
        };
        let current = AtomicU64::new(6);

        let document = load_preview(&request, &current).unwrap();

        assert_eq!(document.completeness, PreviewCompleteness::TailWindow);
        assert!(document.raw_lines.iter().any(|line| line == "TAIL marker"));
        assert!(document.raw_lines.len() < 10);
    }
}
