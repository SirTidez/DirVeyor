use fileadmin_domain::{
    ConflictSummary, JobId, OperationIntent, OperationKind, OperationPlanningProgress, PlanSummary,
    PlannedStrategy,
};
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

const MAX_PLAN_ITEMS: usize = 100_000;
const MAX_SOURCES: usize = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObjectKind {
    File,
    Directory,
}

#[derive(Clone, Debug)]
pub(crate) struct Fingerprint {
    pub kind: ObjectKind,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub(crate) struct FileTask {
    pub source: PathBuf,
    pub target: PathBuf,
    pub fingerprint: Fingerprint,
}

#[derive(Clone, Debug)]
pub(crate) struct DirectoryTask {
    pub source: PathBuf,
    pub target: PathBuf,
    pub fingerprint: Fingerprint,
}

#[derive(Clone, Debug)]
pub(crate) struct TransferRoot {
    pub source: PathBuf,
    pub target: PathBuf,
    pub source_fingerprint: Fingerprint,
    pub directories: Vec<DirectoryTask>,
    pub files: Vec<FileTask>,
}

#[derive(Clone, Debug)]
pub(crate) enum PlannedAction {
    Transfer {
        roots: Vec<TransferRoot>,
        remove_sources: bool,
        worker_count: usize,
    },
    AtomicMove {
        roots: Vec<TransferRoot>,
    },
    Recycle {
        sources: Vec<(PathBuf, Fingerprint, u64)>,
    },
    PermanentDelete {
        roots: Vec<TransferRoot>,
    },
    Rename {
        source: PathBuf,
        target: PathBuf,
        fingerprint: Fingerprint,
    },
    CreateDirectory {
        target: PathBuf,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct OperationPlan {
    pub summary: PlanSummary,
    pub action: PlannedAction,
    pub affected_directories: Vec<PathBuf>,
}

#[cfg(test)]
pub(crate) fn build_plan(
    job: JobId,
    intent: OperationIntent,
    cancelled: &AtomicBool,
) -> Result<OperationPlan, String> {
    build_plan_with_progress(job, intent, cancelled, &mut |_| {})
}

pub(crate) fn build_plan_with_progress(
    job: JobId,
    intent: OperationIntent,
    cancelled: &AtomicBool,
    progress: &mut dyn FnMut(OperationPlanningProgress),
) -> Result<OperationPlan, String> {
    check_cancelled(cancelled)?;
    let kind = intent.kind();
    match intent {
        OperationIntent::Copy {
            sources,
            destination,
        } => build_transfer(
            job,
            OperationKind::Copy,
            sources,
            destination,
            cancelled,
            progress,
        ),
        OperationIntent::Move {
            sources,
            destination,
        } => build_transfer(
            job,
            OperationKind::Move,
            sources,
            destination,
            cancelled,
            progress,
        ),
        OperationIntent::Recycle { sources } => {
            build_delete(job, sources, false, cancelled, progress)
        }
        OperationIntent::PermanentDelete { sources } => {
            build_delete(job, sources, true, cancelled, progress)
        }
        OperationIntent::Rename { source, new_name } => {
            let result = build_rename(job, source, new_name, cancelled);
            progress(OperationPlanningProgress {
                job,
                kind,
                discovered_items: u64::from(result.is_ok()),
                discovered_files: 0,
                discovered_directories: 0,
                discovered_bytes: 0,
                current_path: None,
            });
            result
        }
        OperationIntent::CreateDirectory { parent, name } => {
            build_mkdir(job, parent, name, cancelled)
        }
    }
}

fn build_transfer(
    job: JobId,
    kind: OperationKind,
    sources: Vec<PathBuf>,
    destination: PathBuf,
    cancelled: &AtomicBool,
    progress: &mut dyn FnMut(OperationPlanningProgress),
) -> Result<OperationPlan, String> {
    check_cancelled(cancelled)?;
    let sources = validate_source_set(sources)?;
    let destination_metadata = fs::symlink_metadata(&destination)
        .map_err(|error| format!("Cannot inspect destination: {error}"))?;
    reject_link_or_special(&destination, &destination_metadata)?;
    if !destination_metadata.is_dir() {
        return Err("The destination pane is not a directory".into());
    }
    let canonical_destination = fs::canonicalize(&destination)
        .map_err(|error| format!("Cannot resolve destination: {error}"))?;

    let mut roots = Vec::with_capacity(sources.len());
    let mut conflicts = Vec::new();
    let mut item_count = 0_u64;
    let mut file_count = 0_u64;
    let mut total_bytes = 0_u64;
    let mut all_same_volume = true;
    let mut affected = vec![destination.clone()];

    for source in &sources {
        check_cancelled(cancelled)?;
        let metadata = fs::symlink_metadata(source)
            .map_err(|error| format!("Cannot inspect source {}: {error}", source.display()))?;
        reject_mutation_root(source)?;
        reject_link_or_special(source, &metadata)?;
        let source_fingerprint = fingerprint(&metadata)?;
        let canonical_source = fs::canonicalize(source)
            .map_err(|error| format!("Cannot resolve source {}: {error}", source.display()))?;
        if source_fingerprint.kind == ObjectKind::Directory
            && canonical_destination.starts_with(&canonical_source)
        {
            return Err(format!(
                "Cannot {} a directory into itself or one of its descendants",
                kind.label()
            ));
        }
        let leaf = source
            .file_name()
            .ok_or_else(|| "A selected source has no file name".to_string())?;
        let requested_target = destination.join(leaf);
        let target = keep_both_target(&requested_target, source_fingerprint.kind)?;
        if target != requested_target {
            conflicts.push(ConflictSummary {
                source: source.clone(),
                requested_destination: requested_target,
                resolved_destination: target.clone(),
            });
        }

        let mut root = TransferRoot {
            source: source.clone(),
            target: target.clone(),
            source_fingerprint,
            directories: Vec::new(),
            files: Vec::new(),
        };
        collect_tree(
            job,
            kind,
            source,
            &target,
            &mut root.directories,
            &mut root.files,
            &mut item_count,
            &mut file_count,
            &mut total_bytes,
            cancelled,
            progress,
        )?;
        all_same_volume &= same_volume(source, &destination)?;
        if let Some(parent) = source.parent() {
            affected.push(parent.to_path_buf());
        }
        roots.push(root);
    }

    affected.sort();
    affected.dedup();
    let strategy = match kind {
        OperationKind::Move if all_same_volume => PlannedStrategy::AtomicRename,
        OperationKind::Move => PlannedStrategy::CopyVerifyRemove,
        OperationKind::Copy => PlannedStrategy::ParallelCopy,
        _ => unreachable!(),
    };
    let worker_count = if all_same_volume {
        1
    } else {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2)
            .clamp(1, 2)
    };
    let warnings = match strategy {
        PlannedStrategy::CopyVerifyRemove => vec![
            "Sources are removed only after every copied file passes SHA-256 verification".into(),
        ],
        PlannedStrategy::ParallelCopy => vec![
            "Existing destinations are never overwritten; conflicts receive a numbered name".into(),
        ],
        PlannedStrategy::AtomicRename => {
            vec!["The same-volume rename is a non-interruptible finalization step".into()]
        }
        _ => Vec::new(),
    };
    let action = if strategy == PlannedStrategy::AtomicRename {
        PlannedAction::AtomicMove { roots }
    } else {
        PlannedAction::Transfer {
            roots,
            remove_sources: kind == OperationKind::Move,
            worker_count,
        }
    };
    Ok(OperationPlan {
        summary: PlanSummary {
            job,
            kind,
            sources,
            destination: Some(destination),
            strategy,
            item_count,
            file_count,
            directory_count: item_count.saturating_sub(file_count),
            total_bytes,
            recursive_scope_known: true,
            conflicts,
            warnings,
        },
        action,
        affected_directories: affected,
    })
}

fn build_delete(
    job: JobId,
    sources: Vec<PathBuf>,
    permanent: bool,
    cancelled: &AtomicBool,
    progress: &mut dyn FnMut(OperationPlanningProgress),
) -> Result<OperationPlan, String> {
    check_cancelled(cancelled)?;
    let sources = validate_source_set(sources)?;
    let mut snapshots = Vec::with_capacity(sources.len());
    let mut delete_roots = Vec::with_capacity(sources.len());
    let mut item_count = 0_u64;
    let mut file_count = 0_u64;
    let mut total_bytes = 0_u64;
    let mut recursive_scope_known = true;
    let mut affected = Vec::new();

    for source in &sources {
        check_cancelled(cancelled)?;
        reject_mutation_root(source)?;
        let metadata = fs::symlink_metadata(source)
            .map_err(|error| format!("Cannot inspect source {}: {error}", source.display()))?;
        reject_link_or_special(source, &metadata)?;
        let source_fingerprint = fingerprint(&metadata)?;
        if permanent {
            let mut root = TransferRoot {
                source: source.clone(),
                target: source.clone(),
                source_fingerprint,
                directories: Vec::new(),
                files: Vec::new(),
            };
            collect_tree(
                job,
                OperationKind::PermanentDelete,
                source,
                source,
                &mut root.directories,
                &mut root.files,
                &mut item_count,
                &mut file_count,
                &mut total_bytes,
                cancelled,
                progress,
            )?;
            delete_roots.push(root);
        } else {
            item_count += 1;
            if source_fingerprint.kind == ObjectKind::File {
                file_count += 1;
                total_bytes = total_bytes
                    .checked_add(source_fingerprint.len)
                    .ok_or_else(|| "Operation byte count overflowed".to_string())?;
            } else {
                recursive_scope_known = false;
            }
            snapshots.push((source.clone(), source_fingerprint));
            progress(OperationPlanningProgress {
                job,
                kind: OperationKind::Recycle,
                discovered_items: item_count,
                discovered_files: file_count,
                discovered_directories: item_count.saturating_sub(file_count),
                discovered_bytes: total_bytes,
                current_path: Some(source.clone()),
            });
        }
        if let Some(parent) = source.parent() {
            affected.push(parent.to_path_buf());
        }
    }
    affected.sort();
    affected.dedup();

    let kind = if permanent {
        OperationKind::PermanentDelete
    } else {
        OperationKind::Recycle
    };
    let strategy = if permanent {
        PlannedStrategy::PermanentDelete
    } else {
        PlannedStrategy::RecycleBin
    };
    let warnings = if permanent {
        vec![
            "This operation cannot be undone and does not use the Recycle Bin / Trash".into(),
            "The reviewed file and folder manifest is removed entry by entry".into(),
        ]
    } else {
        let mut warnings = vec![
            "Recycle support depends on the operating system and source location".into(),
            "Failure never falls back to permanent deletion".into(),
        ];
        if !recursive_scope_known {
            warnings.push(
                "Directory contents are delegated to the operating system without pre-enumeration"
                    .into(),
            );
        }
        warnings
    };
    let action = if permanent {
        PlannedAction::PermanentDelete {
            roots: delete_roots,
        }
    } else {
        PlannedAction::Recycle {
            sources: snapshots
                .into_iter()
                .map(|(path, fingerprint)| (path, fingerprint, 1))
                .collect(),
        }
    };

    Ok(OperationPlan {
        summary: PlanSummary {
            job,
            kind,
            sources,
            destination: None,
            strategy,
            item_count,
            file_count,
            directory_count: item_count.saturating_sub(file_count),
            total_bytes,
            recursive_scope_known,
            conflicts: Vec::new(),
            warnings,
        },
        action,
        affected_directories: affected,
    })
}

fn build_rename(
    job: JobId,
    source: PathBuf,
    new_name: OsString,
    cancelled: &AtomicBool,
) -> Result<OperationPlan, String> {
    check_cancelled(cancelled)?;
    reject_mutation_root(&source)?;
    validate_leaf_name(&new_name)?;
    let metadata = fs::symlink_metadata(&source)
        .map_err(|error| format!("Cannot inspect source {}: {error}", source.display()))?;
    reject_link_or_special(&source, &metadata)?;
    let source_fingerprint = fingerprint(&metadata)?;
    let parent = source
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| "Cannot rename a filesystem root".to_string())?
        .to_path_buf();
    let target = parent.join(&new_name);
    if source == target {
        return Err("The new name is unchanged".into());
    }
    if target
        .try_exists()
        .map_err(|error| format!("Cannot inspect the requested name: {error}"))?
    {
        return Err("An item with that name already exists; overwrite is disabled".into());
    }
    Ok(OperationPlan {
        summary: PlanSummary {
            job,
            kind: OperationKind::Rename,
            sources: vec![source.clone()],
            destination: Some(target.clone()),
            strategy: PlannedStrategy::AtomicRename,
            item_count: 1,
            file_count: u64::from(source_fingerprint.kind == ObjectKind::File),
            directory_count: u64::from(source_fingerprint.kind == ObjectKind::Directory),
            total_bytes: source_fingerprint.len,
            recursive_scope_known: true,
            conflicts: Vec::new(),
            warnings: vec!["Existing destinations are never overwritten".into()],
        },
        action: PlannedAction::Rename {
            source,
            target,
            fingerprint: source_fingerprint,
        },
        affected_directories: vec![parent],
    })
}

fn build_mkdir(
    job: JobId,
    parent: PathBuf,
    name: OsString,
    cancelled: &AtomicBool,
) -> Result<OperationPlan, String> {
    check_cancelled(cancelled)?;
    validate_leaf_name(&name)?;
    let metadata = fs::symlink_metadata(&parent)
        .map_err(|error| format!("Cannot inspect parent directory: {error}"))?;
    reject_link_or_special(&parent, &metadata)?;
    if !metadata.is_dir() {
        return Err("The active pane is not a directory".into());
    }
    let target = parent.join(&name);
    if target
        .try_exists()
        .map_err(|error| format!("Cannot inspect the requested folder name: {error}"))?
    {
        return Err("An item with that name already exists".into());
    }
    Ok(OperationPlan {
        summary: PlanSummary {
            job,
            kind: OperationKind::CreateDirectory,
            sources: Vec::new(),
            destination: Some(target.clone()),
            strategy: PlannedStrategy::ExclusiveCreate,
            item_count: 1,
            file_count: 0,
            directory_count: 1,
            total_bytes: 0,
            recursive_scope_known: true,
            conflicts: Vec::new(),
            warnings: Vec::new(),
        },
        action: PlannedAction::CreateDirectory { target },
        affected_directories: vec![parent],
    })
}

fn validate_source_set(mut sources: Vec<PathBuf>) -> Result<Vec<PathBuf>, String> {
    if sources.is_empty() {
        return Err("Select or focus at least one file or directory".into());
    }
    if sources.len() > MAX_SOURCES {
        return Err(format!(
            "A job can contain at most {MAX_SOURCES} top-level selections"
        ));
    }
    sources.sort();
    sources.dedup();
    for (index, source) in sources.iter().enumerate() {
        for other in sources.iter().skip(index + 1) {
            if other.starts_with(source) {
                return Err(format!(
                    "Overlapping selections are not allowed: {} contains {}",
                    source.display(),
                    other.display()
                ));
            }
        }
    }
    Ok(sources)
}

#[allow(clippy::too_many_arguments)]
fn collect_tree(
    job: JobId,
    kind: OperationKind,
    source: &Path,
    target: &Path,
    directories: &mut Vec<DirectoryTask>,
    files: &mut Vec<FileTask>,
    item_count: &mut u64,
    file_count: &mut u64,
    total_bytes: &mut u64,
    cancelled: &AtomicBool,
    progress: &mut dyn FnMut(OperationPlanningProgress),
) -> Result<(), String> {
    check_cancelled(cancelled)?;
    if directories.len() + files.len() >= MAX_PLAN_ITEMS {
        return Err(format!(
            "The operation exceeds the current {MAX_PLAN_ITEMS}-item safety limit"
        ));
    }
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("Cannot inspect {}: {error}", source.display()))?;
    reject_link_or_special(source, &metadata)?;
    let item_fingerprint = fingerprint(&metadata)?;
    *item_count = item_count
        .checked_add(1)
        .ok_or_else(|| "Operation item count overflowed".to_string())?;

    match item_fingerprint.kind {
        ObjectKind::File => {
            *file_count = file_count
                .checked_add(1)
                .ok_or_else(|| "Operation file count overflowed".to_string())?;
            *total_bytes = total_bytes
                .checked_add(item_fingerprint.len)
                .ok_or_else(|| "Operation byte count overflowed".to_string())?;
            files.push(FileTask {
                source: source.to_path_buf(),
                target: target.to_path_buf(),
                fingerprint: item_fingerprint,
            });
        }
        ObjectKind::Directory => {
            directories.push(DirectoryTask {
                source: source.to_path_buf(),
                target: target.to_path_buf(),
                fingerprint: item_fingerprint,
            });
            let reader = fs::read_dir(source)
                .map_err(|error| format!("Cannot enumerate {}: {error}", source.display()))?;
            for child in reader {
                let child = child.map_err(|error| {
                    format!(
                        "Directory enumeration failed in {}: {error}",
                        source.display()
                    )
                })?;
                collect_tree(
                    job,
                    kind,
                    &child.path(),
                    &target.join(child.file_name()),
                    directories,
                    files,
                    item_count,
                    file_count,
                    total_bytes,
                    cancelled,
                    progress,
                )?;
            }
        }
    }
    progress(OperationPlanningProgress {
        job,
        kind,
        discovered_items: *item_count,
        discovered_files: *file_count,
        discovered_directories: item_count.saturating_sub(*file_count),
        discovered_bytes: *total_bytes,
        current_path: Some(source.to_path_buf()),
    });
    Ok(())
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        Err("Planning cancelled; no files changed".into())
    } else {
        Ok(())
    }
}

pub(crate) fn fingerprint(metadata: &Metadata) -> Result<Fingerprint, String> {
    let kind = if metadata.is_file() {
        ObjectKind::File
    } else if metadata.is_dir() {
        ObjectKind::Directory
    } else {
        return Err("Unsupported filesystem object type".into());
    };
    Ok(Fingerprint {
        kind,
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

pub(crate) fn revalidate(path: &Path, expected: &Fingerprint) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot revalidate {}: {error}", path.display()))?;
    reject_link_or_special(path, &metadata)?;
    let actual = fingerprint(&metadata)?;
    if actual.kind != expected.kind
        || actual.len != expected.len
        || (expected.modified.is_some() && actual.modified != expected.modified)
    {
        return Err(format!("Source changed after review: {}", path.display()));
    }
    Ok(())
}

pub(crate) fn revalidate_directory_kind(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot revalidate {}: {error}", path.display()))?;
    reject_link_or_special(path, &metadata)?;
    if !metadata.is_dir() {
        return Err(format!(
            "Directory identity changed after review: {}",
            path.display()
        ));
    }
    Ok(())
}

fn reject_mutation_root(path: &Path) -> Result<(), String> {
    if path.parent().is_none() {
        return Err("Filesystem roots cannot be operation sources".into());
    }
    Ok(())
}

fn reject_link_or_special(path: &Path, metadata: &Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() || is_windows_reparse_point(metadata) {
        return Err(format!(
            "Links, junctions, and reparse points are not supported yet: {}",
            path.display()
        ));
    }
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(format!("Unsupported filesystem object: {}", path.display()));
    }
    Ok(())
}

#[cfg(windows)]
fn is_windows_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_windows_reparse_point(_metadata: &Metadata) -> bool {
    false
}

fn keep_both_target(requested: &Path, kind: ObjectKind) -> Result<PathBuf, String> {
    if !requested
        .try_exists()
        .map_err(|error| format!("Cannot inspect destination: {error}"))?
    {
        return Ok(requested.to_path_buf());
    }
    let file_name = requested
        .file_name()
        .ok_or_else(|| "Destination has no file name".to_string())?;
    let parent = requested
        .parent()
        .ok_or_else(|| "Destination has no parent directory".to_string())?;
    for number in 2..=10_000 {
        let candidate_name = suffixed_name(file_name, requested.extension(), kind, number);
        let candidate = parent.join(candidate_name);
        if !candidate
            .try_exists()
            .map_err(|error| format!("Cannot inspect destination: {error}"))?
        {
            return Ok(candidate);
        }
    }
    Err("Could not allocate a conflict-free destination name".into())
}

fn suffixed_name(
    file_name: &OsStr,
    extension: Option<&OsStr>,
    kind: ObjectKind,
    number: u32,
) -> OsString {
    let mut name = if kind == ObjectKind::File {
        Path::new(file_name)
            .file_stem()
            .unwrap_or(file_name)
            .to_os_string()
    } else {
        file_name.to_os_string()
    };
    name.push(format!(" ({number})"));
    if kind == ObjectKind::File
        && let Some(extension) = extension
    {
        name.push(".");
        name.push(extension);
    }
    name
}

fn validate_leaf_name(name: &OsStr) -> Result<(), String> {
    if name.is_empty() {
        return Err("A name is required".into());
    }
    let path = Path::new(name);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err("Enter one file or folder name, without a path".into());
    }

    #[cfg(windows)]
    validate_windows_leaf_name(name)?;
    Ok(())
}

#[cfg(windows)]
fn validate_windows_leaf_name(name: &OsStr) -> Result<(), String> {
    let value = name.to_string_lossy();
    if value.ends_with(['.', ' '])
        || value
            .chars()
            .any(|character| "<>:\"/\\|?*".contains(character))
    {
        return Err("The name contains characters Windows does not allow".into());
    }
    let stem = value
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            });
    if reserved {
        return Err("That name is reserved by Windows".into());
    }
    Ok(())
}

#[cfg(windows)]
fn same_volume(source: &Path, destination: &Path) -> Result<bool, String> {
    use std::path::Prefix;
    let prefix = |path: &Path| match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                Some((letter as char).to_ascii_uppercase().to_string())
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => Some(format!(
                "{}\\{}",
                server.to_string_lossy().to_ascii_lowercase(),
                share.to_string_lossy().to_ascii_lowercase()
            )),
            _ => None,
        },
        _ => None,
    };
    Ok(prefix(source).is_some() && prefix(source) == prefix(destination))
}

#[cfg(unix)]
fn same_volume(_source: &Path, _destination: &Path) -> Result<bool, String> {
    // The current atomic directory-move implementation uses Windows APIs.
    // Other platforms take the verified copy/remove path until renameat2-style
    // no-replace publication is implemented for both files and directories.
    Ok(false)
}

#[cfg(not(any(windows, unix)))]
fn same_volume(_source: &Path, _destination: &Path) -> Result<bool, String> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_preserves_file_extension() {
        assert_eq!(
            suffixed_name(
                OsStr::new("report.csv"),
                Some(OsStr::new("csv")),
                ObjectKind::File,
                2
            ),
            OsString::from("report (2).csv")
        );
        assert_eq!(
            suffixed_name(
                OsStr::new("archive.v1"),
                Some(OsStr::new("v1")),
                ObjectKind::Directory,
                3
            ),
            OsString::from("archive.v1 (3)")
        );
    }

    #[test]
    fn rejects_paths_where_a_leaf_name_is_required() {
        assert!(validate_leaf_name(OsStr::new("")).is_err());
        assert!(validate_leaf_name(OsStr::new("..")).is_err());
        assert!(validate_leaf_name(OsStr::new("folder/name")).is_err());
        assert!(validate_leaf_name(OsStr::new("good-name.txt")).is_ok());
    }

    #[test]
    fn overlapping_sources_are_rejected_without_filesystem_access() {
        let result = validate_source_set(vec![
            PathBuf::from("root"),
            PathBuf::from("root").join("child"),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn duplicate_sources_are_collapsed_deterministically() {
        let result = validate_source_set(vec![
            PathBuf::from("b"),
            PathBuf::from("a"),
            PathBuf::from("a"),
        ])
        .unwrap();
        assert_eq!(result, vec![PathBuf::from("a"), PathBuf::from("b")]);
    }

    #[test]
    fn cancelled_planning_stops_before_filesystem_access() {
        let cancelled = AtomicBool::new(true);
        let intent = OperationIntent::Copy {
            sources: vec![PathBuf::from("missing-source")],
            destination: PathBuf::from("missing-destination"),
        };

        let error = build_plan(JobId(1), intent, &cancelled).unwrap_err();
        assert_eq!(error, "Planning cancelled; no files changed");
    }

    #[test]
    fn permanent_delete_freezes_recursive_scope_while_recycle_stays_root_scoped() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("folder");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("child.txt"), b"contents").unwrap();
        let cancelled = AtomicBool::new(false);

        let recycle = build_plan(
            JobId(2),
            OperationIntent::Recycle {
                sources: vec![root.clone()],
            },
            &cancelled,
        )
        .unwrap();
        assert_eq!(recycle.summary.item_count, 1);
        assert!(!recycle.summary.recursive_scope_known);
        assert!(matches!(recycle.action, PlannedAction::Recycle { .. }));

        let permanent = build_plan(
            JobId(3),
            OperationIntent::PermanentDelete {
                sources: vec![root],
            },
            &cancelled,
        )
        .unwrap();
        assert_eq!(permanent.summary.item_count, 2);
        assert_eq!(permanent.summary.file_count, 1);
        assert_eq!(permanent.summary.directory_count, 1);
        assert_eq!(permanent.summary.total_bytes, 8);
        assert!(permanent.summary.recursive_scope_known);
        assert!(matches!(
            permanent.action,
            PlannedAction::PermanentDelete { .. }
        ));
    }

    #[cfg(windows)]
    #[test]
    fn rejects_reserved_windows_names() {
        assert!(validate_leaf_name(OsStr::new("CON.txt")).is_err());
        assert!(validate_leaf_name(OsStr::new("report?.txt")).is_err());
        assert!(validate_leaf_name(OsStr::new("normal.txt")).is_ok());
    }
}
