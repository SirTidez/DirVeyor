//! Pure state used by the FileAdmin UI and filesystem adapters.

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
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub display_name: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub metadata_incomplete: bool,
}

impl FileEntry {
    pub fn is_directory(&self) -> bool {
        self.kind == EntryKind::Directory
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
        }
    }

    pub fn begin_load(&mut self, location: PathBuf) -> u64 {
        self.location = location;
        self.entries.clear();
        self.cursor = 0;
        self.selected.clear();
        self.load_state = LoadState::Loading;
        self.truncated = false;
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
                (self.show_hidden || !is_dot_hidden(&entry.display_name))
                    && (needle.is_empty() || entry.display_name.to_lowercase().contains(&needle))
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
}

impl AppState {
    pub fn new(left: PathBuf, right: PathBuf) -> Self {
        Self {
            panes: [PaneState::new(left), PaneState::new(right)],
            active_pane: PaneId::Left,
            help_visible: false,
            filter_mode: false,
            notice: Some("Read-only prototype: filesystem changes are disabled".into()),
            should_quit: false,
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
}
