//! Binding names from the "Namenskonventionen" section of `KONZEPT.md`.
//!
//! The Bundle ID is deliberately not here: its single source is `identifier`
//! in `crates/docsweep-gui/tauri.conf.json`.

/// Name shown to users in windows, menus and installers.
pub const DISPLAY_NAME: &str = "Docsweep";
/// Name of the CLI binary and of Linux directories.
pub const LOWERCASE_NAME: &str = "docsweep";
/// File name of the configuration.
pub const CONFIG_FILE: &str = "docsweep.yaml";
/// File name of the SQLite index inside the data directory.
pub const INDEX_FILE: &str = "docsweep.db";
/// Browser profiles, one subdirectory per company.
pub const PROFILES_DIR: &str = "profiles";
/// Log files inside the data directory.
pub const LOGS_DIR: &str = "logs";
/// Portal recipes inside the data directory.
pub const RECIPES_DIR: &str = "recipes";
/// Service name of all keychain entries.
pub const KEYRING_SERVICE: &str = "docsweep";
/// Prefix of a keychain entry; the full entry name is `docsweep:<id>`.
pub const KEYRING_ENTRY_PREFIX: &str = "docsweep:";
/// Prefix of a secret reference in the configuration: `keyring:<id>`.
pub const CONFIG_SECRET_PREFIX: &str = "keyring:";
/// Environment variable that overrides the configuration path.
pub const ENV_CONFIG: &str = "DOCSWEEP_CONFIG";

/// `User-Agent` value for Docsweep's own HTTP requests.
pub fn user_agent() -> String {
    format!("{DISPLAY_NAME}/{}", env!("CARGO_PKG_VERSION"))
}

/// Keychain entry name for a secret id.
pub fn keyring_entry(id: &str) -> String {
    format!("{KEYRING_ENTRY_PREFIX}{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_uses_display_name_and_version() {
        assert_eq!(
            user_agent(),
            format!("Docsweep/{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn keyring_entry_has_documented_form() {
        assert_eq!(keyring_entry("telekom"), "docsweep:telekom");
    }
}
