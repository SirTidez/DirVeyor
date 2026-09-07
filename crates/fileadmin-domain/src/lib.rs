//! Pure state used by the FileAdmin UI and filesystem adapters.

mod operation;

pub use operation::*;

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
pub enum FolderSizeState {
    Idle,
    Loading {
        request_id: u64,
        pane: PaneId,
        generation: u64,
        path: PathBuf,
    },
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
            Self::Loading {
                pane: state_pane,
                generation: state_generation,
                path: state_path,
                ..
            }
            | Self::Failed {
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
            EntryKind::Parent | EntryKind::Drive | EntryKind::Directory
        )
    }

    pub fn is_parent(&self) -> bool {
        self.kind == EntryKind::Parent
    }

    pub fn is_drive(&self) -> bool {
        self.kind == EntryKind::Drive
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
        }
    }

    pub fn begin_load(&mut self, location: PathBuf) -> u64 {
        self.location = location;
        self.entries.clear();
        self.cursor = 0;
        self.selected.clear();
        self.load_state = LoadState::Loading;
        self.truncated = false;
        self.browsing_drives = false;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    pub fn begin_drive_list(&mut self) -> u64 {
        self.location = PathBuf::new();
        self.entries.clear();
        self.cursor = 0;
        self.selected.clear();
        self.load_state = LoadState::Loading;
        self.truncated = false;
        self.browsing_drives = true;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Applies a result only if it belongs to the latest request.
    pub fn apply_entries(&mut self, generation: u64, entries: Vec<FileEntry>) -> bool {
        if generation != self.generation {
            return false;
        }
        self.entries = entries;
        self.sort_entries();
        self.cursor = self.cursor.min(self.visible_len().saturating_sub(1));
        self.load_state = LoadState::Ready;
        self.truncated = false;
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
        if self
            .focused()
            .is_some_and(|entry| entry.is_parent() || entry.is_drive())
        {
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
                .position(|&index| self.entries[index].path == path)
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
            let parent_order = b.is_parent().cmp(&a.is_parent());
            if !parent_order.is_eq() {
                return parent_order;
            }
            let directory_order = b.is_directory().cmp(&a.is_directory());
            if !directory_order.is_eq() {
                return directory_order;
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
}

impl AppState {
    pub fn new(left: PathBuf, right: PathBuf) -> Self {
        Self {
            panes: [PaneState::new(left), PaneState::new(right)],
            active_pane: PaneId::Left,
            help_visible: false,
            filter_mode: false,
            notice: Some("Ready · c Copy · m Move · d Recycle · r Rename · n New folder".into()),
            should_quit: false,
            operation: OperationView::Idle,
            text_prompt: None,
            folder_size: FolderSizeState::Idle,
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
                .filter(|entry| !entry.is_parent() && !entry.is_drive())
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
}
