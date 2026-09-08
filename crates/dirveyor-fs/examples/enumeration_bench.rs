//! Run with `cargo run --release -p dirveyor-fs --example enumeration_bench -- [directory]`.
//! Optional directory scanning is read-only. Synthetic timings exclude filesystem I/O.
use dirveyor_domain::{EntryKind, FileEntry, PaneState};
use dirveyor_fs::{sanitize_display_name, scan_directory};
use std::{hint::black_box, path::PathBuf, time::Instant};

fn main() {
    let names: Vec<String> = (0..50_000)
        .map(|i| format!("Sample_document_{:05}.txt", (i * 7919) % 50_000))
        .collect();
    let start = Instant::now();
    for name in &names {
        black_box(sanitize_display_name(black_box(name)));
    }
    println!("sanitize 50,000 names: {:?}", start.elapsed());
    let entries = names
        .iter()
        .map(|name| FileEntry {
            path: PathBuf::from(name),
            display_name: name.clone(),
            kind: EntryKind::File,
            size: Some(123),
            modified: None,
            metadata_incomplete: false,
            drive_info: None,
        })
        .collect();
    let mut pane = PaneState::new(PathBuf::from("benchmark"));
    let start = Instant::now();
    pane.apply_entries(0, entries);
    println!("prepare 50,000-entry listing: {:?}", start.elapsed());
    pane.set_filter("document".into());
    let start = Instant::now();
    for _ in 0..100 {
        black_box(black_box(&pane).focused());
        black_box(black_box(&pane).visible_len());
        black_box(black_box(&pane).visible_indices());
    }
    println!("100 filtered focus/count/view reads: {:?}", start.elapsed());
    if let Some(path) = std::env::args_os().nth(1) {
        let path = PathBuf::from(path);
        for pass in 1..=3 {
            let start = Instant::now();
            let listing = scan_directory(&path).expect("directory scan failed");
            println!(
                "scan pass {pass}: {:?}, {} rows, truncated={}",
                start.elapsed(),
                listing.entries.len(),
                listing.truncated
            );
            black_box(listing);
        }
        for pass in 1..=3 {
            let start = Instant::now();
            let queried = metadata_walk(&path, true).expect("metadata walk failed");
            let queried_time = start.elapsed();
            let start = Instant::now();
            let reused = metadata_walk(&path, false).expect("metadata walk failed");
            let reused_time = start.elapsed();
            assert_eq!(queried, reused, "tree changed between benchmark passes");
            println!(
                "metadata walk pass {pass}: path queries {queried_time:?}, entry metadata {reused_time:?}, {} files, {} directories, {} bytes",
                reused.0, reused.1, reused.2
            );
        }
    }
}

// Compare the old per-child path query with entry metadata on the same tree.
// Skips links, reparse points and special objects in both modes. Run against a
// stable tree. The second walk benefits from the first walk's filesystem cache.
fn metadata_walk(root: &std::path::Path, query_paths: bool) -> std::io::Result<(u64, u64, u64)> {
    fn reparse(metadata: &std::fs::Metadata) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & 0x400 != 0
        }
        #[cfg(not(windows))]
        {
            let _ = metadata;
            false
        }
    }
    let metadata = std::fs::symlink_metadata(root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || reparse(&metadata) {
        return Err(std::io::Error::other(
            "benchmark root must be an ordinary directory",
        ));
    }
    let mut pending = vec![root.to_path_buf()];
    let (mut files, mut directories, mut bytes) = (0, 0, 0);
    while let Some(directory) = pending.pop() {
        directories += 1;
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = if query_paths {
                std::fs::symlink_metadata(entry.path())?
            } else {
                entry.metadata()?
            };
            if metadata.file_type().is_symlink() || reparse(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                files += 1;
                bytes += metadata.len();
            }
        }
    }
    Ok((files, directories, bytes))
}
