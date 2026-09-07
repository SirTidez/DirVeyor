use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAX_FAVORITES: usize = 256;
const MAX_FAVORITES_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct FavoritesStore {
    path: Option<PathBuf>,
    legacy_path: Option<PathBuf>,
}

impl FavoritesStore {
    pub fn discover() -> Self {
        Self {
            path: configuration_root().map(|root| root.join("favorites.json")),
            legacy_path: legacy_configuration_root().map(|root| root.join("favorites.json")),
        }
    }

    pub fn from_path(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            legacy_path: None,
        }
    }

    pub fn load(&self) -> Result<Vec<PathBuf>, String> {
        let Some(path) = &self.path else {
            return Ok(Vec::new());
        };
        let path = if path.exists() {
            path
        } else if let Some(legacy_path) = self
            .legacy_path
            .as_ref()
            .filter(|legacy_path| legacy_path.exists())
        {
            legacy_path
        } else {
            return Ok(Vec::new());
        };
        if path
            .metadata()
            .is_ok_and(|metadata| metadata.len() > MAX_FAVORITES_FILE_BYTES)
        {
            return Err("Favorites file exceeds the 1 MiB safety limit".into());
        }
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("Could not read favorites — {error}")),
        };
        let paths: Vec<PathBuf> = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Favorites file is invalid — {error}"))?;
        let paths = deduplicate(paths);
        if paths.len() > MAX_FAVORITES {
            return Err("Favorites file contains more than 256 folders".into());
        }
        Ok(paths)
    }

    pub fn save(&self, paths: &[PathBuf]) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Err("No user configuration directory is available".into());
        };
        if paths.len() > MAX_FAVORITES {
            return Err("Favorites are limited to 256 folders".into());
        }
        let parent = path
            .parent()
            .ok_or_else(|| "Favorites configuration path has no parent".to_owned())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create favorites directory — {error}"))?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| format!("Could not prepare favorites file — {error}"))?;
        serde_json::to_writer_pretty(&mut temporary, &deduplicate(paths.to_vec()))
            .map_err(|error| format!("Could not encode favorites — {error}"))?;
        temporary
            .write_all(b"\n")
            .and_then(|()| temporary.flush())
            .map_err(|error| format!("Could not write favorites — {error}"))?;
        temporary
            .persist(path)
            .map_err(|error| format!("Could not publish favorites — {}", error.error))?;
        Ok(())
    }
}

impl Default for FavoritesStore {
    fn default() -> Self {
        Self::discover()
    }
}

pub fn user_home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("USERPROFILE").map(PathBuf::from).or_else(|| {
            let drive = env::var_os("HOMEDRIVE")?;
            let path = env::var_os("HOMEPATH")?;
            Some(PathBuf::from(drive).join(path))
        })
    }
    #[cfg(not(windows))]
    {
        env::var_os("HOME").map(PathBuf::from)
    }
}

fn configuration_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("APPDATA")
            .or_else(|| env::var_os("LOCALAPPDATA"))
            .map(PathBuf::from)
            .map(|root| root.join("DirVeyor"))
    }
    #[cfg(not(windows))]
    {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| user_home_directory().map(|home| home.join(".config")))
            .map(|root| root.join("dirveyor"))
    }
}

fn legacy_configuration_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("APPDATA")
            .or_else(|| env::var_os("LOCALAPPDATA"))
            .map(PathBuf::from)
            .map(|root| root.join("FileAdmin"))
    }
    #[cfg(not(windows))]
    {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| user_home_directory().map(|home| home.join(".config")))
            .map(|root| root.join("fileadmin"))
    }
}

fn deduplicate(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !unique.iter().any(|existing| paths_match(existing, &path)) {
            unique.push(path);
        }
    }
    unique
}

#[cfg(windows)]
fn paths_match(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(windows))]
fn paths_match(left: &Path, right: &Path) -> bool {
    left == right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_store_loads_as_empty_and_round_trips_paths() {
        let temp = tempfile::tempdir().unwrap();
        let store = FavoritesStore::from_path(temp.path().join("nested").join("favorites.json"));
        assert!(store.load().unwrap().is_empty());

        let paths = vec![PathBuf::from("alpha"), PathBuf::from("beta")];
        store.save(&paths).unwrap();

        assert_eq!(store.load().unwrap(), paths);
        let replacement = vec![PathBuf::from("gamma")];
        store.save(&replacement).unwrap();
        assert_eq!(store.load().unwrap(), replacement);
    }

    #[test]
    fn duplicate_paths_are_removed_without_reordering() {
        let paths = vec![
            PathBuf::from("alpha"),
            PathBuf::from("beta"),
            PathBuf::from("alpha"),
        ];
        assert_eq!(
            deduplicate(paths),
            vec![PathBuf::from("alpha"), PathBuf::from("beta")]
        );
    }

    #[test]
    fn legacy_store_is_used_only_when_the_new_store_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let current_path = temp.path().join("DirVeyor").join("favorites.json");
        let legacy_path = temp.path().join("FileAdmin").join("favorites.json");
        let legacy_store = FavoritesStore::from_path(legacy_path.clone());
        legacy_store.save(&[PathBuf::from("legacy")]).unwrap();

        let store = FavoritesStore {
            path: Some(current_path.clone()),
            legacy_path: Some(legacy_path),
        };
        assert_eq!(store.load().unwrap(), vec![PathBuf::from("legacy")]);

        FavoritesStore::from_path(current_path)
            .save(&[PathBuf::from("current")])
            .unwrap();
        assert_eq!(store.load().unwrap(), vec![PathBuf::from("current")]);
    }
}
