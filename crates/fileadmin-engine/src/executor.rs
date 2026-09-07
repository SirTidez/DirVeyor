use crate::planner::{
    FileTask, Fingerprint, ObjectKind, OperationPlan, PlannedAction, TransferRoot,
    keep_both_target, read_delete_manifest_entry, revalidate, revalidate_directory_kind,
};
use crate::{ConflictResolution, OperationEvent};
use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender};
use fileadmin_domain::{
    ConflictAction, ConflictKind, JobOutcome, JobPhase, OperationFailure, OperationProgress,
    OperationReport, TransferConflict, VerificationMode, VersionRelation,
};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::NamedTempFile;

const COPY_BUFFER_BYTES: usize = 1024 * 1024;

pub(crate) fn execute(
    plan: OperationPlan,
    events: &Sender<OperationEvent>,
    conflict_resolutions: &Receiver<ConflictResolution>,
    cancel_requested: &Arc<AtomicBool>,
) -> OperationReport {
    let total_items = plan.summary.item_count;
    let completed_items = Arc::new(AtomicU64::new(0));
    let completed_bytes = Arc::new(AtomicU64::new(0));
    let completed_files = Arc::new(AtomicU64::new(0));
    let completed_directories = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(Mutex::new(Vec::new()));
    if !matches!(&plan.action, PlannedAction::Transfer { .. }) {
        send_progress(events, &plan, JobPhase::Running, 0, 0, None);
    }

    match &plan.action {
        PlannedAction::Transfer {
            roots,
            remove_sources,
            worker_count,
            verification,
        } => execute_transfer(
            &plan,
            roots,
            *remove_sources,
            *worker_count,
            *verification,
            events,
            conflict_resolutions,
            cancel_requested,
            &completed_items,
            &completed_bytes,
            &completed_files,
            &completed_directories,
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
        PlannedAction::PermanentDelete { manifest } => execute_permanent_delete(
            &plan,
            manifest,
            events,
            cancel_requested,
            &completed_items,
            &completed_bytes,
            &completed_files,
            &completed_directories,
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
    let (completed_files, completed_directories) = if matches!(
        &plan.action,
        PlannedAction::PermanentDelete { .. } | PlannedAction::Transfer { .. }
    ) {
        (
            completed_files.load(Ordering::Acquire),
            completed_directories.load(Ordering::Acquire),
        )
    } else {
        (
            completed_items.min(plan.summary.file_count),
            completed_items.saturating_sub(plan.summary.file_count),
        )
    };
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
        total_items: if plan.summary.recursive_scope_known {
            total_items
        } else {
            completed_items
        },
        completed_files,
        total_files: if plan.summary.recursive_scope_known {
            plan.summary.file_count
        } else {
            completed_files
        },
        completed_directories,
        total_directories: if plan.summary.recursive_scope_known {
            plan.summary.directory_count
        } else {
            completed_directories
        },
        completed_bytes,
        total_bytes: if plan.summary.recursive_scope_known {
            plan.summary.total_bytes
        } else {
            completed_bytes
        },
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
    verification: fileadmin_domain::VerificationMode,
    events: &Sender<OperationEvent>,
    conflict_resolutions: &Receiver<ConflictResolution>,
    cancel_requested: &Arc<AtomicBool>,
    completed_items: &Arc<AtomicU64>,
    completed_bytes: &Arc<AtomicU64>,
    completed_files: &Arc<AtomicU64>,
    completed_directories: &Arc<AtomicU64>,
    failures: &Arc<Mutex<Vec<OperationFailure>>>,
) {
    let worker_count = worker_count.max(1);
    let queue_capacity = worker_count.saturating_mul(8).max(8);
    let (work_sender, work_receiver) = crossbeam_channel::bounded(queue_capacity);
    let stopped = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(StreamingProgress::from_plan(plan));
    let journal = if remove_sources {
        match tempfile::tempfile() {
            Ok(file) => Some(Arc::new(Mutex::new(file))),
            Err(error) => {
                push_failure(
                    failures,
                    None,
                    format!("Cannot create the temporary move journal: {error}"),
                );
                return;
            }
        }
    } else {
        None
    };

    std::thread::scope(|scope| {
        let mut workers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let receiver = work_receiver.clone();
            let stopped = Arc::clone(&stopped);
            let progress = Arc::clone(&progress);
            let journal = journal.clone();
            workers.push(scope.spawn(move || {
                transfer_worker(
                    receiver,
                    verification,
                    events,
                    plan,
                    cancel_requested,
                    completed_items,
                    completed_bytes,
                    completed_files,
                    completed_directories,
                    failures,
                    &stopped,
                    &progress,
                    journal.as_deref(),
                );
            }));
        }
        drop(work_receiver);

        let mut state = StreamingTransfer::new(
            plan,
            events,
            conflict_resolutions,
            cancel_requested,
            completed_items,
            completed_bytes,
            completed_files,
            completed_directories,
            work_sender,
            &stopped,
            &progress,
            journal.as_deref(),
        );
        for root in roots {
            if let Err((path, message)) = state.visit(&root.source, &root.target) {
                if !cancel_requested.load(Ordering::Acquire) && !stopped.load(Ordering::Acquire) {
                    push_failure(failures, Some(path), message);
                    stopped.store(true, Ordering::Release);
                }
                break;
            }
        }
        progress.scope_complete.store(true, Ordering::Release);
        state.publish(None);
        state.close_queue();

        for worker in workers {
            if worker.join().is_err() {
                push_failure(
                    failures,
                    None,
                    "A transfer worker stopped unexpectedly".into(),
                );
                stopped.store(true, Ordering::Release);
            }
        }

        if !stopped.load(Ordering::Acquire) && !cancel_requested.load(Ordering::Acquire) {
            let items = progress.total_items.load(Ordering::Acquire);
            let files = progress.total_files.load(Ordering::Acquire);
            let directories = progress.total_directories.load(Ordering::Acquire);
            completed_items.store(items, Ordering::Release);
            completed_files.store(files, Ordering::Release);
            completed_directories.store(directories, Ordering::Release);
            state.publish_phase(JobPhase::Finalizing, None);
            if remove_sources {
                if let Err((path, message)) =
                    remove_journaled_sources(journal.as_deref(), cancel_requested)
                {
                    push_failure(failures, Some(path), message);
                    stopped.store(true, Ordering::Release);
                    return;
                }
                for root in roots {
                    remove_empty_source_directories(&root.source, cancel_requested);
                }
            }
        }
    });
}

struct StreamingTransfer<'a> {
    plan: &'a OperationPlan,
    events: &'a Sender<OperationEvent>,
    resolutions: &'a Receiver<ConflictResolution>,
    cancelled: &'a AtomicBool,
    completed_items: &'a AtomicU64,
    completed_bytes: &'a AtomicU64,
    completed_files_counter: &'a AtomicU64,
    completed_directories_counter: &'a AtomicU64,
    file_policy: Option<ConflictAction>,
    type_policy: Option<ConflictAction>,
    work_sender: Option<Sender<PreparedFile>>,
    stopped: &'a AtomicBool,
    progress: &'a StreamingProgress,
    journal: Option<&'a Mutex<File>>,
    reserved_targets: HashSet<PathBuf>,
}

#[derive(Default)]
struct StreamingProgress {
    total_items: AtomicU64,
    total_files: AtomicU64,
    total_directories: AtomicU64,
    total_bytes: AtomicU64,
    scope_complete: AtomicBool,
}

impl StreamingProgress {
    fn from_plan(plan: &OperationPlan) -> Self {
        if plan.summary.recursive_scope_known {
            Self {
                total_items: AtomicU64::new(plan.summary.item_count),
                total_files: AtomicU64::new(plan.summary.file_count),
                total_directories: AtomicU64::new(plan.summary.directory_count),
                total_bytes: AtomicU64::new(plan.summary.total_bytes),
                scope_complete: AtomicBool::new(true),
            }
        } else {
            Self::default()
        }
    }
}

struct PreparedFile {
    task: FileTask,
    overwrite: bool,
    remove_source: bool,
    expected_target: Option<Fingerprint>,
}

impl<'a> StreamingTransfer<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        plan: &'a OperationPlan,
        events: &'a Sender<OperationEvent>,
        resolutions: &'a Receiver<ConflictResolution>,
        cancelled: &'a AtomicBool,
        completed_items: &'a AtomicU64,
        completed_bytes: &'a AtomicU64,
        completed_files: &'a AtomicU64,
        completed_directories: &'a AtomicU64,
        work_sender: Sender<PreparedFile>,
        stopped: &'a AtomicBool,
        progress: &'a StreamingProgress,
        journal: Option<&'a Mutex<File>>,
    ) -> Self {
        Self {
            plan,
            events,
            resolutions,
            cancelled,
            completed_items,
            completed_bytes,
            completed_files_counter: completed_files,
            completed_directories_counter: completed_directories,
            file_policy: None,
            type_policy: None,
            work_sender: Some(work_sender),
            stopped,
            progress,
            journal,
            reserved_targets: HashSet::new(),
        }
    }

    fn visit(&mut self, source: &Path, requested_target: &Path) -> Result<(), (PathBuf, String)> {
        if self.cancelled.load(Ordering::Acquire) || self.stopped.load(Ordering::Acquire) {
            return Err((source.to_path_buf(), "Transfer cancelled".into()));
        }
        let metadata = fs::symlink_metadata(source).map_err(|error| {
            (
                source.to_path_buf(),
                format!("Cannot inspect source: {error}"),
            )
        })?;
        let fingerprint = crate::planner::fingerprint(&metadata)
            .map_err(|message| (source.to_path_buf(), message))?;
        revalidate(source, &fingerprint).map_err(|message| (source.to_path_buf(), message))?;
        let totals_known = self.plan.summary.recursive_scope_known;
        if !totals_known {
            self.progress.total_items.fetch_add(1, Ordering::AcqRel);
        }
        match fingerprint.kind {
            ObjectKind::File => {
                if !totals_known {
                    self.progress.total_files.fetch_add(1, Ordering::AcqRel);
                    self.progress
                        .total_bytes
                        .fetch_add(fingerprint.len, Ordering::AcqRel);
                }
                self.publish(Some(source.to_path_buf()));
                self.visit_file(source, requested_target, fingerprint)
            }
            ObjectKind::Directory => {
                if !totals_known {
                    self.progress
                        .total_directories
                        .fetch_add(1, Ordering::AcqRel);
                }
                let Some(target) = self.directory_target(source, requested_target, &fingerprint)?
                else {
                    return Ok(());
                };
                let reader = fs::read_dir(source).map_err(|error| {
                    (
                        source.to_path_buf(),
                        format!("Cannot enumerate source: {error}"),
                    )
                })?;
                for child in reader {
                    let child = child.map_err(|error| {
                        (
                            source.to_path_buf(),
                            format!("Directory enumeration failed: {error}"),
                        )
                    })?;
                    self.visit(&child.path(), &target.join(child.file_name()))?;
                }
                Ok(())
            }
        }
    }

    fn directory_target(
        &mut self,
        source: &Path,
        target: &Path,
        fingerprint: &Fingerprint,
    ) -> Result<Option<PathBuf>, (PathBuf, String)> {
        match fs::symlink_metadata(target) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                let target_fingerprint = crate::planner::fingerprint(&metadata)
                    .map_err(|message| (target.to_path_buf(), message))?;
                revalidate(target, &target_fingerprint)
                    .map_err(|message| (target.to_path_buf(), message))?;
                Ok(Some(target.to_path_buf()))
            }
            Ok(metadata) => {
                let conflict = transfer_conflict(self.plan, source, target, fingerprint, &metadata);
                match self.resolve(conflict)? {
                    ConflictAction::KeepBoth => {
                        let allocated =
                            self.allocate_keep_both_target(target, ObjectKind::Directory)?;
                        fs::create_dir(&allocated).map_err(|error| {
                            (
                                allocated.clone(),
                                format!("Cannot create conflict copy folder: {error}"),
                            )
                        })?;
                        Ok(Some(allocated))
                    }
                    ConflictAction::KeepDestination | ConflictAction::Skip => Ok(None),
                    _ => Err((
                        target.to_path_buf(),
                        "Type conflicts allow Keep both, Keep destination, or Skip".into(),
                    )),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(target).map_err(|error| {
                    (
                        target.to_path_buf(),
                        format!("Cannot create folder: {error}"),
                    )
                })?;
                Ok(Some(target.to_path_buf()))
            }
            Err(error) => Err((
                target.to_path_buf(),
                format!("Cannot inspect target: {error}"),
            )),
        }
    }

    fn visit_file(
        &mut self,
        source: &Path,
        requested_target: &Path,
        fingerprint: Fingerprint,
    ) -> Result<(), (PathBuf, String)> {
        let (target, overwrite, remove_source, expected_target) =
            match fs::symlink_metadata(requested_target) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    (requested_target.to_path_buf(), false, true, None)
                }
                Err(error) => {
                    return Err((
                        requested_target.to_path_buf(),
                        format!("Cannot inspect target: {error}"),
                    ));
                }
                Ok(metadata) if metadata.is_file() => {
                    let conflict = transfer_conflict(
                        self.plan,
                        source,
                        requested_target,
                        &fingerprint,
                        &metadata,
                    );
                    if conflict.relation == VersionRelation::SameTimestamp
                        && conflict.source_bytes == conflict.destination_bytes
                        && files_identical(source, requested_target, self.cancelled)?
                    {
                        (requested_target.to_path_buf(), false, true, None)
                    } else {
                        let mut action = self.resolve(conflict.clone())?;
                        while matches!(
                            action,
                            ConflictAction::KeepNewer | ConflictAction::KeepOlder
                        ) && !matches!(
                            conflict.relation,
                            VersionRelation::SourceNewer | VersionRelation::DestinationNewer
                        ) {
                            self.file_policy = None;
                            action = self.resolve(conflict.clone())?;
                        }
                        let source_wins = match action {
                            ConflictAction::KeepSource => true,
                            ConflictAction::KeepDestination => false,
                            ConflictAction::KeepNewer => match conflict.relation {
                                VersionRelation::SourceNewer => true,
                                VersionRelation::DestinationNewer => false,
                                _ => unreachable!(),
                            },
                            ConflictAction::KeepOlder => match conflict.relation {
                                VersionRelation::SourceNewer => false,
                                VersionRelation::DestinationNewer => true,
                                _ => unreachable!(),
                            },
                            ConflictAction::KeepBoth => {
                                let target = self.allocate_keep_both_target(
                                    requested_target,
                                    ObjectKind::File,
                                )?;
                                return self.copy_and_record(
                                    source,
                                    target,
                                    fingerprint,
                                    false,
                                    true,
                                    None,
                                );
                            }
                            ConflictAction::Skip => {
                                self.complete_file(source);
                                return Ok(());
                            }
                        };
                        if source_wins {
                            let expected = crate::planner::fingerprint(&metadata)
                                .map_err(|message| (requested_target.to_path_buf(), message))?;
                            (requested_target.to_path_buf(), true, true, Some(expected))
                        } else {
                            (requested_target.to_path_buf(), false, true, None)
                        }
                    }
                }
                Ok(metadata) => {
                    let conflict = transfer_conflict(
                        self.plan,
                        source,
                        requested_target,
                        &fingerprint,
                        &metadata,
                    );
                    match self.resolve(conflict)? {
                        ConflictAction::KeepBoth => {
                            let target =
                                self.allocate_keep_both_target(requested_target, ObjectKind::File)?;
                            return self.copy_and_record(
                                source,
                                target,
                                fingerprint,
                                false,
                                true,
                                None,
                            );
                        }
                        ConflictAction::KeepDestination | ConflictAction::Skip => {
                            self.complete_file(source);
                            return Ok(());
                        }
                        _ => {
                            return Err((
                                requested_target.to_path_buf(),
                                "Type conflicts allow Keep both, Keep destination, or Skip".into(),
                            ));
                        }
                    }
                }
            };
        if overwrite || !target.exists() {
            self.copy_and_record(
                source,
                target,
                fingerprint,
                overwrite,
                remove_source,
                expected_target.as_ref(),
            )
        } else {
            if remove_source {
                self.record_source(source, &fingerprint)?;
            }
            self.complete_file(source);
            Ok(())
        }
    }

    fn copy_and_record(
        &mut self,
        source: &Path,
        target: PathBuf,
        fingerprint: Fingerprint,
        overwrite: bool,
        remove_source: bool,
        expected_target: Option<&Fingerprint>,
    ) -> Result<(), (PathBuf, String)> {
        if !self.reserved_targets.insert(target.clone()) {
            return Err((
                target,
                "Multiple sources map to the same destination path".into(),
            ));
        }
        let prepared = PreparedFile {
            task: FileTask {
                source: source.to_path_buf(),
                target,
                fingerprint,
            },
            overwrite,
            remove_source,
            expected_target: expected_target.cloned(),
        };
        let mut pending = prepared;
        loop {
            if self.cancelled.load(Ordering::Acquire) || self.stopped.load(Ordering::Acquire) {
                return Err((source.to_path_buf(), "Transfer cancelled".into()));
            }
            let Some(sender) = &self.work_sender else {
                return Err((source.to_path_buf(), "Transfer queue closed".into()));
            };
            match sender.send_timeout(pending, std::time::Duration::from_millis(100)) {
                Ok(()) => return Ok(()),
                Err(SendTimeoutError::Timeout(returned)) => pending = returned,
                Err(SendTimeoutError::Disconnected(_)) => {
                    return Err((source.to_path_buf(), "Transfer queue closed".into()));
                }
            }
        }
    }

    fn resolve(&mut self, conflict: TransferConflict) -> Result<ConflictAction, (PathBuf, String)> {
        let policy = match conflict.kind {
            ConflictKind::FileToFile => self.file_policy,
            ConflictKind::TypeMismatch => self.type_policy,
        };
        if let Some(action) = policy {
            return Ok(action);
        }
        self.events
            .send(OperationEvent::Conflict(conflict.clone()))
            .map_err(|_| (conflict.source.clone(), "Conflict channel closed".into()))?;
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return Err((conflict.source.clone(), "Transfer cancelled".into()));
            }
            match self
                .resolutions
                .recv_timeout(std::time::Duration::from_millis(100))
            {
                Ok(resolution) if resolution.job == self.plan.summary.job => {
                    if resolution.apply_to_all {
                        match conflict.kind {
                            ConflictKind::FileToFile => self.file_policy = Some(resolution.action),
                            ConflictKind::TypeMismatch => {
                                self.type_policy = Some(resolution.action)
                            }
                        }
                    }
                    return Ok(resolution.action);
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err((conflict.source.clone(), "Conflict channel closed".into()));
                }
            }
        }
    }

    fn record_source(
        &mut self,
        source: &Path,
        fingerprint: &Fingerprint,
    ) -> Result<(), (PathBuf, String)> {
        record_source(self.journal, source, fingerprint)
    }

    fn complete_file(&mut self, path: &Path) {
        self.completed_files_counter.fetch_add(1, Ordering::AcqRel);
        self.completed_items.fetch_add(1, Ordering::AcqRel);
        self.publish(Some(path.to_path_buf()));
    }

    fn allocate_keep_both_target(
        &self,
        requested: &Path,
        kind: ObjectKind,
    ) -> Result<PathBuf, (PathBuf, String)> {
        keep_both_target(requested, kind, &self.reserved_targets)
            .map_err(|message| (requested.to_path_buf(), message))
    }

    fn close_queue(&mut self) {
        self.work_sender.take();
    }

    fn publish(&self, current_path: Option<PathBuf>) {
        self.publish_phase(JobPhase::Running, current_path);
    }

    fn publish_phase(&self, phase: JobPhase, current_path: Option<PathBuf>) {
        let _ = self
            .events
            .try_send(OperationEvent::Progress(OperationProgress {
                job: self.plan.summary.job,
                kind: self.plan.summary.kind,
                phase,
                completed_items: self.completed_items.load(Ordering::Acquire),
                total_items: self.progress.total_items.load(Ordering::Acquire),
                completed_files: self.completed_files_counter.load(Ordering::Acquire),
                total_files: self.progress.total_files.load(Ordering::Acquire),
                completed_directories: self.completed_directories_counter.load(Ordering::Acquire),
                total_directories: self.progress.total_directories.load(Ordering::Acquire),
                completed_bytes: self.completed_bytes.load(Ordering::Acquire),
                total_bytes: self.progress.total_bytes.load(Ordering::Acquire),
                scope_complete: self.progress.scope_complete.load(Ordering::Acquire),
                current_path,
            }));
    }
}

#[allow(clippy::too_many_arguments)]
fn transfer_worker(
    receiver: Receiver<PreparedFile>,
    verification: VerificationMode,
    events: &Sender<OperationEvent>,
    plan: &OperationPlan,
    cancelled: &AtomicBool,
    completed_items: &AtomicU64,
    completed_bytes: &AtomicU64,
    completed_files: &AtomicU64,
    completed_directories: &AtomicU64,
    failures: &Mutex<Vec<OperationFailure>>,
    stopped: &AtomicBool,
    progress: &StreamingProgress,
    journal: Option<&Mutex<File>>,
) {
    while let Ok(prepared) = receiver.recv() {
        if cancelled.load(Ordering::Acquire) || stopped.load(Ordering::Acquire) {
            continue;
        }
        let result = copy_file(
            &prepared.task,
            verification == VerificationMode::Full,
            prepared.overwrite,
            prepared.expected_target.as_ref(),
            progress,
            cancelled,
            completed_bytes,
            completed_files,
            completed_directories,
            events,
            plan,
            completed_items,
        );
        if let Err(error) = result {
            let message = match error {
                CopyError::Cancelled => "Transfer cancelled".into(),
                CopyError::Failed(message) => message,
            };
            if !cancelled.load(Ordering::Acquire) {
                push_failure(failures, Some(prepared.task.source.clone()), message);
            }
            stopped.store(true, Ordering::Release);
            continue;
        }
        if prepared.remove_source
            && let Err((path, message)) =
                record_source(journal, &prepared.task.source, &prepared.task.fingerprint)
        {
            push_failure(failures, Some(path), message);
            stopped.store(true, Ordering::Release);
            continue;
        }
        completed_files.fetch_add(1, Ordering::AcqRel);
        completed_items.fetch_add(1, Ordering::AcqRel);
        send_stream_progress(
            events,
            plan,
            JobPhase::Running,
            Some(prepared.task.target),
            progress,
            completed_items,
            completed_bytes,
            completed_files,
            completed_directories,
        );
    }
}

fn record_source(
    journal: Option<&Mutex<File>>,
    source: &Path,
    fingerprint: &Fingerprint,
) -> Result<(), (PathBuf, String)> {
    let Some(journal) = journal else {
        return Ok(());
    };
    let mut journal = journal
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    write_journal_entry(&mut journal, source, fingerprint).map_err(|error| {
        (
            source.to_path_buf(),
            format!("Cannot write move journal: {error}"),
        )
    })
}

fn remove_journaled_sources(
    journal: Option<&Mutex<File>>,
    cancelled: &AtomicBool,
) -> Result<(), (PathBuf, String)> {
    let Some(journal) = journal else {
        return Ok(());
    };
    let mut journal = journal
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    journal.flush().map_err(|error| {
        (
            PathBuf::new(),
            format!("Cannot flush move journal: {error}"),
        )
    })?;
    journal
        .seek(SeekFrom::Start(0))
        .map_err(|error| (PathBuf::new(), format!("Cannot read move journal: {error}")))?;
    while let Some((source, fingerprint)) = read_journal_entry(&mut journal)
        .map_err(|error| (PathBuf::new(), format!("Cannot read move journal: {error}")))?
    {
        if cancelled.load(Ordering::Acquire) {
            return Err((
                source,
                "Move cancelled before source cleanup completed".into(),
            ));
        }
        revalidate(&source, &fingerprint).map_err(|message| (source.clone(), message))?;
        fs::remove_file(&source).map_err(|error| {
            (
                source.clone(),
                format!("Verified copy retained, but source removal failed: {error}"),
            )
        })?;
    }
    Ok(())
}

fn transfer_conflict(
    plan: &OperationPlan,
    source: &Path,
    destination: &Path,
    source_fingerprint: &Fingerprint,
    destination_metadata: &fs::Metadata,
) -> TransferConflict {
    let source_modified = source_fingerprint.modified;
    let destination_modified = destination_metadata.modified().ok();
    let relation = match (source_modified, destination_modified) {
        (Some(source), Some(destination)) if source > destination => VersionRelation::SourceNewer,
        (Some(source), Some(destination)) if source < destination => {
            VersionRelation::DestinationNewer
        }
        (Some(_), Some(_)) => VersionRelation::SameTimestamp,
        _ => VersionRelation::Unknown,
    };
    TransferConflict {
        job: plan.summary.job,
        kind: if destination_metadata.is_file() && source_fingerprint.kind == ObjectKind::File {
            ConflictKind::FileToFile
        } else {
            ConflictKind::TypeMismatch
        },
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
        source_bytes: source_fingerprint.len,
        destination_bytes: destination_metadata.len(),
        source_modified,
        destination_modified,
        relation,
    }
}

fn files_identical(
    source: &Path,
    destination: &Path,
    cancelled: &AtomicBool,
) -> Result<bool, (PathBuf, String)> {
    let mut source_file = File::open(source).map_err(|error| {
        (
            source.to_path_buf(),
            format!("Cannot compare source: {error}"),
        )
    })?;
    let mut destination_file = File::open(destination).map_err(|error| {
        (
            destination.to_path_buf(),
            format!("Cannot compare destination: {error}"),
        )
    })?;
    let source_hash = hash_reader(&mut source_file, cancelled).map_err(|error| match error {
        CopyError::Cancelled => (source.to_path_buf(), "Comparison cancelled".into()),
        CopyError::Failed(message) => (source.to_path_buf(), message),
    })?;
    let destination_hash =
        hash_reader(&mut destination_file, cancelled).map_err(|error| match error {
            CopyError::Cancelled => (destination.to_path_buf(), "Comparison cancelled".into()),
            CopyError::Failed(message) => (destination.to_path_buf(), message),
        })?;
    Ok(source_hash == destination_hash)
}

fn remove_empty_source_directories(path: &Path, cancelled: &AtomicBool) {
    if cancelled.load(Ordering::Acquire) {
        return;
    }
    if revalidate_directory_kind(path).is_err() {
        return;
    }
    if let Ok(reader) = fs::read_dir(path) {
        for child in reader.flatten() {
            remove_empty_source_directories(&child.path(), cancelled);
        }
    }
    let _ = fs::remove_dir(path);
}

fn write_journal_entry(file: &mut File, path: &Path, fingerprint: &Fingerprint) -> io::Result<()> {
    let encoded = encode_path(path);
    file.write_all(&(encoded.len() as u32).to_le_bytes())?;
    file.write_all(&encoded)?;
    file.write_all(&fingerprint.len.to_le_bytes())?;
    let modified = fingerprint
        .modified
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
    file.write_all(
        &modified
            .map_or(u64::MAX, |value| value.as_secs())
            .to_le_bytes(),
    )?;
    file.write_all(
        &modified
            .map_or(0, |value| value.subsec_nanos())
            .to_le_bytes(),
    )
}

fn read_journal_entry(file: &mut File) -> io::Result<Option<(PathBuf, Fingerprint)>> {
    let mut length = [0_u8; 4];
    match file.read_exact(&mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut encoded = vec![0_u8; u32::from_le_bytes(length) as usize];
    file.read_exact(&mut encoded)?;
    let mut len = [0_u8; 8];
    file.read_exact(&mut len)?;
    let mut seconds = [0_u8; 8];
    file.read_exact(&mut seconds)?;
    let mut nanos = [0_u8; 4];
    file.read_exact(&mut nanos)?;
    let seconds = u64::from_le_bytes(seconds);
    let modified = (seconds != u64::MAX).then(|| {
        std::time::UNIX_EPOCH + std::time::Duration::new(seconds, u32::from_le_bytes(nanos))
    });
    Ok(Some((
        decode_path(encoded),
        Fingerprint {
            kind: ObjectKind::File,
            len: u64::from_le_bytes(len),
            modified,
        },
    )))
}

#[cfg(windows)]
fn encode_path(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(windows)]
fn decode_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    std::ffi::OsString::from_wide(&units).into()
}

#[cfg(unix)]
fn encode_path(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(unix)]
fn decode_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes).into()
}

#[allow(clippy::too_many_arguments)]
fn copy_file(
    task: &FileTask,
    verify_hash: bool,
    overwrite: bool,
    expected_target: Option<&Fingerprint>,
    progress: &StreamingProgress,
    cancel_requested: &AtomicBool,
    completed_bytes: &AtomicU64,
    completed_files: &AtomicU64,
    completed_directories: &AtomicU64,
    events: &Sender<OperationEvent>,
    plan: &OperationPlan,
    completed_items: &AtomicU64,
) -> Result<(), CopyError> {
    let mut local_bytes = 0_u64;
    let result = copy_file_inner(
        task,
        verify_hash,
        overwrite,
        expected_target,
        progress,
        cancel_requested,
        completed_bytes,
        completed_files,
        completed_directories,
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
    overwrite: bool,
    expected_target: Option<&Fingerprint>,
    progress: &StreamingProgress,
    cancel_requested: &AtomicBool,
    completed_bytes: &AtomicU64,
    completed_files: &AtomicU64,
    completed_directories: &AtomicU64,
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
        completed_bytes.fetch_add(count, Ordering::AcqRel);
        send_stream_progress(
            events,
            plan,
            JobPhase::Running,
            Some(task.source.clone()),
            progress,
            completed_items,
            completed_bytes,
            completed_files,
            completed_directories,
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
        send_stream_progress(
            events,
            plan,
            JobPhase::Verifying,
            Some(task.target.clone()),
            progress,
            completed_items,
            completed_bytes,
            completed_files,
            completed_directories,
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
    if let Some(expected) = expected_target {
        revalidate(&task.target, expected).map_err(CopyError::Failed)?;
    }
    if overwrite {
        temporary
            .persist(&task.target)
            .map_err(|error| CopyError::Failed(format!("Cannot replace destination: {error}")))?;
    } else {
        temporary.persist_noclobber(&task.target).map_err(|error| {
            CopyError::Failed(format!(
                "Cannot publish destination without overwriting: {error}"
            ))
        })?;
    }
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
        let items = completed_items.fetch_add(1, Ordering::AcqRel) + 1;
        send_progress(
            events,
            plan,
            JobPhase::Finalizing,
            items,
            plan.summary.total_bytes,
            Some(root.target.clone()),
        );
    }
    completed_items.store(plan.summary.item_count, Ordering::Release);
    send_progress(
        events,
        plan,
        JobPhase::Finalizing,
        plan.summary.item_count,
        plan.summary.total_bytes,
        plan.summary.destination.clone(),
    );
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

#[allow(clippy::too_many_arguments)]
fn execute_permanent_delete(
    plan: &OperationPlan,
    manifest: &File,
    events: &Sender<OperationEvent>,
    cancel_requested: &AtomicBool,
    completed_items: &AtomicU64,
    completed_bytes: &AtomicU64,
    completed_files: &AtomicU64,
    completed_directories: &AtomicU64,
    failures: &Mutex<Vec<OperationFailure>>,
) {
    for pass in [ObjectKind::File, ObjectKind::Directory] {
        let mut reader = match manifest.try_clone() {
            Ok(reader) => reader,
            Err(error) => {
                push_failure(
                    failures,
                    None,
                    format!("Cannot open temporary delete manifest: {error}"),
                );
                return;
            }
        };
        if let Err(error) = reader.seek(SeekFrom::Start(0)) {
            push_failure(
                failures,
                None,
                format!("Cannot read temporary delete manifest: {error}"),
            );
            return;
        }
        loop {
            if cancel_requested.load(Ordering::Acquire) {
                return;
            }
            let entry = match read_delete_manifest_entry(&mut reader) {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) => {
                    push_failure(
                        failures,
                        None,
                        format!("Cannot read temporary delete manifest: {error}"),
                    );
                    return;
                }
            };
            let (path, fingerprint) = entry;
            if fingerprint.kind != pass {
                continue;
            }
            match pass {
                ObjectKind::File => {
                    if let Err(message) = revalidate(&path, &fingerprint) {
                        push_failure(failures, Some(path), message);
                        return;
                    }
                    if let Err(error) = fs::remove_file(&path) {
                        push_failure(
                            failures,
                            Some(path),
                            format!("Permanent file deletion failed: {error}"),
                        );
                        return;
                    }
                    let bytes = completed_bytes.fetch_add(fingerprint.len, Ordering::AcqRel)
                        + fingerprint.len;
                    let items = completed_items.fetch_add(1, Ordering::AcqRel) + 1;
                    let files = completed_files.fetch_add(1, Ordering::AcqRel) + 1;
                    send_delete_progress(
                        events,
                        plan,
                        JobPhase::Running,
                        items,
                        files,
                        completed_directories.load(Ordering::Acquire),
                        bytes,
                        Some(path),
                    );
                }
                ObjectKind::Directory => {
                    if let Err(message) = revalidate_directory_kind(&path) {
                        push_failure(failures, Some(path), message);
                        return;
                    }
                    if let Err(error) = fs::remove_dir(&path) {
                        push_failure(
                            failures,
                            Some(path),
                            format!("Permanent directory deletion failed: {error}"),
                        );
                        return;
                    }
                    let items = completed_items.fetch_add(1, Ordering::AcqRel) + 1;
                    let directories = completed_directories.fetch_add(1, Ordering::AcqRel) + 1;
                    send_delete_progress(
                        events,
                        plan,
                        JobPhase::Finalizing,
                        items,
                        completed_files.load(Ordering::Acquire),
                        directories,
                        completed_bytes.load(Ordering::Acquire),
                        Some(path),
                    );
                }
            }
        }
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
        completed_files: completed_items.min(plan.summary.file_count),
        total_files: plan.summary.file_count,
        completed_directories: completed_items.saturating_sub(plan.summary.file_count),
        total_directories: plan.summary.directory_count,
        completed_bytes,
        total_bytes: plan.summary.total_bytes,
        scope_complete: true,
        current_path,
    }));
}

#[allow(clippy::too_many_arguments)]
fn send_stream_progress(
    events: &Sender<OperationEvent>,
    plan: &OperationPlan,
    phase: JobPhase,
    current_path: Option<PathBuf>,
    progress: &StreamingProgress,
    completed_items: &AtomicU64,
    completed_bytes: &AtomicU64,
    completed_files: &AtomicU64,
    completed_directories: &AtomicU64,
) {
    let _ = events.try_send(OperationEvent::Progress(OperationProgress {
        job: plan.summary.job,
        kind: plan.summary.kind,
        phase,
        completed_items: completed_items.load(Ordering::Acquire),
        total_items: progress.total_items.load(Ordering::Acquire),
        completed_files: completed_files.load(Ordering::Acquire),
        total_files: progress.total_files.load(Ordering::Acquire),
        completed_directories: completed_directories.load(Ordering::Acquire),
        total_directories: progress.total_directories.load(Ordering::Acquire),
        completed_bytes: completed_bytes.load(Ordering::Acquire),
        total_bytes: progress.total_bytes.load(Ordering::Acquire),
        scope_complete: progress.scope_complete.load(Ordering::Acquire),
        current_path,
    }));
}

#[allow(clippy::too_many_arguments)]
fn send_delete_progress(
    events: &Sender<OperationEvent>,
    plan: &OperationPlan,
    phase: JobPhase,
    completed_items: u64,
    completed_files: u64,
    completed_directories: u64,
    completed_bytes: u64,
    current_path: Option<PathBuf>,
) {
    let _ = events.try_send(OperationEvent::Progress(OperationProgress {
        job: plan.summary.job,
        kind: plan.summary.kind,
        phase,
        completed_items,
        total_items: plan.summary.item_count,
        completed_files,
        total_files: plan.summary.file_count,
        completed_directories,
        total_directories: plan.summary.directory_count,
        completed_bytes,
        total_bytes: plan.summary.total_bytes,
        scope_complete: true,
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
