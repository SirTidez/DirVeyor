use crate::OperationEvent;
use crate::planner::{
    FileTask, Fingerprint, OperationPlan, PlannedAction, TransferRoot, revalidate,
};
use crossbeam_channel::Sender;
use fileadmin_domain::{
    JobOutcome, JobPhase, OperationFailure, OperationProgress, OperationReport,
};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tempfile::NamedTempFile;

const COPY_BUFFER_BYTES: usize = 1024 * 1024;

pub(crate) fn execute(
    plan: OperationPlan,
    events: &Sender<OperationEvent>,
    cancel_requested: &Arc<AtomicBool>,
) -> OperationReport {
    let total_items = plan.summary.item_count;
    let completed_items = Arc::new(AtomicU64::new(0));
    let completed_bytes = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(Mutex::new(Vec::new()));
    send_progress(events, &plan, JobPhase::Running, 0, 0, None);

    match &plan.action {
        PlannedAction::Transfer {
            roots,
            remove_sources,
            worker_count,
        } => execute_transfer(
            &plan,
            roots,
            *remove_sources,
            *worker_count,
            events,
            cancel_requested,
            &completed_items,
            &completed_bytes,
            &failures,
        ),
        PlannedAction::AtomicMove { roots } => execute_atomic_moves(
            &plan,
            roots,
            events,
            cancel_requested,
            &completed_items,
            &failures,
        ),
        PlannedAction::Recycle { sources } => execute_recycle(
            &plan,
            sources,
            events,
            cancel_requested,
            &completed_items,
            &failures,
        ),
        PlannedAction::Rename {
            source,
            target,
            fingerprint,
        } => {
            if !cancel_requested.load(Ordering::Acquire) {
                let result = revalidate(source, fingerprint).and_then(|()| {
                    rename_no_replace(source, target).map_err(|error| error.to_string())
                });
                match result {
                    Ok(()) => {
                        completed_items.store(1, Ordering::Release);
                        send_progress(
                            events,
                            &plan,
                            JobPhase::Finalizing,
                            1,
                            0,
                            Some(target.clone()),
                        );
                    }
                    Err(message) => push_failure(&failures, Some(source.clone()), message),
                }
            }
        }
        PlannedAction::CreateDirectory { target } => {
            if !cancel_requested.load(Ordering::Acquire) {
                match fs::create_dir(target) {
                    Ok(()) => {
                        completed_items.store(1, Ordering::Release);
                        send_progress(
                            events,
                            &plan,
                            JobPhase::Finalizing,
                            1,
                            0,
                            Some(target.clone()),
                        );
                    }
                    Err(error) => {
                        push_failure(&failures, Some(target.clone()), error.to_string());
                    }
                }
            }
        }
    }

    let completed_items = completed_items.load(Ordering::Acquire);
    let completed_bytes = completed_bytes.load(Ordering::Acquire);
    let failures = Arc::try_unwrap(failures)
        .unwrap_or_else(|_| panic!("operation failure list still has owners"))
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cancelled = cancel_requested.load(Ordering::Acquire);
    let outcome = if failures.is_empty() && !cancelled {
        JobOutcome::Completed
    } else if completed_items > 0 {
        JobOutcome::Partial
    } else if cancelled {
        JobOutcome::Cancelled
    } else {
        JobOutcome::Failed
    };

    OperationReport {
        job: plan.summary.job,
        kind: plan.summary.kind,
        outcome,
        completed_items,
        total_items,
        completed_bytes,
        failures,
        affected_directories: plan.affected_directories,
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_transfer(
    plan: &OperationPlan,
    roots: &[TransferRoot],
    remove_sources: bool,
    worker_count: usize,
    events: &Sender<OperationEvent>,
    cancel_requested: &Arc<AtomicBool>,
    completed_items: &Arc<AtomicU64>,
    completed_bytes: &Arc<AtomicU64>,
    failures: &Arc<Mutex<Vec<OperationFailure>>>,
) {
    let mut created_directories = Vec::new();
    for directory in roots.iter().flat_map(|root| &root.directories) {
        if cancel_requested.load(Ordering::Acquire) {
            cleanup_empty_directories(&created_directories);
            return;
        }
        if let Err(message) = revalidate(&directory.source, &directory.fingerprint) {
            push_failure(failures, Some(directory.source.clone()), message);
            cleanup_empty_directories(&created_directories);
            return;
        }
        match fs::create_dir(&directory.target) {
            Ok(()) => {
                created_directories.push(directory.target.clone());
                let items = completed_items.fetch_add(1, Ordering::AcqRel) + 1;
                send_progress(
                    events,
                    plan,
                    JobPhase::Running,
                    items,
                    completed_bytes.load(Ordering::Acquire),
                    Some(directory.target.clone()),
                );
            }
            Err(error) => {
                push_failure(failures, Some(directory.target.clone()), error.to_string());
                cleanup_empty_directories(&created_directories);
                return;
            }
        }
    }

    let files: Arc<Vec<FileTask>> = Arc::new(
        roots
            .iter()
            .flat_map(|root| root.files.iter().cloned())
            .collect(),
    );
    let next = Arc::new(AtomicUsize::new(0));
    let stop_after_error = Arc::new(AtomicBool::new(false));
    let verify_hash = remove_sources;

    thread::scope(|scope| {
        for worker_number in 0..worker_count.max(1) {
            let files = Arc::clone(&files);
            let next = Arc::clone(&next);
            let stop_after_error = Arc::clone(&stop_after_error);
            let cancel_requested = Arc::clone(cancel_requested);
            let completed_items = Arc::clone(completed_items);
            let completed_bytes = Arc::clone(completed_bytes);
            let failures = Arc::clone(failures);
            let events = events.clone();
            scope.spawn(move || {
                let _worker_number = worker_number;
                loop {
                    if cancel_requested.load(Ordering::Acquire)
                        || stop_after_error.load(Ordering::Acquire)
                    {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::AcqRel);
                    let Some(task) = files.get(index) else {
                        break;
                    };
                    let result = copy_file(
                        task,
                        verify_hash,
                        &cancel_requested,
                        &completed_bytes,
                        &events,
                        plan,
                        &completed_items,
                    );
                    match result {
                        Ok(()) => {
                            let items = completed_items.fetch_add(1, Ordering::AcqRel) + 1;
                            send_progress(
                                &events,
                                plan,
                                JobPhase::Running,
                                items,
                                completed_bytes.load(Ordering::Acquire),
                                Some(task.target.clone()),
                            );
                        }
                        Err(CopyError::Cancelled) => break,
                        Err(CopyError::Failed(message)) => {
                            stop_after_error.store(true, Ordering::Release);
                            push_failure(&failures, Some(task.source.clone()), message);
                            break;
                        }
                    }
                }
            });
        }
    });

    if cancel_requested.load(Ordering::Acquire)
        || stop_after_error.load(Ordering::Acquire)
        || !failures
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty()
    {
        cleanup_empty_directories(&created_directories);
        return;
    }

    if remove_sources {
        send_progress(
            events,
            plan,
            JobPhase::Verifying,
            completed_items.load(Ordering::Acquire),
            completed_bytes.load(Ordering::Acquire),
            None,
        );
        if let Err((path, message)) = remove_frozen_sources(roots, cancel_requested) {
            push_failure(failures, Some(path), message);
        }
    }
}

fn copy_file(
    task: &FileTask,
    verify_hash: bool,
    cancel_requested: &AtomicBool,
    completed_bytes: &AtomicU64,
    events: &Sender<OperationEvent>,
    plan: &OperationPlan,
    completed_items: &AtomicU64,
) -> Result<(), CopyError> {
    let mut local_bytes = 0_u64;
    let result = copy_file_inner(
        task,
        verify_hash,
        cancel_requested,
        completed_bytes,
        events,
        plan,
        completed_items,
        &mut local_bytes,
    );
    if result.is_err() {
        completed_bytes.fetch_sub(local_bytes, Ordering::AcqRel);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn copy_file_inner(
    task: &FileTask,
    verify_hash: bool,
    cancel_requested: &AtomicBool,
    completed_bytes: &AtomicU64,
    events: &Sender<OperationEvent>,
    plan: &OperationPlan,
    completed_items: &AtomicU64,
    local_bytes: &mut u64,
) -> Result<(), CopyError> {
    revalidate(&task.source, &task.fingerprint).map_err(CopyError::Failed)?;
    let mut source = File::open(&task.source)
        .map_err(|error| CopyError::Failed(format!("Cannot open source: {error}")))?;
    let parent = task
        .target
        .parent()
        .ok_or_else(|| CopyError::Failed("Destination has no parent directory".into()))?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|error| {
        CopyError::Failed(format!("Cannot create temporary destination: {error}"))
    })?;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut source_hash = Sha256::new();
    loop {
        if cancel_requested.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let count = source
            .read(&mut buffer)
            .map_err(|error| CopyError::Failed(format!("Cannot read source: {error}")))?;
        if count == 0 {
            break;
        }
        temporary
            .write_all(&buffer[..count])
            .map_err(|error| CopyError::Failed(format!("Cannot write destination: {error}")))?;
        if verify_hash {
            source_hash.update(&buffer[..count]);
        }
        let count = count as u64;
        *local_bytes += count;
        let bytes = completed_bytes.fetch_add(count, Ordering::AcqRel) + count;
        send_progress(
            events,
            plan,
            JobPhase::Running,
            completed_items.load(Ordering::Acquire),
            bytes,
            Some(task.source.clone()),
        );
    }

    if *local_bytes != task.fingerprint.len {
        return Err(CopyError::Failed(
            "Source size changed while copying".into(),
        ));
    }
    temporary
        .flush()
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| CopyError::Failed(format!("Cannot flush destination: {error}")))?;
    revalidate(&task.source, &task.fingerprint).map_err(CopyError::Failed)?;

    if verify_hash {
        send_progress(
            events,
            plan,
            JobPhase::Verifying,
            completed_items.load(Ordering::Acquire),
            completed_bytes.load(Ordering::Acquire),
            Some(task.target.clone()),
        );
        temporary
            .as_file_mut()
            .seek(SeekFrom::Start(0))
            .map_err(|error| CopyError::Failed(format!("Cannot verify destination: {error}")))?;
        let destination_hash = hash_reader(temporary.as_file_mut(), cancel_requested)?;
        if source_hash.finalize().as_slice() != destination_hash.as_slice() {
            return Err(CopyError::Failed("SHA-256 verification failed".into()));
        }
    }

    let permissions = fs::symlink_metadata(&task.source)
        .map_err(|error| CopyError::Failed(format!("Cannot read source permissions: {error}")))?
        .permissions();
    temporary
        .as_file()
        .set_permissions(permissions)
        .map_err(|error| CopyError::Failed(format!("Cannot apply permissions: {error}")))?;
    temporary.persist_noclobber(&task.target).map_err(|error| {
        CopyError::Failed(format!(
            "Cannot publish destination without overwriting: {error}"
        ))
    })?;
    Ok(())
}

fn hash_reader(reader: &mut File, cancel_requested: &AtomicBool) -> Result<Vec<u8>, CopyError> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        if cancel_requested.load(Ordering::Acquire) {
            return Err(CopyError::Cancelled);
        }
        let count = reader
            .read(&mut buffer)
            .map_err(|error| CopyError::Failed(format!("Cannot verify destination: {error}")))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finalize().to_vec())
}

fn execute_atomic_moves(
    plan: &OperationPlan,
    roots: &[TransferRoot],
    events: &Sender<OperationEvent>,
    cancel_requested: &AtomicBool,
    completed_items: &AtomicU64,
    failures: &Mutex<Vec<OperationFailure>>,
) {
    for root in roots {
        if cancel_requested.load(Ordering::Acquire) {
            return;
        }
        if let Err(message) = revalidate(&root.source, &root.source_fingerprint) {
            push_failure(failures, Some(root.source.clone()), message);
            return;
        }
        if let Err(error) = rename_no_replace(&root.source, &root.target) {
            push_failure(failures, Some(root.source.clone()), error.to_string());
            return;
        }
        let root_items = (root.directories.len() + root.files.len()) as u64;
        let items = completed_items.fetch_add(root_items, Ordering::AcqRel) + root_items;
        send_progress(
            events,
            plan,
            JobPhase::Finalizing,
            items,
            plan.summary.total_bytes,
            Some(root.target.clone()),
        );
    }
}

fn execute_recycle(
    plan: &OperationPlan,
    sources: &[(PathBuf, Fingerprint, u64)],
    events: &Sender<OperationEvent>,
    cancel_requested: &AtomicBool,
    completed_items: &AtomicU64,
    failures: &Mutex<Vec<OperationFailure>>,
) {
    for (source, fingerprint, item_count) in sources {
        if cancel_requested.load(Ordering::Acquire) {
            return;
        }
        if let Err(message) = revalidate(source, fingerprint) {
            push_failure(failures, Some(source.clone()), message);
            return;
        }
        match trash::delete(source) {
            Ok(()) => {
                let items = completed_items.fetch_add(*item_count, Ordering::AcqRel) + *item_count;
                send_progress(
                    events,
                    plan,
                    JobPhase::Finalizing,
                    items,
                    0,
                    Some(source.clone()),
                );
            }
            Err(error) => {
                push_failure(
                    failures,
                    Some(source.clone()),
                    format!(
                        "Recycle Bin operation failed; nothing was permanently deleted: {error}"
                    ),
                );
                return;
            }
        }
    }
}

fn remove_frozen_sources(
    roots: &[TransferRoot],
    cancel_requested: &AtomicBool,
) -> Result<(), (PathBuf, String)> {
    for file in roots.iter().flat_map(|root| &root.files) {
        if cancel_requested.load(Ordering::Acquire) {
            return Err((
                file.source.clone(),
                "Cancellation requested; copied destination retained and source left in place"
                    .into(),
            ));
        }
        revalidate(&file.source, &file.fingerprint)
            .map_err(|message| (file.source.clone(), message))?;
        fs::remove_file(&file.source).map_err(|error| {
            (
                file.source.clone(),
                format!("Copied and verified, but source removal failed: {error}"),
            )
        })?;
    }

    let mut directories: Vec<_> = roots
        .iter()
        .flat_map(|root| root.directories.iter())
        .collect();
    directories.sort_by_key(|directory| std::cmp::Reverse(directory.source.components().count()));
    for directory in directories {
        if cancel_requested.load(Ordering::Acquire) {
            return Err((
                directory.source.clone(),
                "Cancellation requested during source cleanup; remaining sources were retained"
                    .into(),
            ));
        }
        let metadata = fs::symlink_metadata(&directory.source).map_err(|error| {
            (
                directory.source.clone(),
                format!("Cannot revalidate source directory before removal: {error}"),
            )
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err((
                directory.source.clone(),
                "Source directory identity changed before removal".into(),
            ));
        }
        fs::remove_dir(&directory.source).map_err(|error| {
            (
                directory.source.clone(),
                format!("Copied and verified, but source directory was retained: {error}"),
            )
        })?;
    }
    Ok(())
}

fn cleanup_empty_directories(directories: &[PathBuf]) {
    for directory in directories.iter().rev() {
        let _ = fs::remove_dir(directory);
    }
}

fn push_failure(failures: &Mutex<Vec<OperationFailure>>, path: Option<PathBuf>, message: String) {
    failures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(OperationFailure { path, message });
}

fn send_progress(
    events: &Sender<OperationEvent>,
    plan: &OperationPlan,
    phase: JobPhase,
    completed_items: u64,
    completed_bytes: u64,
    current_path: Option<PathBuf>,
) {
    let _ = events.try_send(OperationEvent::Progress(OperationProgress {
        job: plan.summary.job,
        kind: plan.summary.kind,
        phase,
        completed_items,
        total_items: plan.summary.item_count,
        completed_bytes,
        total_bytes: plan.summary.total_bytes,
        current_path,
    }));
}

enum CopyError {
    Cancelled,
    Failed(String),
}

#[cfg(windows)]
fn rename_no_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both paths are valid NUL-terminated UTF-16 buffers. Flags are zero,
    // so MoveFileExW refuses to replace an existing destination.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(windows))]
fn rename_no_replace(source: &Path, target: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_file() {
        fs::hard_link(source, target)?;
        fs::remove_file(source)
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "safe no-replace directory rename is not implemented on this platform",
        ))
    }
}
