//! Pure state used by the FileAdmin UI and filesystem adapters.

mod operation;
mod preview;

pub use operation::*;
pub use preview::*;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Stable identifier for one of the two browser panes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaneId {
    Left,
    Right,
}

impl PaneId {
    pub const ALL: [Self; 2] = [Self::Left, Self::Right];

    pub const fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }

    pub const fn other(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    Parent,
    Home,
    Favorite,
    Drive,
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Optical,
    RamDisk,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriveInfo {
    pub kind: DriveKind,
    pub label: Option<String>,
    pub filesystem: Option<String>,
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
}

impl DriveInfo {
    pub fn used_bytes(&self) -> Option<u64> {
        Some(self.total_bytes?.saturating_sub(self.available_bytes?))
    }

    pub fn used_percent(&self) -> Option<u8> {
        let total = self.total_bytes?;
        if total == 0 {
            return None;
        }
        let used = self.used_bytes()?;
        Some(((used as u128 * 100) / total as u128).min(100) as u8)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub display_name: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub metadata_incomplete: bool,
    pub drive_info: Option<DriveInfo>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderSizeSummary {
    pub request_id: u64,
    pub pane: PaneId,
    pub generation: u64,
    pub path: PathBuf,
    pub total_bytes: u64,
    pub file_count: u64,
    pub directory_count: u64,
    pub skipped_items: u64,
    pub drive_total_bytes: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderSizeProgress {
    pub request_id: u64,
    pub pane: PaneId,
    pub generation: u64,
    pub path: PathBuf,
    pub discovered_bytes: u64,
    pub file_count: u64,
    pub directory_count: u64,
    pub skipped_items: u64,
    pub drive_total_bytes: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FolderSizeState {
    Idle,
    Loading(FolderSizeProgress),
    Ready(FolderSizeSummary),
    Failed {
        request_id: u64,
        pane: PaneId,
        generation: u64,
        path: PathBuf,
        message: String,
    },
}

impl FolderSizeState {
    pub fn matches(&self, pane: PaneId, generation: u64, path: &Path) -> bool {
        match self {
            Self::Idle => false,
            Self::Loading(progress) => {
                progress.pane == pane && progress.generation == generation && progress.path == path
            }
            Self::Failed {
                pane: state_pane,
                generation: state_generation,
                path: state_path,
                ..
            } => *state_pane == pane && *state_generation == generation && state_path == path,
            Self::Ready(summary) => {
                summary.pane == pane && summary.generation == generation && summary.path == path
            }
        }
    }
}

impl FileEntry {
    pub fn is_directory(&self) -> bool {
        matches!(
            self.kind,
            EntryKind::Parent
                | EntryKind::Home
                | EntryKind::Favorite
                | EntryKind::Drive
                | EntryKind::Directory
        )
    }

    pub fn is_parent(&self) -> bool {
        self.kind == EntryKind::Parent
    }

    pub fn is_drive(&self) -> bool {
        self.kind == EntryKind::Drive
    }

    pub fn is_virtual_location(&self) -> bool {
        matches!(
            self.kind,
            EntryKind::Parent | EntryKind::Home | EntryKind::Favorite | EntryKind::Drive
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortField {
    Name,
    Size,
    Modified,
}

impl SortField {
    pub const fn next(self) -> Self {
        match self {
            Self::Name => Self::Size,
            Self::Size => Self::Modified,
            Self::Modified => Self::Name,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Size => "size",
            Self::Modified => "modified",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadState {
    Loading,
    Ready,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct PaneState {
    pub location: PathBuf,
    pub entries: Vec<FileEntry>,
    pub cursor: usize,
    pub selected: HashSet<PathBuf>,
    pub filter: String,
    pub show_hidden: bool,
    pub sort: SortField,
    pub load_state: LoadState,
    pub generation: u64,
    pub truncated: bool,
    pub browsing_drives: bool,
    focus_after_load: Option<PathBuf>,
}

impl PaneState {
    pub fn new(location: PathBuf) -> Self {
        Self {
            location,
            entries: Vec::new(),
            cursor: 0,
            selected: HashSet::new(),
            filter: String::new(),
            show_hidden: false,
            sort: SortField::Name,
            load_state: LoadState::Loading,
            generation: 0,
            truncated: false,
            browsing_drives: false,
            focus_after_load: None,
        }
    }

    pub fn begin_load(&mut self, location: PathBuf) -> u64 {
        self.begin_load_restoring_focus(location, None)
    }

    pub fn begin_load_restoring_focus(
        &mut self,
        location: PathBuf,
        focus_after_load: Option<PathBuf>,
    ) -> u64 {
        self.location = location;
        self.entries.clear();
        self.cursor = 0;
        self.selected.clear();
        self.load_state = LoadState::Loading;
        self.truncated = false;
        self.browsing_drives = false;
        self.focus_after_load = focus_after_load;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    pub fn begin_drive_list(&mut self) -> u64 {
        self.begin_drive_list_restoring_focus(None)
    }

    pub fn begin_drive_list_restoring_focus(&mut self, focus_after_load: Option<PathBuf>) -> u64 {
        self.location = PathBuf::new();
        self.entries.clear();
        self.cursor = 0;
        self.selected.clear();
        self.load_state = LoadState::Loading;
        self.truncated = false;
        self.browsing_drives = true;
        self.focus_after_load = focus_after_load;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    pub fn retry_load(&mut self) -> u64 {
        let focus_after_load = self.focus_after_load.take();
        if self.browsing_drives {
            self.begin_drive_list_restoring_focus(focus_after_load)
        } else {
            self.begin_load_restoring_focus(self.location.clone(), focus_after_load)
        }
    }

    /// Applies a result only if it belongs to the latest request.
    pub fn apply_entries(&mut self, generation: u64, entries: Vec<FileEntry>) -> bool {
        if generation != self.generation {
            return false;
        }
        self.entries = entries;
        self.sort_entries();
        if let Some(target) = self.focus_after_load.take() {
            self.restore_focus(&target);
        } else {
            self.cursor = self.cursor.min(self.visible_len().saturating_sub(1));
        }
        self.load_state = LoadState::Ready;
        self.truncated = false;
        self.focus_after_load = None;
        true
    }

    pub fn apply_error(&mut self, generation: u64, message: String) -> bool {
        if generation != self.generation {
            return false;
        }
        self.entries.clear();
        self.cursor = 0;
        self.load_state = LoadState::Failed(message);
        self.truncated = false;
        true
    }

    pub fn visible_indices(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.is_parent()
                    || ((self.show_hidden || !is_dot_hidden(&entry.display_name))
                        && (needle.is_empty()
                            || entry.display_name.to_lowercase().contains(&needle)))
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub fn visible_len(&self) -> usize {
        self.visible_indices().len()
    }

    pub fn focused(&self) -> Option<&FileEntry> {
        let index = *self.visible_indices().get(self.cursor)?;
        self.entries.get(index)
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let last = self.visible_len().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    pub fn toggle_focused_selection(&mut self) {
        let Some(path) = self.focused().map(|entry| entry.path.clone()) else {
            return;
        };
        if self.focused().is_some_and(FileEntry::is_virtual_location) {
            return;
        }
        if !self.selected.remove(&path) {
            self.selected.insert(path);
        }
    }

    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.cursor = self.cursor.min(self.visible_len().saturating_sub(1));
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        let focused_path = self.focused().map(|entry| entry.path.clone());
        self.sort_entries();
        if let Some(path) = focused_path
            && let Some(position) = self
                .visible_indices()
                .iter()
                .position(|&index| paths_match(&self.entries[index].path, &path))
        {
            self.cursor = position;
        }
    }

    pub fn selected_bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|entry| self.selected.contains(&entry.path))
            .filter_map(|entry| entry.size)
            .sum()
    }

    fn sort_entries(&mut self) {
        let sort = self.sort;
        self.entries.sort_by(|a, b| {
            let kind_order = entry_sort_group(a.kind).cmp(&entry_sort_group(b.kind));
            if !kind_order.is_eq() {
                return kind_order;
            }
            let primary = match sort {
                SortField::Name => a
                    .display_name
                    .to_lowercase()
                    .cmp(&b.display_name.to_lowercase()),
                SortField::Size => a.size.cmp(&b.size),
                SortField::Modified => a.modified.cmp(&b.modified),
            };
            primary
                .then_with(|| a.display_name.cmp(&b.display_name))
                .then_with(|| a.path.cmp(&b.path))
        });
    }

    fn restore_focus(&mut self, target: &Path) {
        let target_exists = self
            .entries
            .iter()
            .any(|entry| paths_match(&entry.path, target));
        let mut position = self.visible_indices().iter().position(|&index| {
            self.entries
                .get(index)
                .is_some_and(|entry| paths_match(&entry.path, target))
        });
        if position.is_none() && target_exists && !self.filter.is_empty() {
            self.filter.clear();
            position = self.visible_indices().iter().position(|&index| {
                self.entries
                    .get(index)
                    .is_some_and(|entry| paths_match(&entry.path, target))
            });
        }
        self.cursor = position.unwrap_or(0);
    }
}

fn entry_sort_group(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::Parent => 0,
        EntryKind::Home => 1,
        EntryKind::Favorite => 2,
        EntryKind::Drive => 3,
        EntryKind::Directory => 4,
        EntryKind::File => 5,
        EntryKind::Symlink => 6,
        EntryKind::Other => 7,
    }
}

#[cfg(windows)]
pub fn paths_match(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(windows))]
pub fn paths_match(left: &Path, right: &Path) -> bool {
    left == right
}

fn is_dot_hidden(name: &str) -> bool {
    name.starts_with('.') && name != "." && name != ".."
}

#[derive(Debug)]
pub struct AppState {
    pub panes: [PaneState; 2],
    pub active_pane: PaneId,
    pub help_visible: bool,
    pub filter_mode: bool,
    pub notice: Option<String>,
    pub should_quit: bool,
    pub operation: OperationView,
    pub text_prompt: Option<TextPrompt>,
    pub folder_size: FolderSizeState,
    pub preview: PreviewState,
    pub home_directory: Option<PathBuf>,
    pub favorites: Vec<PathBuf>,
    pub favorites_panel: Option<FavoritesPanel>,
}

#[derive(Clone, Debug, Default)]
pub struct FavoritesPanel {
    pub cursor: usize,
}

impl AppState {
    pub fn new(left: PathBuf, right: PathBuf) -> Self {
        Self {
            panes: [PaneState::new(left), PaneState::new(right)],
            active_pane: PaneId::Left,
            help_visible: false,
            filter_mode: false,
            notice: Some(
                "Ready · P Preview · C Copy · M Move · D Recycle · R Rename · N New folder".into(),
            ),
            should_quit: false,
            operation: OperationView::Idle,
            text_prompt: None,
            folder_size: FolderSizeState::Idle,
            preview: PreviewState::Closed,
            home_directory: None,
            favorites: Vec::new(),
            favorites_panel: None,
        }
    }

    pub fn active(&self) -> &PaneState {
        &self.panes[self.active_pane.index()]
    }

    pub fn active_mut(&mut self) -> &mut PaneState {
        &mut self.panes[self.active_pane.index()]
    }

    pub fn pane(&self, id: PaneId) -> &PaneState {
        &self.panes[id.index()]
    }

    pub fn pane_mut(&mut self, id: PaneId) -> &mut PaneState {
        &mut self.panes[id.index()]
    }

    pub fn switch_pane(&mut self) {
        self.active_pane = self.active_pane.other();
    }

    pub fn selected_count(&self) -> usize {
        self.panes.iter().map(|pane| pane.selected.len()).sum()
    }

    pub fn operation_sources(&self) -> Vec<PathBuf> {
        let pane = self.active();
        if pane.selected.is_empty() {
            return pane
                .focused()
                .filter(|entry| !entry.is_virtual_location())
                .map(|entry| vec![entry.path.clone()])
                .unwrap_or_default();
        }

        let mut sources: Vec<_> = pane.selected.iter().cloned().collect();
        sources.sort();
        sources
    }
}

pub fn parent_or_same(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(path)
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind, size: u64) -> FileEntry {
        FileEntry {
            path: PathBuf::from(name),
            display_name: name.into(),
            kind,
            size: Some(size),
            modified: None,
            metadata_incomplete: false,
            drive_info: None,
        }
    }

    #[test]
    fn focus_and_selection_are_independent() {
        let mut pane = PaneState::new(PathBuf::from("root"));
        pane.apply_entries(
            0,
            vec![
                entry("alpha", EntryKind::File, 1),
                entry("beta", EntryKind::File, 2),
            ],
        );
        pane.toggle_focused_selection();
        pane.move_cursor(1);

        assert_eq!(pane.focused().unwrap().display_name, "beta");
        assert!(pane.selected.contains(Path::new("alpha")));
        assert!(!pane.selected.contains(Path::new("beta")));
    }

    #[test]
    fn stale_results_are_ignored() {
        let mut pane = PaneState::new(PathBuf::from("old"));
        let current = pane.begin_load(PathBuf::from("new"));

        assert!(!pane.apply_entries(
            current.wrapping_sub(1),
            vec![entry("stale", EntryKind::File, 1),]
        ));
        assert!(pane.entries.is_empty());
        assert_eq!(pane.load_state, LoadState::Loading);
    }

    #[test]
    fn directories_sort_before_files_and_sort_changes_preserve_focus() {
        let mut pane = PaneState::new(PathBuf::from("root"));
        pane.apply_entries(
            0,
            vec![
                entry("large", EntryKind::File, 20),
                entry("folder", EntryKind::Directory, 0),
                entry("small", EntryKind::File, 2),
            ],
        );
        pane.move_cursor(1);
        assert_eq!(pane.focused().unwrap().display_name, "large");

        pane.cycle_sort();

        assert_eq!(pane.entries[0].display_name, "folder");
        assert_eq!(pane.focused().unwrap().display_name, "large");
    }

    #[test]
    fn filtering_clamps_cursor_and_hidden_files_are_opt_in() {
        let mut pane = PaneState::new(PathBuf::from("root"));
        pane.apply_entries(
            0,
            vec![
                entry(".secret", EntryKind::File, 1),
                entry("alpha", EntryKind::File, 1),
                entry("beta", EntryKind::File, 1),
            ],
        );
        pane.move_cursor(10);
        pane.set_filter("alpha".into());

        assert_eq!(pane.visible_len(), 1);
        assert_eq!(pane.cursor, 0);
        assert_eq!(pane.focused().unwrap().display_name, "alpha");
    }

    #[test]
    fn root_parent_navigation_is_harmless() {
        let root = Path::new(std::path::MAIN_SEPARATOR_STR);
        assert_eq!(parent_or_same(root), root);
    }

    #[test]
    fn parent_row_stays_first_and_cannot_be_selected() {
        let mut pane = PaneState::new(PathBuf::from("root/child"));
        pane.apply_entries(
            0,
            vec![
                entry("file", EntryKind::File, 1),
                entry("parent", EntryKind::Parent, 0),
                entry("folder", EntryKind::Directory, 0),
            ],
        );

        assert!(pane.focused().unwrap().is_parent());
        pane.toggle_focused_selection();
        assert!(pane.selected.is_empty());

        pane.set_filter("does-not-match".into());
        assert_eq!(pane.visible_len(), 1);
        assert!(pane.focused().unwrap().is_parent());
    }

    #[test]
    fn returning_to_a_parent_restores_focus_by_path_after_sorting() {
        let root = PathBuf::from("root");
        let child = root.join("beta");
        let mut pane = PaneState::new(child.clone());
        let generation = pane.begin_load_restoring_focus(root.clone(), Some(child.clone()));

        pane.apply_entries(
            generation,
            vec![
                FileEntry {
                    path: root.join("alpha"),
                    display_name: "alpha".into(),
                    kind: EntryKind::Directory,
                    size: None,
                    modified: None,
                    metadata_incomplete: false,
                    drive_info: None,
                },
                FileEntry {
                    path: child,
                    display_name: "beta".into(),
                    kind: EntryKind::Directory,
                    size: None,
                    modified: None,
                    metadata_incomplete: false,
                    drive_info: None,
                },
            ],
        );

        assert_eq!(pane.focused().unwrap().display_name, "beta");
    }

    #[test]
    fn failed_parent_scan_keeps_the_focus_target_for_retry() {
        let root = PathBuf::from("root");
        let child = root.join("child");
        let mut pane = PaneState::new(child.clone());
        let failed_generation = pane.begin_load_restoring_focus(root.clone(), Some(child.clone()));
        pane.apply_error(failed_generation, "temporary failure".into());

        let retry_generation = pane.retry_load();
        pane.apply_entries(
            retry_generation,
            vec![FileEntry {
                path: child,
                display_name: "child".into(),
                kind: EntryKind::Directory,
                size: None,
                modified: None,
                metadata_incomplete: false,
                drive_info: None,
            }],
        );

        assert_eq!(pane.focused().unwrap().display_name, "child");
    }

    #[test]
    fn returning_to_the_drive_list_restores_the_departed_drive() {
        let departed = PathBuf::from("D:\\");
        let mut pane = PaneState::new(departed.clone());
        let generation = pane.begin_drive_list_restoring_focus(Some(departed));
        pane.apply_entries(
            generation,
            vec![
                FileEntry {
                    path: PathBuf::from("C:\\"),
                    display_name: "C:\\".into(),
                    kind: EntryKind::Drive,
                    size: None,
                    modified: None,
                    metadata_incomplete: false,
                    drive_info: None,
                },
                FileEntry {
                    path: PathBuf::from("D:\\"),
                    display_name: "D:\\".into(),
                    kind: EntryKind::Drive,
                    size: None,
                    modified: None,
                    metadata_incomplete: false,
                    drive_info: None,
                },
            ],
        );

        assert_eq!(pane.focused().unwrap().display_name, "D:\\");
    }

    #[test]
    fn virtual_shortcuts_cannot_be_selected_or_used_as_operation_sources() {
        let path = PathBuf::from("favorite");
        let mut app = AppState::new(PathBuf::from("root"), PathBuf::from("other"));
        app.active_mut().apply_entries(
            0,
            vec![FileEntry {
                path,
                display_name: "favorite".into(),
                kind: EntryKind::Favorite,
                size: None,
                modified: None,
                metadata_incomplete: false,
                drive_info: None,
            }],
        );

        app.active_mut().toggle_focused_selection();

        assert!(app.active().selected.is_empty());
        assert!(app.operation_sources().is_empty());
    }
}
