use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct JobId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationKind {
    Copy,
    Move,
    Recycle,
    PermanentDelete,
    Rename,
    CreateDirectory,
}

impl OperationKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Move => "move",
            Self::Recycle => "recycle",
            Self::PermanentDelete => "permanent delete",
            Self::Rename => "rename",
            Self::CreateDirectory => "create folder",
        }
    }

    pub const fn is_delete(self) -> bool {
        matches!(self, Self::Recycle | Self::PermanentDelete)
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
    PermanentDelete {
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
            Self::PermanentDelete { .. } => OperationKind::PermanentDelete,
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
    PermanentDelete,
    ExclusiveCreate,
}

impl PlannedStrategy {
    pub const fn label(self) -> &'static str {
        match self {
            Self::ParallelCopy => "bounded copy with no-overwrite publication",
            Self::CopyVerifyRemove => "copy, SHA-256 verify, then remove source",
            Self::AtomicRename => "same-volume atomic no-replace rename",
            Self::RecycleBin => "operating-system Recycle Bin / Trash",
            Self::PermanentDelete => "irreversible recursive deletion",
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
    pub directory_count: u64,
    pub total_bytes: u64,
    pub recursive_scope_known: bool,
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
    pub completed_files: u64,
    pub total_files: u64,
    pub completed_directories: u64,
    pub total_directories: u64,
    pub completed_bytes: u64,
    pub total_bytes: u64,
    pub current_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationPlanningProgress {
    pub job: JobId,
    pub kind: OperationKind,
    pub discovered_items: u64,
    pub discovered_files: u64,
    pub discovered_directories: u64,
    pub discovered_bytes: u64,
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
    pub completed_files: u64,
    pub total_files: u64,
    pub completed_directories: u64,
    pub total_directories: u64,
    pub completed_bytes: u64,
    pub total_bytes: u64,
    pub failures: Vec<OperationFailure>,
    pub affected_directories: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationView {
    Idle,
    Planning(OperationPlanningProgress),
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
        matches!(self, Self::Planning(_) | Self::Review(_) | Self::Running(_))
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

        let delete = OperationIntent::PermanentDelete {
            sources: vec![PathBuf::from("source")],
        };
        assert_eq!(delete.kind(), OperationKind::PermanentDelete);
        assert!(delete.kind().is_delete());
    }

    #[test]
    fn operation_busy_state_requires_an_unfinished_job() {
        assert!(
            OperationView::Planning(OperationPlanningProgress {
                job: JobId(1),
                kind: OperationKind::Copy,
                discovered_items: 0,
                discovered_files: 0,
                discovered_directories: 0,
                discovered_bytes: 0,
                current_path: None,
            })
            .is_busy()
        );
        assert!(!OperationView::Idle.is_busy());
    }
}
