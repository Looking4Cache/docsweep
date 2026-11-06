//! The project's platform directories.
//!
//! This uses `directories::BaseDirs` on purpose. `ProjectDirs` would append
//! `config` and `data` on Windows and build a reverse-domain name on macOS,
//! neither of which matches the documented paths.

use std::path::{Path, PathBuf};

use directories::BaseDirs;

use crate::names;

/// Configuration and data directories of the current user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    config_dir: PathBuf,
    data_dir: PathBuf,
}

impl AppDirs {
    /// Resolves the directories, or returns `None` if no home directory exists.
    pub fn discover() -> Option<Self> {
        let base = BaseDirs::new()?;
        Some(Self::from_base(base.config_dir(), base.data_dir()))
    }

    /// The directories below given base directories.
    pub fn from_base(config_base: &Path, data_base: &Path) -> Self {
        let app_dir = if cfg!(any(target_os = "macos", target_os = "windows")) {
            names::DISPLAY_NAME
        } else {
            names::LOWERCASE_NAME
        };
        Self {
            config_dir: config_base.join(app_dir),
            data_dir: data_base.join(app_dir),
        }
    }

    /// Default path of `docsweep.yaml`.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join(names::CONFIG_FILE)
    }

    /// Directory holding the index, profiles, logs and recipes.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Path of `docsweep.db`.
    pub fn index_file(&self) -> PathBuf {
        self.data_dir.join(names::INDEX_FILE)
    }

    /// Browser profile directory of one company.
    pub fn profile_dir(&self, company_id: &str) -> PathBuf {
        self.data_dir.join(names::PROFILES_DIR).join(company_id)
    }

    /// Directory for log files.
    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join(names::LOGS_DIR)
    }

    /// Directory for portal recipes.
    pub fn recipes_dir(&self) -> PathBuf {
        self.data_dir.join(names::RECIPES_DIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_uses_application_support() {
        let home = BaseDirs::new().unwrap().home_dir().to_path_buf();
        let dirs = AppDirs::discover().unwrap();
        let root = home.join("Library/Application Support/Docsweep");
        assert_eq!(dirs.config_file(), root.join("docsweep.yaml"));
        assert_eq!(dirs.index_file(), root.join("docsweep.db"));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn windows_uses_roaming_app_data_without_suffix() {
        let base = BaseDirs::new().unwrap();
        let root = base
            .home_dir()
            .join("AppData")
            .join("Roaming")
            .join("Docsweep");
        let dirs = AppDirs::discover().unwrap();
        assert_eq!(dirs.config_file(), root.join("docsweep.yaml"));
        assert_eq!(dirs.index_file(), root.join("docsweep.db"));
    }

    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn linux_separates_config_and_data() {
        let dirs = AppDirs::from_base(Path::new("/h/.config"), Path::new("/h/.local/share"));
        assert_eq!(
            dirs.config_file(),
            Path::new("/h/.config/docsweep/docsweep.yaml")
        );
        assert_eq!(
            dirs.index_file(),
            Path::new("/h/.local/share/docsweep/docsweep.db")
        );
    }

    #[test]
    fn data_subdirectories_follow_the_concept() {
        let dirs = AppDirs::from_base(Path::new("c"), Path::new("d"));
        let root = dirs.data_dir().to_path_buf();
        assert_eq!(
            dirs.profile_dir("telekom"),
            root.join("profiles").join("telekom")
        );
        assert_eq!(dirs.logs_dir(), root.join("logs"));
        assert_eq!(dirs.recipes_dir(), root.join("recipes"));
    }
}
