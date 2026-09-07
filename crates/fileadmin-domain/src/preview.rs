use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewKind {
    Markdown,
    Json,
    Config,
    Log,
    Source,
    Text,
}

impl PreviewKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Json => "JSON",
            Self::Config => "Configuration",
            Self::Log => "Log",
            Self::Source => "Source",
            Self::Text => "Text",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    LossyUtf8,
}

impl PreviewEncoding {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::LossyUtf8 => "UTF-8 (lossy)",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewCompleteness {
    Complete,
    HeadWindow,
    TailWindow,
}

impl PreviewCompleteness {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Complete => "Complete file",
            Self::HeadWindow => "Head window",
            Self::TailWindow => "Tail window",
        }
    }

    pub const fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewLineStyle {
    Normal,
    Heading,
    Quote,
    Code,
    List,
    Rule,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewLine {
    pub text: String,
    pub style: PreviewLineStyle,
}

#[derive(Clone, Debug)]
pub struct PreviewDocument {
    pub request_id: u64,
    pub path: PathBuf,
    pub file_size: u64,
    pub modified: Option<SystemTime>,
    pub kind: PreviewKind,
    pub encoding: PreviewEncoding,
    pub completeness: PreviewCompleteness,
    pub raw_lines: Vec<String>,
    pub formatted_lines: Option<Vec<PreviewLine>>,
    pub format_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewMode {
    Raw,
    Split,
    Formatted,
}

impl PreviewMode {
    pub const fn label(self, kind: PreviewKind) -> &'static str {
        match (self, kind) {
            (Self::Raw, _) => "Raw",
            (Self::Split, _) => "Raw + Preview",
            (Self::Formatted, PreviewKind::Json) => "Pretty",
            (Self::Formatted, _) => "Preview",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewRegion {
    Raw,
    Formatted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewSearchMode {
    Literal,
    Regex,
}

impl PreviewSearchMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Literal => "Literal",
            Self::Regex => "Regex",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewMatch {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug)]
pub struct PreviewSearch {
    pub editing: bool,
    pub query: String,
    pub mode: PreviewSearchMode,
    pub case_sensitive: bool,
    pub matches: Vec<PreviewMatch>,
    pub current: Option<usize>,
    pub error: Option<String>,
}

impl Default for PreviewSearch {
    fn default() -> Self {
        Self {
            editing: false,
            query: String::new(),
            mode: PreviewSearchMode::Literal,
            case_sensitive: false,
            matches: Vec::new(),
            current: None,
            error: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreviewSession {
    pub document: PreviewDocument,
    pub mode: PreviewMode,
    pub active_region: PreviewRegion,
    pub raw_scroll: usize,
    pub formatted_scroll: usize,
    pub horizontal_scroll: usize,
    pub wrap: bool,
    pub search: PreviewSearch,
    pub notice: Option<String>,
    pub help_visible: bool,
}

impl PreviewSession {
    pub fn new(document: PreviewDocument) -> Self {
        let mode = match document.kind {
            PreviewKind::Markdown | PreviewKind::Json if document.formatted_lines.is_some() => {
                PreviewMode::Formatted
            }
            _ => PreviewMode::Raw,
        };
        Self {
            document,
            mode,
            active_region: PreviewRegion::Formatted,
            raw_scroll: 0,
            formatted_scroll: 0,
            horizontal_scroll: 0,
            wrap: false,
            search: PreviewSearch::default(),
            notice: None,
            help_visible: false,
        }
    }

    pub fn active_lines(&self) -> Vec<&str> {
        if self.mode == PreviewMode::Raw || self.active_region == PreviewRegion::Raw {
            self.document.raw_lines.iter().map(String::as_str).collect()
        } else {
            self.document
                .formatted_lines
                .as_ref()
                .map(|lines| lines.iter().map(|line| line.text.as_str()).collect())
                .unwrap_or_else(|| self.document.raw_lines.iter().map(String::as_str).collect())
        }
    }

    pub fn active_scroll_mut(&mut self) -> &mut usize {
        if self.mode == PreviewMode::Raw || self.active_region == PreviewRegion::Raw {
            &mut self.raw_scroll
        } else {
            &mut self.formatted_scroll
        }
    }
}

#[derive(Clone, Debug)]
pub enum PreviewState {
    Closed,
    Loading {
        request_id: u64,
        path: PathBuf,
    },
    Ready(Box<PreviewSession>),
    Failed {
        request_id: u64,
        path: PathBuf,
        message: String,
    },
}

impl PreviewState {
    pub const fn is_open(&self) -> bool {
        !matches!(self, Self::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_documents_open_formatted() {
        let document = PreviewDocument {
            request_id: 1,
            path: PathBuf::from("README.md"),
            file_size: 4,
            modified: None,
            kind: PreviewKind::Markdown,
            encoding: PreviewEncoding::Utf8,
            completeness: PreviewCompleteness::Complete,
            raw_lines: vec!["# Hi".into()],
            formatted_lines: Some(vec![PreviewLine {
                text: "Hi".into(),
                style: PreviewLineStyle::Heading,
            }]),
            format_error: None,
        };
        let session = PreviewSession::new(document);
        assert_eq!(session.mode, PreviewMode::Formatted);
    }
}
