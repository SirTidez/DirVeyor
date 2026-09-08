use dirveyor_domain::{FolderSizeProgress, FolderSizeSummary, PaneId};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const RESULT_CAPACITY: usize = 4;
const RESULT_CACHE_CAPACITY: usize = 16;
const FOCUS_SETTLE_TIME: Duration = Duration::from_millis(200);

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
    pub update: FolderSizeUpdate,
}

#[derive(Debug)]
pub enum FolderSizeUpdate {
    Progress(FolderSizeProgress),
    Finished(Result<FolderSizeSummary, String>),
}

pub struct FolderSizeScanner {
    pending: Arc<(Mutex<Option<FolderSizeRequest>>, Condvar)>,
    results: Receiver<FolderSizeEvent>,
    current_request: Arc<AtomicU64>,
    next_request: AtomicU64,
    shutdown: Arc<AtomicBool>,
}

impl FolderSizeScanner {
    pub fn new() -> Self {
        Self::with_worker_count(4)
    }

    /// Limit parallel directory reads to 1..=8 workers. Default: four.
    pub fn with_worker_count(worker_count: usize) -> Self {
        let worker_count = worker_count.clamp(1, 8);
        let pending = Arc::new((Mutex::new(None), Condvar::new()));
        let current_request = Arc::new(AtomicU64::new(0));
        let (result_tx, result_rx) = mpsc::sync_channel(RESULT_CAPACITY);
        let worker_pending = Arc::clone(&pending);
        let worker_current = Arc::clone(&current_request);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        thread::Builder::new()
            .name("dirveyor-folder-size".into())
            .spawn(move || {
                folder_size_worker(
                    worker_pending,
                    result_tx,
                    worker_current,
                    worker_count,
                    worker_shutdown,
                )
            })
            .expect("failed to start folder size scanner");

        Self {
            pending,
            results: result_rx,
            current_request,
            next_request: AtomicU64::new(1),
            shutdown,
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
        let (pending, ready) = &*self.pending;
        *pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        ready.notify_one();
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

impl Drop for FolderSizeScanner {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.cancel();
    }
}

fn folder_size_worker(
    pending: Arc<(Mutex<Option<FolderSizeRequest>>, Condvar)>,
    results: SyncSender<FolderSizeEvent>,
    current_request: Arc<AtomicU64>,
    worker_count: usize,
    shutdown: Arc<AtomicBool>,
) {
    let mut cache: VecDeque<FolderSizeSummary> = VecDeque::new();
    loop {
        let request = {
            let (pending, ready) = &*pending;
            let mut slot = pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            while slot.is_none() && !shutdown.load(Ordering::Acquire) {
                slot = ready
                    .wait(slot)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            slot.take().expect("folder size request disappeared")
        };

        if let Some(mut summary) = cache
            .iter()
            .find(|summary| {
                summary.pane == request.pane
                    && summary.generation == request.generation
                    && summary.path == request.path
            })
            .cloned()
        {
            summary.request_id = request.request_id;
            let event = FolderSizeEvent {
                request_id: request.request_id,
                pane: request.pane,
                generation: request.generation,
                path: request.path,
                update: FolderSizeUpdate::Finished(Ok(summary)),
            };
            if results.send(event).is_err() {
                break;
            }
            continue;
        }

        // Cached sizes remain immediate. Before doing new disk work, allow
        // focus to settle; a replacement request or cancellation wakes us early.
        {
            let (pending, ready) = &*pending;
            let slot = pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _settled = ready
                .wait_timeout_while(slot, FOCUS_SETTLE_TIME, |slot| {
                    slot.is_none() && current_request.load(Ordering::Acquire) == request.request_id
                })
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        let Some(result) =
            measure_folder_with_workers(&request, &current_request, Some(&results), worker_count)
        else {
            continue;
        };
        if current_request.load(Ordering::Acquire) != request.request_id {
            continue;
        }
        if let Ok(summary) = &result {
            cache.retain(|cached| {
                cached.pane != summary.pane
                    || cached.generation != summary.generation
                    || cached.path != summary.path
            });
            cache.push_front(summary.clone());
            cache.truncate(RESULT_CACHE_CAPACITY);
        }
        let event = FolderSizeEvent {
            request_id: request.request_id,
            pane: request.pane,
            generation: request.generation,
            path: request.path,
            update: FolderSizeUpdate::Finished(result),
        };
        if results.send(event).is_err() {
            break;
        }
    }
}

#[cfg(test)]
fn measure_folder(
    request: &FolderSizeRequest,
    current_request: &AtomicU64,
    progress_events: Option<&SyncSender<FolderSizeEvent>>,
) -> Option<Result<FolderSizeSummary, String>> {
    measure_folder_with_workers(request, current_request, progress_events, 2)
}

fn measure_folder_with_workers(
    request: &FolderSizeRequest,
    current_request: &AtomicU64,
    progress_events: Option<&SyncSender<FolderSizeEvent>>,
    worker_count: usize,
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

    // Resolve only the root; native enumeration preserves this verbatim prefix
    // for long Windows paths. Root links were rejected before resolution.
    let root = match fs::canonicalize(&request.path) {
        Ok(path) => path,
        Err(error) => return Some(Err(format!("Could not resolve folder — {error}"))),
    };
    let drive_total_bytes = drive_total_bytes(&root);
    let shared = WalkShared {
        state: Mutex::new(WalkState {
            pending: vec![root.clone()],
            active: 0,
            counts: Counts::default(),
            error: None,
        }),
        ready: Condvar::new(),
        failed: std::sync::atomic::AtomicBool::new(false),
    };
    thread::scope(|scope| {
        for _ in 0..worker_count {
            let shared = &shared;
            let root = &root;
            scope.spawn(move || walk_worker(shared, root, request, current_request));
        }
        let mut last_progress = Instant::now();
        loop {
            let state = shared.state.lock().unwrap();
            if (state.pending.is_empty() && state.active == 0)
                || current_request.load(Ordering::Acquire) != request.request_id
            {
                shared.ready.notify_all();
                break;
            }
            let (state, _) = shared
                .ready
                .wait_timeout(state, Duration::from_millis(100))
                .unwrap();
            if last_progress.elapsed() >= Duration::from_millis(100) {
                let counts = state.counts;
                drop(state);
                send_progress(
                    progress_events,
                    request,
                    counts.bytes,
                    counts.files,
                    counts.directories,
                    counts.skipped,
                    drive_total_bytes,
                );
                last_progress = Instant::now();
            }
        }
    });
    if current_request.load(Ordering::Acquire) != request.request_id {
        return None;
    }
    let state = shared.state.into_inner().unwrap();
    if let Some(error) = state.error {
        return Some(Err(error));
    }
    Some(Ok(FolderSizeSummary {
        request_id: request.request_id,
        pane: request.pane,
        generation: request.generation,
        path: request.path.clone(),
        total_bytes: state.counts.bytes,
        file_count: state.counts.files,
        directory_count: state.counts.directories,
        skipped_items: state.counts.skipped,
        drive_total_bytes,
    }))
}

const DIRECTORY_BACKLOG: usize = 1024;
const COUNT_BATCH: usize = 256;

#[derive(Clone, Copy, Default)]
struct Counts {
    bytes: u64,
    files: u64,
    directories: u64,
    skipped: u64,
}

struct WalkState {
    pending: Vec<PathBuf>,
    active: usize,
    counts: Counts,
    error: Option<String>,
}

struct WalkShared {
    state: Mutex<WalkState>,
    ready: Condvar,
    failed: std::sync::atomic::AtomicBool,
}

fn flush_counts(shared: &WalkShared, counts: &mut Counts) -> Result<(), String> {
    let mut state = shared.state.lock().unwrap();
    state.counts.bytes = state
        .counts
        .bytes
        .checked_add(counts.bytes)
        .ok_or_else(|| "Contained size exceeds the supported range".to_owned())?;
    state.counts.files = state.counts.files.saturating_add(counts.files);
    state.counts.directories = state.counts.directories.saturating_add(counts.directories);
    state.counts.skipped = state.counts.skipped.saturating_add(counts.skipped);
    *counts = Counts::default();
    Ok(())
}

fn walk_worker(shared: &WalkShared, root: &Path, request: &FolderSizeRequest, current: &AtomicU64) {
    let cancelled = || {
        current.load(Ordering::Acquire) != request.request_id
            || shared.failed.load(Ordering::Relaxed)
    };
    loop {
        let path = {
            let mut state = shared.state.lock().unwrap();
            loop {
                if cancelled() {
                    return;
                }
                if let Some(path) = state.pending.pop() {
                    state.active += 1;
                    break path;
                }
                if state.active == 0 {
                    return;
                }
                state = shared
                    .ready
                    .wait_timeout(state, Duration::from_millis(50))
                    .unwrap()
                    .0;
            }
        };
        let result = scan_subtree(path, root, shared, &cancelled);
        let mut state = shared.state.lock().unwrap();
        state.active -= 1;
        if let Err(error) = result {
            if state.error.is_none() {
                state.error = Some(error);
            }
            state.pending.clear();
            shared.failed.store(true, Ordering::Relaxed);
        }
        if (state.active == 0 && state.pending.is_empty()) || state.error.is_some() {
            shared.ready.notify_all();
        }
    }
}

fn scan_subtree(
    path: PathBuf,
    root: &Path,
    shared: &WalkShared,
    cancelled: &impl Fn() -> bool,
) -> Result<(), String> {
    use crate::size_entries::{SizeEntry, read_dir};
    let mut counts = Counts {
        directories: 1,
        ..Counts::default()
    };
    let first = match read_dir(&path) {
        Ok(reader) => reader,
        Err(error) if path == root => return Err(format!("Could not read folder — {error}")),
        Err(_) => {
            counts.skipped += 1;
            return flush_counts(shared, &mut counts);
        }
    };
    // When the bounded shared backlog fills, descend locally. Keeping iterators
    // instead of collecting children bounds overflow memory by tree depth.
    let mut readers = vec![first];
    let mut batch = 0;
    while !readers.is_empty() && !cancelled() {
        let Some(entry) = readers.last_mut().unwrap().next() else {
            readers.pop();
            continue;
        };
        match entry {
            Ok(SizeEntry::Directory(path)) => {
                let mut state = shared.state.lock().unwrap();
                if state.pending.len() < DIRECTORY_BACKLOG {
                    state.pending.push(path);
                    if state.pending.len() == 1 {
                        // Wake both idle workers and the progress coordinator;
                        // notify_one could repeatedly wake only the coordinator.
                        shared.ready.notify_all();
                    }
                } else {
                    drop(state);
                    counts.directories += 1;
                    match read_dir(&path) {
                        Ok(reader) => readers.push(reader),
                        Err(_) => counts.skipped += 1,
                    }
                }
            }
            Ok(SizeEntry::File(bytes)) => {
                counts.files += 1;
                counts.bytes = counts
                    .bytes
                    .checked_add(bytes)
                    .ok_or_else(|| "Contained size exceeds the supported range".to_owned())?;
            }
            Ok(SizeEntry::Skipped) | Err(_) => counts.skipped += 1,
        }
        batch += 1;
        if batch == COUNT_BATCH {
            flush_counts(shared, &mut counts)?;
            batch = 0;
        }
    }
    flush_counts(shared, &mut counts)
}

fn send_progress(
    events: Option<&SyncSender<FolderSizeEvent>>,
    request: &FolderSizeRequest,
    discovered_bytes: u64,
    file_count: u64,
    directory_count: u64,
    skipped_items: u64,
    drive_total_bytes: Option<u64>,
) {
    let Some(events) = events else {
        return;
    };
    let progress = FolderSizeProgress {
        request_id: request.request_id,
        pane: request.pane,
        generation: request.generation,
        path: request.path.clone(),
        discovered_bytes,
        file_count,
        directory_count,
        skipped_items,
        drive_total_bytes,
    };
    match events.try_send(FolderSizeEvent {
        request_id: request.request_id,
        pane: request.pane,
        generation: request.generation,
        path: request.path.clone(),
        update: FolderSizeUpdate::Progress(progress),
    }) {
        Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
    }
}

#[cfg(windows)]
fn drive_total_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    // The inspector only needs capacity, not the volume label or filesystem.
    let mut path: Vec<u16> = path.as_os_str().encode_wide().collect();
    if path.last() != Some(&(b'\\' as u16)) {
        path.push(b'\\' as u16);
    }
    path.push(0);
    let mut total = 0;
    // SAFETY: path is terminated and total is writable for the duration of the call.
    let success = unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            std::ptr::null_mut(),
            &mut total,
            std::ptr::null_mut(),
        )
    };
    (success != 0).then_some(total)
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

    fn test_request(path: PathBuf) -> FolderSizeRequest {
        FolderSizeRequest {
            request_id: 1,
            pane: PaneId::Left,
            generation: 0,
            path,
        }
    }

    #[test]
    fn worker_counts_agree_on_backlog_overflow_and_deep_unicode_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let width = DIRECTORY_BACKLOG + 17;
        for index in 0..width {
            let directory = root.join(format!("child_{index}"));
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("file"), b"abc").unwrap();
        }
        let mut deep = root.clone();
        for _ in 0..32 {
            deep = deep.join("nested_ä_folder");
            fs::create_dir(&deep).unwrap();
        }
        fs::write(deep.join("你好.txt"), b"seven77").unwrap();
        for workers in [1, 2, 4, 8] {
            let summary = measure_folder_with_workers(
                &test_request(root.clone()),
                &AtomicU64::new(1),
                None,
                workers,
            )
            .unwrap()
            .unwrap();
            assert_eq!(summary.file_count, width as u64 + 1);
            assert_eq!(summary.directory_count, width as u64 + 33);
            assert_eq!(summary.total_bytes, width as u64 * 3 + 7);
            assert_eq!(summary.skipped_items, 0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_walk_skips_junction_cycles_and_rejects_a_junction_root() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file"), b"keep").unwrap();
        let junction = temp.path().join("cycle");
        let output = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let summary = measure_folder(
            &test_request(temp.path().to_path_buf()),
            &AtomicU64::new(1),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (
                summary.file_count,
                summary.directory_count,
                summary.total_bytes,
                summary.skipped_items
            ),
            (1, 1, 4, 1)
        );
        assert!(
            measure_folder(&test_request(junction), &AtomicU64::new(1), None)
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn worker_waiters_exit_on_cancellation() {
        let shared = WalkShared {
            state: Mutex::new(WalkState {
                pending: vec![],
                active: 1,
                counts: Counts::default(),
                error: None,
            }),
            ready: Condvar::new(),
            failed: AtomicBool::new(false),
        };
        let current = AtomicU64::new(1);
        let request = test_request(PathBuf::from("unused"));
        thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| walk_worker(&shared, Path::new("unused"), &request, &current));
            }
            current.store(2, Ordering::Release);
            shared.ready.notify_all();
        });
        assert_eq!(shared.state.lock().unwrap().active, 1);
    }

    #[test]
    fn aggregate_overflow_is_reported() {
        let shared = WalkShared {
            state: Mutex::new(WalkState {
                pending: vec![],
                active: 0,
                counts: Counts {
                    bytes: u64::MAX,
                    ..Counts::default()
                },
                error: None,
            }),
            ready: Condvar::new(),
            failed: AtomicBool::new(false),
        };
        assert!(
            flush_counts(
                &shared,
                &mut Counts {
                    bytes: 1,
                    ..Counts::default()
                }
            )
            .is_err()
        );
    }

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

        let summary = measure_folder(&request, &current, None).unwrap().unwrap();

        assert_eq!(summary.total_bytes, 12);
        assert_eq!(summary.file_count, 2);
        assert_eq!(summary.directory_count, 2);
        assert_eq!(summary.skipped_items, 0);
        #[cfg(windows)]
        assert!(summary.drive_total_bytes.is_some_and(|total| total > 0));
    }

    #[test]
    fn uncached_measurement_waits_for_focus_and_only_finishes_latest_request() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file"), b"contents").unwrap();
        let scanner = FolderSizeScanner::new();
        scanner.request(PaneId::Left, 0, temp.path().join("obsolete"));
        let start = Instant::now();
        let latest = scanner.request(PaneId::Left, 1, temp.path().to_path_buf());
        let event = scanner
            .results
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(event.request_id, latest);
        assert!(start.elapsed() >= FOCUS_SETTLE_TIME);
        let FolderSizeUpdate::Finished(Ok(summary)) = event.update else {
            panic!("expected a finished measurement");
        };
        assert_eq!(summary.total_bytes, 8);
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

        assert!(measure_folder(&request, &current, None).is_none());
    }
}
