//! Text for the few places outside the web view, from the same catalogues.

use std::collections::HashMap;
use std::sync::LazyLock;

static ENGLISH: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../ui/i18n/en.json"))
        .expect("the English catalogue is valid JSON")
});

/// Translates a key; an unknown key is shown as it is.
pub fn text(key: &str) -> String {
    ENGLISH.get(key).cloned().unwrap_or_else(|| key.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// Collects the keys in `t("key")` and `tn("key", n)` calls of a script.
    fn keys_used_in(source: &str) -> Vec<String> {
        let mut keys = Vec::new();
        for marker in ["t(\"", "tn(\""] {
            for (position, _) in source.match_indices(marker) {
                let preceded_by_word = source[..position]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
                if preceded_by_word {
                    continue;
                }
                let rest = &source[position + marker.len()..];
                if let Some(end) = rest.find('"') {
                    keys.push(rest[..end].to_owned());
                }
            }
        }
        keys
    }

    fn scripts_below(dir: &Path, found: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scripts_below(&path, found);
            } else if path.extension().is_some_and(|e| e == "js") {
                found.push(fs::read_to_string(path).unwrap());
            }
        }
    }

    #[test]
    fn every_key_used_by_the_interface_exists() {
        let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
        let mut scripts = Vec::new();
        scripts_below(&ui, &mut scripts);
        assert!(!scripts.is_empty());

        let mut missing = Vec::new();
        for script in &scripts {
            for key in keys_used_in(script) {
                let plural = ENGLISH.contains_key(&format!("{key}_one"))
                    && ENGLISH.contains_key(&format!("{key}_other"));
                if !ENGLISH.contains_key(&key) && !plural {
                    missing.push(key);
                }
            }
        }
        assert!(missing.is_empty(), "missing translations: {missing:?}");
    }

    #[test]
    fn unknown_keys_fall_back_to_the_key() {
        assert_eq!(text("does.not.exist"), "does.not.exist");
        assert_eq!(text("tray.open"), "Open Docsweep");
    }
}
