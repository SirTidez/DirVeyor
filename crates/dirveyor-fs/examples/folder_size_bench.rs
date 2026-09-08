//! Read-only end-to-end folder size timing, including focus settling.
use dirveyor_domain::PaneId;
use dirveyor_fs::{FolderSizeScanner, FolderSizeUpdate};
use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

fn main() {
    let path = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .expect("pass a directory to measure"),
    );
    let scanner = FolderSizeScanner::with_worker_count(
        std::env::args()
            .nth(2)
            .map(|value| value.parse().expect("worker count"))
            .unwrap_or(4),
    );
    let start = Instant::now();
    let request_id = scanner.request(PaneId::Left, 0, path);
    loop {
        match scanner.try_recv() {
            Ok(event) if event.request_id == request_id => match event.update {
                FolderSizeUpdate::Progress(progress) => eprintln!(
                    "{:.2}s: {} files, {} directories",
                    start.elapsed().as_secs_f64(),
                    progress.file_count,
                    progress.directory_count
                ),
                FolderSizeUpdate::Finished(result) => {
                    let result = result.expect("folder measurement failed");
                    println!(
                        "elapsed={:.6}s files={} directories={} bytes={} skipped={}",
                        start.elapsed().as_secs_f64(),
                        result.file_count,
                        result.directory_count,
                        result.total_bytes,
                        result.skipped_items
                    );
                    break;
                }
            },
            Ok(_) => {}
            Err(std::sync::mpsc::TryRecvError::Empty) => thread::sleep(Duration::from_millis(1)),
            Err(error) => panic!("scanner disconnected: {error}"),
        }
    }
}
