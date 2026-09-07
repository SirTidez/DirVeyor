use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct JobId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationKind {
    Copy,
    Move,
    Recycle,
    Rename,
    CreateDirectory,
}

impl OperationKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Move => "move",
            Self::Recycle => "recycle",
            Self::Rename => "rename",
            Self::CreateDirectory => "create folder",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationIntent {
    Copy {
        sources: Vec<PathBuf>,
        destination: PathBuf,
    },
    Move {
        sources: Vec<PathBuf>,
        destination: PathBuf,
    },
    Recycle {
        sources: Vec<PathBuf>,
    },
    Rename {
        source: PathBuf,
        new_name: OsString,
    },
    CreateDirectory {
        parent: PathBuf,
        name: OsString,
    },
}

impl OperationIntent {
    pub const fn kind(&self) -> OperationKind {
        match self {
            Self::Copy { .. } => OperationKind::Copy,
            Self::Move { .. } => OperationKind::Move,
            Self::Recycle { .. } => OperationKind::Recycle,
            Self::Rename { .. } => OperationKind::Rename,
            Self::CreateDirectory { .. } => OperationKind::CreateDirectory,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlannedStrategy {
    ParallelCopy,
    CopyVerifyRemove,
    AtomicRename,
    RecycleBin,
    ExclusiveCreate,
}

impl PlannedStrategy {
    pub const fn label(self) -> &'static str {
        match self {
            Self::ParallelCopy => "bounded copy with no-overwrite publication",
            Self::CopyVerifyRemove => "copy, SHA-256 verify, then remove source",
            Self::AtomicRename => "same-volume atomic no-replace rename",
            Self::RecycleBin => "operating-system Recycle Bin / Trash",
            Self::ExclusiveCreate => "exclusive create; existing names fail",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictSummary {
    pub source: PathBuf,
    pub requested_destination: PathBuf,
    pub resolved_destination: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanSummary {
    pub job: JobId,
    pub kind: OperationKind,
    pub sources: Vec<PathBuf>,
    pub destination: Option<PathBuf>,
    pub strategy: PlannedStrategy,
    pub item_count: u64,
    pub file_count: u64,
    pub total_bytes: u64,
    pub conflicts: Vec<ConflictSummary>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobPhase {
    Planning,
    AwaitingReview,
    Running,
    Cancelling,
    Verifying,
    Finalizing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationProgress {
    pub job: JobId,
    pub kind: OperationKind,
    pub phase: JobPhase,
    pub completed_items: u64,
    pub total_items: u64,
    pub completed_bytes: u64,
    pub total_bytes: u64,
    pub current_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationFailure {
    pub path: Option<PathBuf>,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobOutcome {
    Completed,
    Partial,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationReport {
    pub job: JobId,
    pub kind: OperationKind,
    pub outcome: JobOutcome,
    pub completed_items: u64,
    pub total_items: u64,
    pub completed_bytes: u64,
    pub failures: Vec<OperationFailure>,
    pub affected_directories: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationView {
    Idle,
    Planning {
        job: JobId,
        kind: OperationKind,
    },
    Review(PlanSummary),
    Running(OperationProgress),
    Finished(OperationReport),
    Error {
        job: Option<JobId>,
        kind: OperationKind,
        message: String,
    },
}

impl OperationView {
    pub const fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Planning { .. } | Self::Review(_) | Self::Running(_)
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextAction {
    Rename { source: PathBuf },
    CreateDirectory { parent: PathBuf },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextPrompt {
    pub action: TextAction,
    pub value: String,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_kind_is_stable() {
        let intent = OperationIntent::Copy {
            sources: vec![PathBuf::from("source")],
            destination: PathBuf::from("destination"),
        };
        assert_eq!(intent.kind(), OperationKind::Copy);
    }

    #[test]
    fn operation_busy_state_requires_an_unfinished_job() {
        assert!(
            OperationView::Planning {
                job: JobId(1),
                kind: OperationKind::Copy,
            }
            .is_busy()
        );
        assert!(!OperationView::Idle.is_busy());
    }
}
