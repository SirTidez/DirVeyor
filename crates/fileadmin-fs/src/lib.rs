//! Read-only filesystem scanning on bounded background workers.

use fileadmin_domain::{EntryKind, FileEntry, PaneId};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;

const REQUEST_CAPACITY: usize = 8;
const RESULT_CAPACITY: usize = 8;
const WORKER_COUNT: usize = 2;
pub const MAX_DIRECTORY_ENTRIES: usize = 50_000;

#[derive(Clone, Debug)]
pub struct ScanRequest {
    pub pane: PaneId,
    pub generation: u64,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct ScanEvent {
    pub pane: PaneId,
    pub generation: u64,
    pub path: PathBuf,
    pub result: Result<DirectoryListing, ScanError>,
}

#[derive(Debug)]
pub struct DirectoryListing {
    pub entries: Vec<FileEntry>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanError {
    pub kind: ScanErrorKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanErrorKind {
    NotFound,
    PermissionDenied,
    Unavailable,
    InvalidPath,
    Other,
}

impl ScanError {
    fn from_io(error: io::Error) -> Self {
        let kind = match error.kind() {
            io::ErrorKind::NotFound => ScanErrorKind::NotFound,
            io::ErrorKind::PermissionDenied => ScanErrorKind::PermissionDenied,
            io::ErrorKind::NotConnected | io::ErrorKind::NetworkUnreachable => {
                ScanErrorKind::Unavailable
            }
            io::ErrorKind::InvalidInput => ScanErrorKind::InvalidPath,
            _ => ScanErrorKind::Other,
        };
        let message = match kind {
            ScanErrorKind::NotFound => "Folder no longer exists".into(),
            ScanErrorKind::PermissionDenied => "Permission denied — cannot read this folder".into(),
            ScanErrorKind::Unavailable => {
                "Location unavailable — drive or share may be disconnected".into()
            }
            ScanErrorKind::InvalidPath => "The folder path is invalid".into(),
            ScanErrorKind::Other => format!("Could not read directory — {error}"),
        };
        Self { kind, message }
    }
}

#[derive(Debug)]
pub enum RequestError {
    Busy,
    Closed,
}

pub struct DirectoryScanner {
    requests: SyncSender<ScanRequest>,
    results: Receiver<ScanEvent>,
}

impl DirectoryScanner {
    pub fn new() -> Self {
        let (request_tx, request_rx) = mpsc::sync_channel::<ScanRequest>(REQUEST_CAPACITY);
        let (result_tx, result_rx) = mpsc::sync_channel::<ScanEvent>(RESULT_CAPACITY);
        let shared_requests = Arc::new(Mutex::new(request_rx));

        for worker_number in 0..WORKER_COUNT {
            let requests = Arc::clone(&shared_requests);
            let results = result_tx.clone();
            thread::Builder::new()
                .name(format!("fileadmin-scan-{worker_number}"))
                .spawn(move || worker_loop(requests, results))
                .expect("failed to start directory scanner");
        }

        Self {
            requests: request_tx,
            results: result_rx,
        }
    }

    pub fn request(&self, request: ScanRequest) -> Result<(), RequestError> {
        self.requests
            .try_send(request)
            .map_err(|error| match error {
                TrySendError::Full(_) => RequestError::Busy,
                TrySendError::Disconnected(_) => RequestError::Closed,
            })
    }

    pub fn try_recv(&self) -> Result<ScanEvent, TryRecvError> {
        self.results.try_recv()
    }
}

impl Default for DirectoryScanner {
    fn default() -> Self {
        Self::new()
    }
}

fn worker_loop(requests: Arc<Mutex<Receiver<ScanRequest>>>, results: SyncSender<ScanEvent>) {
    loop {
        let request = {
            let receiver = requests.lock().expect("directory request queue poisoned");
            receiver.recv()
        };
        let Ok(request) = request else {
            break;
        };
        let event = ScanEvent {
            pane: request.pane,
            generation: request.generation,
            path: request.path.clone(),
            result: scan_directory(&request.path),
        };
        if results.send(event).is_err() {
            break;
        }
    }
}

pub fn scan_directory(path: &Path) -> Result<DirectoryListing, ScanError> {
    let reader = fs::read_dir(path).map_err(ScanError::from_io)?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in reader {
        if entries.len() == MAX_DIRECTORY_ENTRIES {
            truncated = true;
            break;
        }
        let item = match item {
            Ok(item) => item,
            Err(_) => continue,
        };
        let path = item.path();
        let display_name = sanitize_display_name(&item.file_name().to_string_lossy());
        let file_type = item.file_type();
        let file_type_missing = file_type.is_err();
        let metadata = item.metadata();
        let kind = match file_type {
            Ok(file_type) => {
                if file_type.is_dir() {
                    EntryKind::Directory
                } else if file_type.is_file() {
                    EntryKind::File
                } else if file_type.is_symlink() {
                    EntryKind::Symlink
                } else {
                    EntryKind::Other
                }
            }
            Err(_) => EntryKind::Other,
        };
        let (size, modified, metadata_incomplete) = match metadata {
            Ok(metadata) => (
                (kind == EntryKind::File).then_some(metadata.len()),
                metadata.modified().ok(),
                file_type_missing,
            ),
            Err(_) => (None, None, true),
        };
        entries.push(FileEntry {
            path,
            display_name,
            kind,
            size,
            modified,
            metadata_incomplete,
        });
    }
    Ok(DirectoryListing { entries, truncated })
}

pub fn sanitize_display_name(name: &str) -> String {
    name.chars()
        .flat_map(|character| {
            if character.is_control() || is_bidirectional_control(character) {
                char::REPLACEMENT_CHARACTER
            } else {
                character
            }
            .to_string()
            .chars()
            .collect::<Vec<_>>()
        })
        .collect()
}

fn is_bidirectional_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scan_lists_files_and_directories_without_mutating_them() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("note.txt"), b"hello").unwrap();
        fs::create_dir(temp.path().join("folder")).unwrap();

        let listing = scan_directory(temp.path()).unwrap();
        let entries = listing.entries;

        assert_eq!(entries.len(), 2);
        assert!(!listing.truncated);
        assert!(entries.iter().any(|entry| {
            entry.display_name == "note.txt"
                && entry.kind == EntryKind::File
                && entry.size == Some(5)
        }));
        assert!(
            entries.iter().any(|entry| {
                entry.display_name == "folder" && entry.kind == EntryKind::Directory
            })
        );
    }

    #[test]
    fn missing_directory_has_a_stable_error_classification() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");

        let error = scan_directory(&missing).unwrap_err();

        assert_eq!(error.kind, ScanErrorKind::NotFound);
        assert_eq!(error.message, "Folder no longer exists");
    }

    #[test]
    fn display_names_cannot_inject_control_characters() {
        assert_eq!(
            sanitize_display_name("hello\nworld\u{1b}\u{202e}txt.exe"),
            "hello�world��txt.exe"
        );
    }
}
