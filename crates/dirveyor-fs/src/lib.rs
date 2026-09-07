//! Read-only filesystem scanning on bounded background workers.

mod favorites;
mod folder_size;
mod preview;

pub use favorites::{FavoritesStore, MAX_FAVORITES, user_home_directory};
pub use folder_size::{FolderSizeEvent, FolderSizeRequest, FolderSizeScanner, FolderSizeUpdate};
pub use preview::{PreviewChangeEvent, PreviewEvent, PreviewLoader, PreviewWindowTarget};

use dirveyor_domain::{DriveInfo, DriveKind, EntryKind, FileEntry, PaneId};
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
    pub location: ScanLocation,
}

#[derive(Clone, Debug)]
pub enum ScanLocation {
    Directory(PathBuf),
    Drives,
}

#[derive(Debug)]
pub struct ScanEvent {
    pub pane: PaneId,
    pub generation: u64,
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
                .name(format!("dirveyor-scan-{worker_number}"))
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
            result: match request.location {
                ScanLocation::Directory(path) => scan_directory(&path),
                ScanLocation::Drives => scan_drives(),
            },
        };
        if results.send(event).is_err() {
            break;
        }
    }
}

pub fn scan_directory(path: &Path) -> Result<DirectoryListing, ScanError> {
    let reader = fs::read_dir(path).map_err(ScanError::from_io)?;
    let mut entries = Vec::new();
    if let Some(parent) = parent_entry(path) {
        entries.push(parent);
    }
    let mut truncated = false;
    let mut discovered = 0;
    for item in reader {
        if discovered == MAX_DIRECTORY_ENTRIES {
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
            drive_info: None,
        });
        discovered += 1;
    }
    Ok(DirectoryListing { entries, truncated })
}

fn parent_entry(path: &Path) -> Option<FileEntry> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        return Some(FileEntry {
            path: parent.to_path_buf(),
            display_name: "..".into(),
            kind: EntryKind::Parent,
            size: None,
            modified: None,
            metadata_incomplete: false,
            drive_info: None,
        });
    }

    drive_root_parent_entry()
}

#[cfg(windows)]
fn drive_root_parent_entry() -> Option<FileEntry> {
    Some(FileEntry {
        path: PathBuf::new(),
        display_name: "..  All drives".into(),
        kind: EntryKind::Parent,
        size: None,
        modified: None,
        metadata_incomplete: false,
        drive_info: None,
    })
}

#[cfg(not(windows))]
fn drive_root_parent_entry() -> Option<FileEntry> {
    None
}

#[cfg(windows)]
fn scan_drives() -> Result<DirectoryListing, ScanError> {
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

    // SAFETY: GetLogicalDrives takes no pointers and has no preconditions.
    let drive_mask = unsafe { GetLogicalDrives() };
    let entries = (b'A'..=b'Z')
        .filter(|letter| drive_mask & (1 << (letter - b'A')) != 0)
        .map(|letter| {
            let path = PathBuf::from(format!("{}:\\", letter as char));
            let drive_info = windows_drive_info(&path);
            let display_name = match drive_info.label.as_deref() {
                Some(label) if !label.is_empty() => format!("{}  {label}", path.display()),
                _ => path.to_string_lossy().into_owned(),
            };
            FileEntry {
                display_name,
                path,
                kind: EntryKind::Drive,
                size: None,
                modified: None,
                metadata_incomplete: drive_info.total_bytes.is_none(),
                drive_info: Some(drive_info),
            }
        })
        .collect();
    Ok(DirectoryListing {
        entries,
        truncated: false,
    })
}

#[cfg(windows)]
fn windows_drive_info(path: &Path) -> DriveInfo {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeInformationW,
    };
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
    };

    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: wide_path is NUL-terminated and remains alive for every API call below.
    let raw_kind = unsafe { GetDriveTypeW(wide_path.as_ptr()) };
    let kind = match raw_kind {
        DRIVE_FIXED => DriveKind::Fixed,
        DRIVE_REMOVABLE => DriveKind::Removable,
        DRIVE_REMOTE => DriveKind::Network,
        DRIVE_CDROM => DriveKind::Optical,
        DRIVE_RAMDISK => DriveKind::RamDisk,
        _ => DriveKind::Unknown,
    };

    let mut available = 0_u64;
    let mut total = 0_u64;
    let mut total_free = 0_u64;
    // SAFETY: each output pointer references a valid u64 for the duration of the call.
    let space_available = unsafe {
        GetDiskFreeSpaceExW(
            wide_path.as_ptr(),
            &mut available,
            &mut total,
            &mut total_free,
        ) != 0
    };

    let mut label_buffer = [0_u16; 261];
    let mut filesystem_buffer = [0_u16; 261];
    let mut serial = 0_u32;
    let mut max_component = 0_u32;
    let mut flags = 0_u32;
    // SAFETY: both buffers are writable and their exact lengths are supplied.
    let volume_available = unsafe {
        GetVolumeInformationW(
            wide_path.as_ptr(),
            label_buffer.as_mut_ptr(),
            label_buffer.len() as u32,
            &mut serial,
            &mut max_component,
            &mut flags,
            filesystem_buffer.as_mut_ptr(),
            filesystem_buffer.len() as u32,
        ) != 0
    };

    let (label, filesystem) = if volume_available {
        (
            wide_buffer_to_string(&label_buffer),
            wide_buffer_to_string(&filesystem_buffer),
        )
    } else {
        (None, None)
    };

    DriveInfo {
        kind,
        label,
        filesystem,
        total_bytes: space_available.then_some(total),
        available_bytes: space_available.then_some(available),
    }
}

#[cfg(windows)]
fn wide_buffer_to_string(buffer: &[u16]) -> Option<String> {
    let length = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    let value = String::from_utf16_lossy(&buffer[..length]);
    (!value.is_empty()).then_some(value)
}

#[cfg(not(windows))]
fn scan_drives() -> Result<DirectoryListing, ScanError> {
    Err(ScanError {
        kind: ScanErrorKind::Other,
        message: "Drive selection is only available on Windows".into(),
    })
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

        assert_eq!(entries.len(), 3);
        assert!(!listing.truncated);
        assert!(
            entries
                .iter()
                .any(|entry| { entry.display_name == ".." && entry.kind == EntryKind::Parent })
        );
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

    #[test]
    fn filesystem_root_has_no_parent_entry() {
        let root = parent_entry(Path::new(std::path::MAIN_SEPARATOR_STR));
        if cfg!(windows) {
            assert!(root.is_some_and(|entry| entry.path.as_os_str().is_empty()));
        } else {
            assert!(root.is_none());
        }
    }

    #[cfg(windows)]
    #[test]
    fn drive_listing_reports_capacity_for_an_accessible_volume() {
        let listing = scan_drives().unwrap();

        assert!(listing.entries.iter().any(|entry| {
            entry
                .drive_info
                .as_ref()
                .and_then(|drive| drive.total_bytes)
                .is_some_and(|total| total > 0)
        }));
    }
}
