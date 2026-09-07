use fileadmin_domain::{FolderSizeSummary, PaneId};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

const RESULT_CAPACITY: usize = 4;

#[derive(Clone, Debug)]
pub struct FolderSizeRequest {
    pub request_id: u64,
    pub pane: PaneId,
    pub generation: u64,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct FolderSizeEvent {
    pub request_id: u64,
    pub pane: PaneId,
    pub generation: u64,
    pub path: PathBuf,
    pub result: Result<FolderSizeSummary, String>,
}

pub struct FolderSizeScanner {
    pending: Arc<(Mutex<Option<FolderSizeRequest>>, Condvar)>,
    results: Receiver<FolderSizeEvent>,
    current_request: Arc<AtomicU64>,
    next_request: AtomicU64,
}

impl FolderSizeScanner {
    pub fn new() -> Self {
        let pending = Arc::new((Mutex::new(None), Condvar::new()));
        let current_request = Arc::new(AtomicU64::new(0));
        let (result_tx, result_rx) = mpsc::sync_channel(RESULT_CAPACITY);
        let worker_pending = Arc::clone(&pending);
        let worker_current = Arc::clone(&current_request);
        thread::Builder::new()
            .name("fileadmin-folder-size".into())
            .spawn(move || folder_size_worker(worker_pending, result_tx, worker_current))
            .expect("failed to start folder size scanner");

        Self {
            pending,
            results: result_rx,
            current_request,
            next_request: AtomicU64::new(1),
        }
    }

    pub fn request(&self, pane: PaneId, generation: u64, path: PathBuf) -> u64 {
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        self.current_request.store(request_id, Ordering::Release);
        let request = FolderSizeRequest {
            request_id,
            pane,
            generation,
            path,
        };
        let (pending, ready) = &*self.pending;
        *pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(request);
        ready.notify_one();
        request_id
    }

    pub fn cancel(&self) {
        let cancellation_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        self.current_request
            .store(cancellation_id, Ordering::Release);
        let (pending, _) = &*self.pending;
        *pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    pub fn try_recv(&self) -> Result<FolderSizeEvent, TryRecvError> {
        self.results.try_recv()
    }
}

impl Default for FolderSizeScanner {
    fn default() -> Self {
        Self::new()
    }
}

fn folder_size_worker(
    pending: Arc<(Mutex<Option<FolderSizeRequest>>, Condvar)>,
    results: SyncSender<FolderSizeEvent>,
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
            slot.take().expect("folder size request disappeared")
        };

        let Some(result) = measure_folder(&request, &current_request) else {
            continue;
        };
        if current_request.load(Ordering::Acquire) != request.request_id {
            continue;
        }
        let event = FolderSizeEvent {
            request_id: request.request_id,
            pane: request.pane,
            generation: request.generation,
            path: request.path,
            result,
        };
        if results.send(event).is_err() {
            break;
        }
    }
}

fn measure_folder(
    request: &FolderSizeRequest,
    current_request: &AtomicU64,
) -> Option<Result<FolderSizeSummary, String>> {
    if current_request.load(Ordering::Acquire) != request.request_id {
        return None;
    }
    let root_metadata = match fs::symlink_metadata(&request.path) {
        Ok(metadata) => metadata,
        Err(error) => return Some(Err(format!("Could not inspect folder — {error}"))),
    };
    if !root_metadata.is_dir()
        || root_metadata.file_type().is_symlink()
        || is_windows_reparse_point(&root_metadata)
    {
        return Some(Err("Focused item is not a scannable directory".into()));
    }

    let mut pending = vec![request.path.clone()];
    let mut total_bytes = 0_u64;
    let mut file_count = 0_u64;
    let mut directory_count = 0_u64;
    let mut skipped_items = 0_u64;

    while let Some(directory) = pending.pop() {
        if current_request.load(Ordering::Acquire) != request.request_id {
            return None;
        }
        directory_count = directory_count.saturating_add(1);
        let reader = match fs::read_dir(&directory) {
            Ok(reader) => reader,
            Err(error) if directory == request.path => {
                return Some(Err(format!("Could not read folder — {error}")));
            }
            Err(_) => {
                skipped_items = skipped_items.saturating_add(1);
                continue;
            }
        };

        for item in reader {
            if current_request.load(Ordering::Acquire) != request.request_id {
                return None;
            }
            let item = match item {
                Ok(item) => item,
                Err(_) => {
                    skipped_items = skipped_items.saturating_add(1);
                    continue;
                }
            };
            let metadata = match fs::symlink_metadata(item.path()) {
                Ok(metadata) => metadata,
                Err(_) => {
                    skipped_items = skipped_items.saturating_add(1);
                    continue;
                }
            };
            if metadata.file_type().is_symlink() || is_windows_reparse_point(&metadata) {
                skipped_items = skipped_items.saturating_add(1);
            } else if metadata.is_dir() {
                pending.push(item.path());
            } else if metadata.is_file() {
                file_count = file_count.saturating_add(1);
                total_bytes = match total_bytes.checked_add(metadata.len()) {
                    Some(total) => total,
                    None => return Some(Err("Contained size exceeds the supported range".into())),
                };
            } else {
                skipped_items = skipped_items.saturating_add(1);
            }
        }
    }

    Some(Ok(FolderSizeSummary {
        request_id: request.request_id,
        pane: request.pane,
        generation: request.generation,
        path: request.path.clone(),
        total_bytes,
        file_count,
        directory_count,
        skipped_items,
        drive_total_bytes: drive_total_bytes(&request.path),
    }))
}

#[cfg(windows)]
fn drive_total_bytes(path: &Path) -> Option<u64> {
    crate::windows_drive_info(path).total_bytes
}

#[cfg(not(windows))]
fn drive_total_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(windows)]
fn is_windows_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_windows_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn measures_nested_file_contents_without_following_links() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::File::create(temp.path().join("one.bin"))
            .unwrap()
            .write_all(&[1; 5])
            .unwrap();
        fs::File::create(nested.join("two.bin"))
            .unwrap()
            .write_all(&[2; 7])
            .unwrap();
        let request = FolderSizeRequest {
            request_id: 4,
            pane: PaneId::Left,
            generation: 2,
            path: temp.path().to_path_buf(),
        };
        let current = AtomicU64::new(4);

        let summary = measure_folder(&request, &current).unwrap().unwrap();

        assert_eq!(summary.total_bytes, 12);
        assert_eq!(summary.file_count, 2);
        assert_eq!(summary.directory_count, 2);
        assert_eq!(summary.skipped_items, 0);
    }

    #[test]
    fn stale_request_cancels_before_scanning() {
        let request = FolderSizeRequest {
            request_id: 1,
            pane: PaneId::Right,
            generation: 1,
            path: PathBuf::from("missing"),
        };
        let current = AtomicU64::new(2);

        assert!(measure_folder(&request, &current).is_none());
    }
}
