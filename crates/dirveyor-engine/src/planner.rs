use dirveyor_domain::{
    JobId, OperationIntent, OperationKind, OperationPlanningProgress, PlanSummary, PlannedStrategy,
};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

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
pub(crate) struct TransferRoot {
    pub source: PathBuf,
    pub target: PathBuf,
    pub source_fingerprint: Fingerprint,
}

#[derive(Debug)]
pub(crate) enum PlannedAction {
    Transfer {
        roots: Vec<TransferRoot>,
        remove_sources: bool,
        worker_count: usize,
        verification: dirveyor_domain::VerificationMode,
    },
    AtomicMove {
        roots: Vec<TransferRoot>,
    },
    Recycle {
        sources: Vec<(PathBuf, Fingerprint, u64)>,
    },
    PermanentDelete {
        manifest: File,
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

#[derive(Debug)]
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
            verification,
        } => build_transfer(
            job,
            OperationKind::Copy,
            sources,
            destination,
            cancelled,
            verification,
            progress,
        ),
        OperationIntent::Move {
            sources,
            destination,
            verification,
        } => build_transfer(
            job,
            OperationKind::Move,
            sources,
            destination,
            cancelled,
            verification,
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
    verification: dirveyor_domain::VerificationMode,
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
    let conflicts = Vec::new();
    let mut all_same_volume = true;
    let mut every_target_clear = true;
    let mut affected = vec![destination.clone()];
    let mut target_keys = HashSet::with_capacity(sources.len());

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
        if !target_keys.insert(target_identity_key(&requested_target)) {
            return Err(format!(
                "Multiple selected sources would use the same destination name: {}",
                requested_target.display()
            ));
        }
        let target_exists = requested_target
            .try_exists()
            .map_err(|error| format!("Cannot inspect destination: {error}"))?;
        if target_exists {
            let canonical_target = fs::canonicalize(&requested_target).map_err(|error| {
                format!(
                    "Cannot resolve destination item {}: {error}",
                    requested_target.display()
                )
            })?;
            if canonical_target == canonical_source {
                return Err("Source and destination resolve to the same item".into());
            }
        }
        every_target_clear &= !target_exists;
        let target = requested_target;

        let root = TransferRoot {
            source: source.clone(),
            target: target.clone(),
            source_fingerprint,
        };
        all_same_volume &= same_volume(source, &destination)?;
        if let Some(parent) = source.parent() {
            affected.push(parent.to_path_buf());
        }
        roots.push(root);
    }

    let (item_count, file_count, total_bytes) =
        count_transfer_scope(job, kind, &roots, cancelled, progress)?;

    affected.sort();
    affected.dedup();
    let strategy = match kind {
        OperationKind::Move if all_same_volume && every_target_clear => {
            PlannedStrategy::AtomicRename
        }
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
            format!(
                "Sources remain until the streamed copy completes using {} verification",
                verification.label()
            ),
            format!("A bounded queue feeds {worker_count} concurrent file worker(s)"),
        ],
        PlannedStrategy::ParallelCopy => vec![
            "Directory totals are counted without storing a file manifest; colliding files pause for a choice".into(),
            format!("A bounded queue feeds {worker_count} concurrent file worker(s)"),
        ],
        PlannedStrategy::AtomicRename => {
            vec!["The same-volume rename is a non-interruptible finalization step".into()]
        }
        _ => Vec::new(),
    };
    let recursive_scope_known = true;
    let action = if strategy == PlannedStrategy::AtomicRename {
        PlannedAction::AtomicMove { roots }
    } else {
        PlannedAction::Transfer {
            roots,
            remove_sources: kind == OperationKind::Move,
            worker_count,
            verification,
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
            recursive_scope_known,
            conflicts,
            warnings,
            verification: Some(verification),
        },
        action,
        affected_directories: affected,
    })
}

fn count_transfer_scope(
    job: JobId,
    kind: OperationKind,
    roots: &[TransferRoot],
    cancelled: &AtomicBool,
    progress: &mut dyn FnMut(OperationPlanningProgress),
) -> Result<(u64, u64, u64), String> {
    let mut item_count = 0_u64;
    let mut file_count = 0_u64;
    let mut total_bytes = 0_u64;
    for root in roots {
        count_transfer_tree(
            job,
            kind,
            &root.source,
            Some(&root.source_fingerprint),
            &mut item_count,
            &mut file_count,
            &mut total_bytes,
            cancelled,
            progress,
        )?;
        publish_transfer_count(
            job,
            kind,
            &root.source,
            item_count,
            file_count,
            total_bytes,
            progress,
        );
    }
    Ok((item_count, file_count, total_bytes))
}

#[allow(clippy::too_many_arguments)]
fn count_transfer_tree(
    job: JobId,
    kind: OperationKind,
    source: &Path,
    known_fingerprint: Option<&Fingerprint>,
    item_count: &mut u64,
    file_count: &mut u64,
    total_bytes: &mut u64,
    cancelled: &AtomicBool,
    progress: &mut dyn FnMut(OperationPlanningProgress),
) -> Result<(), String> {
    check_cancelled(cancelled)?;
    let owned_fingerprint;
    let item_fingerprint = if let Some(fingerprint) = known_fingerprint {
        fingerprint
    } else {
        let metadata = fs::symlink_metadata(source)
            .map_err(|error| format!("Cannot inspect {}: {error}", source.display()))?;
        reject_link_or_special(source, &metadata)?;
        owned_fingerprint = fingerprint(&metadata)?;
        &owned_fingerprint
    };
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
        }
        ObjectKind::Directory => {
            let reader = fs::read_dir(source)
                .map_err(|error| format!("Cannot enumerate {}: {error}", source.display()))?;
            for child in reader {
                let child = child.map_err(|error| {
                    format!(
                        "Directory enumeration failed in {}: {error}",
                        source.display()
                    )
                })?;
                let child_path = child.path();
                let metadata = child
                    .metadata()
                    .map_err(|error| format!("Cannot inspect {}: {error}", child_path.display()))?;
                reject_link_or_special(&child_path, &metadata)?;
                let child_fingerprint = fingerprint(&metadata)?;
                count_transfer_tree(
                    job,
                    kind,
                    &child_path,
                    Some(&child_fingerprint),
                    item_count,
                    file_count,
                    total_bytes,
                    cancelled,
                    progress,
                )?;
            }
        }
    }
    if *item_count == 1 || (*item_count).is_multiple_of(128) {
        publish_transfer_count(
            job,
            kind,
            source,
            *item_count,
            *file_count,
            *total_bytes,
            progress,
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn publish_transfer_count(
    job: JobId,
    kind: OperationKind,
    current_path: &Path,
    item_count: u64,
    file_count: u64,
    total_bytes: u64,
    progress: &mut dyn FnMut(OperationPlanningProgress),
) {
    progress(OperationPlanningProgress {
        job,
        kind,
        discovered_items: item_count,
        discovered_files: file_count,
        discovered_directories: item_count.saturating_sub(file_count),
        discovered_bytes: total_bytes,
        current_path: Some(current_path.to_path_buf()),
    });
}

#[cfg(windows)]
fn target_identity_key(path: &Path) -> OsString {
    path.to_string_lossy().to_lowercase().into()
}

#[cfg(not(windows))]
fn target_identity_key(path: &Path) -> OsString {
    path.as_os_str().to_os_string()
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
    let mut delete_manifest = permanent
        .then(|| tempfile::tempfile().map(BufWriter::new))
        .transpose()
        .map_err(|error| format!("Cannot create temporary delete manifest: {error}"))?;
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
            collect_delete_manifest(
                job,
                OperationKind::PermanentDelete,
                source,
                Some(&source_fingerprint),
                delete_manifest
                    .as_mut()
                    .expect("permanent delete manifest should exist"),
                &mut item_count,
                &mut file_count,
                &mut total_bytes,
                cancelled,
                progress,
            )?;
            publish_transfer_count(
                job,
                OperationKind::PermanentDelete,
                source,
                item_count,
                file_count,
                total_bytes,
                progress,
            );
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
    let delete_manifest = delete_manifest
        .map(|mut manifest| {
            manifest
                .flush()
                .map_err(|error| format!("Cannot flush temporary delete manifest: {error}"))?;
            manifest.into_inner().map_err(|error| {
                format!("Cannot finish temporary delete manifest: {}", error.error())
            })
        })
        .transpose()?;

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
            "The reviewed on-disk file and folder manifest is removed entry by entry".into(),
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
            manifest: delete_manifest.expect("permanent delete manifest should exist"),
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
            verification: None,
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
            verification: None,
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
            verification: None,
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
fn collect_delete_manifest(
    job: JobId,
    kind: OperationKind,
    source: &Path,
    known_fingerprint: Option<&Fingerprint>,
    manifest: &mut BufWriter<File>,
    item_count: &mut u64,
    file_count: &mut u64,
    total_bytes: &mut u64,
    cancelled: &AtomicBool,
    progress: &mut dyn FnMut(OperationPlanningProgress),
) -> Result<(), String> {
    check_cancelled(cancelled)?;
    let owned_fingerprint;
    let item_fingerprint = if let Some(fingerprint) = known_fingerprint {
        fingerprint
    } else {
        let metadata = fs::symlink_metadata(source)
            .map_err(|error| format!("Cannot inspect {}: {error}", source.display()))?;
        reject_link_or_special(source, &metadata)?;
        owned_fingerprint = fingerprint(&metadata)?;
        &owned_fingerprint
    };
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
        }
        ObjectKind::Directory => {
            let reader = fs::read_dir(source)
                .map_err(|error| format!("Cannot enumerate {}: {error}", source.display()))?;
            for child in reader {
                let child = child.map_err(|error| {
                    format!(
                        "Directory enumeration failed in {}: {error}",
                        source.display()
                    )
                })?;
                let child_path = child.path();
                let metadata = child
                    .metadata()
                    .map_err(|error| format!("Cannot inspect {}: {error}", child_path.display()))?;
                reject_link_or_special(&child_path, &metadata)?;
                let child_fingerprint = fingerprint(&metadata)?;
                collect_delete_manifest(
                    job,
                    kind,
                    &child_path,
                    Some(&child_fingerprint),
                    manifest,
                    item_count,
                    file_count,
                    total_bytes,
                    cancelled,
                    progress,
                )?;
            }
        }
    }
    write_delete_manifest_entry(manifest, source, item_fingerprint)
        .map_err(|error| format!("Cannot write temporary delete manifest: {error}"))?;
    if *item_count == 1 || (*item_count).is_multiple_of(128) {
        publish_transfer_count(
            job,
            kind,
            source,
            *item_count,
            *file_count,
            *total_bytes,
            progress,
        );
    }
    Ok(())
}

fn write_delete_manifest_entry(
    file: &mut impl Write,
    path: &Path,
    fingerprint: &Fingerprint,
) -> io::Result<()> {
    let encoded = encode_manifest_path(path);
    let length = u32::try_from(encoded.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Path is too long"))?;
    file.write_all(&length.to_le_bytes())?;
    file.write_all(&encoded)?;
    file.write_all(&[match fingerprint.kind {
        ObjectKind::File => 0,
        ObjectKind::Directory => 1,
    }])?;
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

pub(crate) fn read_delete_manifest_entry(
    file: &mut File,
) -> io::Result<Option<(PathBuf, Fingerprint)>> {
    let mut length = [0_u8; 4];
    match file.read_exact(&mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut encoded = vec![0_u8; u32::from_le_bytes(length) as usize];
    file.read_exact(&mut encoded)?;
    let mut kind = [0_u8; 1];
    file.read_exact(&mut kind)?;
    let kind = match kind[0] {
        0 => ObjectKind::File,
        1 => ObjectKind::Directory,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid object kind",
            ));
        }
    };
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
        decode_manifest_path(encoded),
        Fingerprint {
            kind,
            len: u64::from_le_bytes(len),
            modified,
        },
    )))
}

#[cfg(windows)]
fn encode_manifest_path(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(windows)]
fn decode_manifest_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    std::ffi::OsString::from_wide(&units).into()
}

#[cfg(unix)]
fn encode_manifest_path(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(unix)]
fn decode_manifest_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes).into()
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

pub(crate) fn keep_both_target(
    requested: &Path,
    kind: ObjectKind,
    reserved: &std::collections::HashSet<PathBuf>,
) -> Result<PathBuf, String> {
    if !requested
        .try_exists()
        .map_err(|error| format!("Cannot inspect destination: {error}"))?
        && !reserved.contains(requested)
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
            && !reserved.contains(&candidate)
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
    use std::io::Seek;

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
            verification: dirveyor_domain::VerificationMode::Full,
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

    #[test]
    fn nested_manifest_preserves_child_fingerprints_and_postorder() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let child = nested.join("child.txt");
        fs::write(&child, b"original").unwrap();
        let plan = build_plan(
            JobId(77),
            OperationIntent::PermanentDelete {
                sources: vec![root.clone()],
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(plan.summary.item_count, 3);
        assert_eq!(plan.summary.total_bytes, 8);
        let PlannedAction::PermanentDelete { mut manifest } = plan.action else {
            panic!("expected delete manifest");
        };
        std::io::Seek::rewind(&mut manifest).unwrap();
        let (path, fingerprint) = read_delete_manifest_entry(&mut manifest).unwrap().unwrap();
        assert_eq!(path, child);
        revalidate(&child, &fingerprint).unwrap();
        assert_eq!(
            read_delete_manifest_entry(&mut manifest)
                .unwrap()
                .unwrap()
                .0,
            nested
        );
        assert_eq!(
            read_delete_manifest_entry(&mut manifest)
                .unwrap()
                .unwrap()
                .0,
            root
        );
        assert!(read_delete_manifest_entry(&mut manifest).unwrap().is_none());
        fs::write(&child, b"changed length").unwrap();
        assert!(revalidate(&child, &fingerprint).is_err());
    }

    #[test]
    fn transfer_plan_counts_directory_contents_without_storing_a_manifest() {
        let source_parent = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let root = source_parent.path().join("large-tree");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("child.txt"), b"not planned individually").unwrap();
        let cancelled = AtomicBool::new(false);

        let mut last_progress = None;
        let plan = build_plan_with_progress(
            JobId(4),
            OperationIntent::Copy {
                sources: vec![root],
                destination: destination.path().to_path_buf(),
                verification: dirveyor_domain::VerificationMode::Full,
            },
            &cancelled,
            &mut |update| last_progress = Some(update),
        )
        .unwrap();

        assert_eq!(plan.summary.item_count, 2);
        assert_eq!(plan.summary.file_count, 1);
        assert_eq!(plan.summary.directory_count, 1);
        assert_eq!(plan.summary.total_bytes, 24);
        assert!(plan.summary.recursive_scope_known);
        let progress = last_progress.expect("count scan should publish progress");
        assert_eq!(progress.discovered_items, 2);
        assert_eq!(progress.discovered_files, 1);
        assert_eq!(progress.discovered_directories, 1);
        assert_eq!(progress.discovered_bytes, 24);
        let PlannedAction::Transfer { roots, .. } = plan.action else {
            panic!("expected a streaming transfer");
        };
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn transfer_rejects_source_and_destination_identity() {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("same.txt");
        fs::write(&source, b"keep me").unwrap();
        let cancelled = AtomicBool::new(false);

        let error = build_plan(
            JobId(5),
            OperationIntent::Move {
                sources: vec![source],
                destination: parent.path().to_path_buf(),
                verification: dirveyor_domain::VerificationMode::Full,
            },
            &cancelled,
        )
        .unwrap_err();

        assert_eq!(error, "Source and destination resolve to the same item");
    }

    #[test]
    fn delete_manifest_is_not_limited_to_one_hundred_thousand_entries() {
        let mut manifest = tempfile::tempfile().unwrap();
        let fingerprint = Fingerprint {
            kind: ObjectKind::File,
            len: 1,
            modified: None,
        };
        for number in 0..=100_000_u32 {
            write_delete_manifest_entry(
                &mut manifest,
                &PathBuf::from(format!("entry-{number}")),
                &fingerprint,
            )
            .unwrap();
        }
        manifest.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut entries = 0_u32;
        while read_delete_manifest_entry(&mut manifest).unwrap().is_some() {
            entries += 1;
        }
        assert_eq!(entries, 100_001);
    }

    #[test]
    fn transfer_rejects_sources_with_the_same_destination_name() {
        let first_parent = tempfile::tempdir().unwrap();
        let second_parent = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let first = first_parent.path().join("shared.txt");
        let second = second_parent.path().join("shared.txt");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();
        let cancelled = AtomicBool::new(false);

        let error = build_plan(
            JobId(6),
            OperationIntent::Copy {
                sources: vec![first, second],
                destination: destination.path().to_path_buf(),
                verification: dirveyor_domain::VerificationMode::Full,
            },
            &cancelled,
        )
        .unwrap_err();

        assert!(error.starts_with("Multiple selected sources would use the same destination name"));
    }

    #[cfg(windows)]
    #[test]
    fn rejects_reserved_windows_names() {
        assert!(validate_leaf_name(OsStr::new("CON.txt")).is_err());
        assert!(validate_leaf_name(OsStr::new("report?.txt")).is_err());
        assert!(validate_leaf_name(OsStr::new("normal.txt")).is_ok());
    }
}
