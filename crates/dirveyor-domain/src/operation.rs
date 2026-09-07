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
        verification: VerificationMode,
    },
    Move {
        sources: Vec<PathBuf>,
        destination: PathBuf,
        verification: VerificationMode,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VerificationMode {
    Fast,
    #[default]
    Full,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictAction {
    KeepNewer,
    KeepOlder,
    KeepSource,
    KeepDestination,
    KeepBoth,
    Skip,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictKind {
    FileToFile,
    TypeMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersionRelation {
    SourceNewer,
    DestinationNewer,
    SameTimestamp,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferConflict {
    pub job: JobId,
    pub kind: ConflictKind,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub source_bytes: u64,
    pub destination_bytes: u64,
    pub source_modified: Option<std::time::SystemTime>,
    pub destination_modified: Option<std::time::SystemTime>,
    pub relation: VersionRelation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictPrompt {
    pub conflict: TransferConflict,
    pub apply_to_all: bool,
}

impl VerificationMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast (size + flush)",
            Self::Full => "Full (SHA-256)",
        }
    }

    pub const fn toggle(self) -> Self {
        match self {
            Self::Fast => Self::Full,
            Self::Full => Self::Fast,
        }
    }
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
            Self::ParallelCopy => "streaming copy with interactive conflict handling",
            Self::CopyVerifyRemove => "streaming copy, verify, then journaled source cleanup",
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
    pub verification: Option<VerificationMode>,
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
    pub scope_complete: bool,
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
    Conflict(ConflictPrompt),
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
            Self::Planning(_) | Self::Review(_) | Self::Running(_) | Self::Conflict(_)
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
            verification: VerificationMode::Full,
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
