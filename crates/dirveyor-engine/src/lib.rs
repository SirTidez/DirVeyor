//! Reviewed filesystem operations for DirVeyor.
//!
//! Submitting an intent can only produce a reviewable plan. Its job id must be
//! approved before any filesystem mutation is attempted.

mod executor;
mod planner;

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use dirveyor_domain::{
    ConflictAction, JobId, OperationIntent, OperationPlanningProgress, OperationProgress,
    OperationReport, PlanSummary, TransferConflict,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

const COMMAND_CAPACITY: usize = 4;
const EVENT_CAPACITY: usize = 32;

#[derive(Clone, Debug)]
pub enum OperationEvent {
    Planning(OperationPlanningProgress),
    PlanReady(PlanSummary),
    Progress(OperationProgress),
    Conflict(TransferConflict),
    Finished(OperationReport),
    Failed {
        job: Option<JobId>,
        kind: dirveyor_domain::OperationKind,
        message: String,
    },
}

enum Command {
    Plan { job: JobId, intent: OperationIntent },
    Approve(JobId),
    Abandon(JobId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitError {
    Busy,
    Closed,
}

pub struct OperationEngine {
    commands: Sender<Command>,
    events: Receiver<OperationEvent>,
    busy: Arc<AtomicBool>,
    cancel_requested: Arc<AtomicBool>,
    next_job: AtomicU64,
    conflict_resolutions: Sender<ConflictResolution>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ConflictResolution {
    pub job: JobId,
    pub action: ConflictAction,
    pub apply_to_all: bool,
}

impl OperationEngine {
    pub fn new() -> Self {
        let (command_tx, command_rx) = bounded(COMMAND_CAPACITY);
        let (event_tx, event_rx) = bounded(EVENT_CAPACITY);
        let (conflict_tx, conflict_rx) = bounded(1);
        let busy = Arc::new(AtomicBool::new(false));
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let worker_busy = Arc::clone(&busy);
        let worker_cancel = Arc::clone(&cancel_requested);
        thread::Builder::new()
            .name("dirveyor-operation-coordinator".into())
            .spawn(move || {
                coordinator(
                    command_rx,
                    event_tx,
                    conflict_rx,
                    worker_busy,
                    worker_cancel,
                )
            })
            .expect("failed to start operation coordinator");

        Self {
            commands: command_tx,
            events: event_rx,
            busy,
            cancel_requested,
            next_job: AtomicU64::new(1),
            conflict_resolutions: conflict_tx,
        }
    }

    pub fn submit(&self, intent: OperationIntent) -> Result<JobId, SubmitError> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SubmitError::Busy);
        }
        self.cancel_requested.store(false, Ordering::Release);
        let job = JobId(self.next_job.fetch_add(1, Ordering::Relaxed));
        match self.commands.try_send(Command::Plan { job, intent }) {
            Ok(()) => Ok(job),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.busy.store(false, Ordering::Release);
                Err(SubmitError::Closed)
            }
        }
    }

    pub fn approve(&self, job: JobId) -> Result<(), SubmitError> {
        self.commands
            .try_send(Command::Approve(job))
            .map_err(|error| match error {
                TrySendError::Full(_) => SubmitError::Busy,
                TrySendError::Disconnected(_) => SubmitError::Closed,
            })
    }

    pub fn abandon(&self, job: JobId) -> Result<(), SubmitError> {
        self.commands
            .try_send(Command::Abandon(job))
            .map_err(|error| match error {
                TrySendError::Full(_) => SubmitError::Busy,
                TrySendError::Disconnected(_) => SubmitError::Closed,
            })
    }

    pub fn cancel(&self) {
        self.cancel_requested.store(true, Ordering::Release);
    }

    pub fn resolve_conflict(
        &self,
        job: JobId,
        action: ConflictAction,
        apply_to_all: bool,
    ) -> Result<(), SubmitError> {
        self.conflict_resolutions
            .try_send(ConflictResolution {
                job,
                action,
                apply_to_all,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => SubmitError::Busy,
                TrySendError::Disconnected(_) => SubmitError::Closed,
            })
    }

    pub fn try_recv(&self) -> Result<OperationEvent, TryRecvError> {
        self.events.try_recv()
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
}

impl Default for OperationEngine {
    fn default() -> Self {
        Self::new()
    }
}

fn coordinator(
    commands: Receiver<Command>,
    events: Sender<OperationEvent>,
    conflict_resolutions: Receiver<ConflictResolution>,
    busy: Arc<AtomicBool>,
    cancel_requested: Arc<AtomicBool>,
) {
    let mut pending: Option<planner::OperationPlan> = None;
    while let Ok(command) = commands.recv() {
        match command {
            Command::Plan { job, intent } => {
                let kind = intent.kind();
                let initial = OperationPlanningProgress {
                    job,
                    kind,
                    discovered_items: 0,
                    discovered_files: 0,
                    discovered_directories: 0,
                    discovered_bytes: 0,
                    current_path: None,
                };
                let _ = events.send(OperationEvent::Planning(initial));
                let mut last_update = std::time::Instant::now();
                let mut publish_progress = |progress: OperationPlanningProgress| {
                    if progress.discovered_items == 1
                        || last_update.elapsed() >= std::time::Duration::from_millis(75)
                    {
                        let _ = events.try_send(OperationEvent::Planning(progress));
                        last_update = std::time::Instant::now();
                    }
                };
                match planner::build_plan_with_progress(
                    job,
                    intent,
                    &cancel_requested,
                    &mut publish_progress,
                ) {
                    Ok(_) if cancel_requested.load(Ordering::Acquire) => {
                        let _ = events.send(OperationEvent::Failed {
                            job: Some(job),
                            kind,
                            message: "Planning cancelled; no files changed".into(),
                        });
                        busy.store(false, Ordering::Release);
                    }
                    Ok(plan) => {
                        let _ = events.send(OperationEvent::PlanReady(plan.summary.clone()));
                        pending = Some(plan);
                    }
                    Err(message) => {
                        let _ = events.send(OperationEvent::Failed {
                            job: Some(job),
                            kind,
                            message,
                        });
                        busy.store(false, Ordering::Release);
                    }
                }
            }
            Command::Approve(job) => {
                let Some(plan) = pending.take().filter(|plan| plan.summary.job == job) else {
                    continue;
                };
                let report =
                    executor::execute(plan, &events, &conflict_resolutions, &cancel_requested);
                let _ = events.send(OperationEvent::Finished(report));
                busy.store(false, Ordering::Release);
            }
            Command::Abandon(job) => {
                if pending.as_ref().is_some_and(|plan| plan.summary.job == job) {
                    pending = None;
                    busy.store(false, Ordering::Release);
                }
            }
        }
    }
}
