//! Text for the few places outside the web view, from the same catalogues.

//! The web view picks the language and reports it through [`set_language`], so
//! the tray follows the same choice. Until then it is English.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

type Catalogue = HashMap<String, String>;

static ENGLISH: LazyLock<Catalogue> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../ui/i18n/en.json"))
        .expect("the English catalogue is valid JSON")
});

static GERMAN: LazyLock<Catalogue> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../ui/i18n/de.json"))
        .expect("the German catalogue is valid JSON")
});

static USE_GERMAN: AtomicBool = AtomicBool::new(false);

/// Switches the language; returns whether it is supported. Unsupported codes
/// leave the current language alone.
pub fn set_language(code: &str) -> bool {
    let Some(german) = is_german(code) else {
        return false;
    };
    USE_GERMAN.store(german, Ordering::SeqCst);
    true
}

fn is_german(code: &str) -> Option<bool> {
    match code {
        "en" => Some(false),
        "de" => Some(true),
        _ => None,
    }
}

/// Translates a key, falling back to English; an unknown key is shown as it is.
pub fn text(key: &str) -> String {
    text_in(USE_GERMAN.load(Ordering::SeqCst), key)
}

fn text_in(german: bool, key: &str) -> String {
    let current: &Catalogue = if german { &GERMAN } else { &ENGLISH };
    current
        .get(key)
        .or_else(|| ENGLISH.get(key))
        .cloned()
        .unwrap_or_else(|| key.to_owned())
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
    fn the_keys_of_the_tray_exist() {
        for key in [
            "tray.status_none",
            "tray.status_running",
            "tray.status_error",
            "tray.status_last_ok",
            "tray.status_last_failed",
        ] {
            assert!(ENGLISH.contains_key(key), "{key}");
        }
    }

    /// The `{name}` placeholders of a text, sorted.
    fn placeholders(text: &str) -> Vec<String> {
        let mut found: Vec<String> = text
            .split('{')
            .skip(1)
            .filter_map(|rest| rest.split_once('}').map(|(name, _)| name.to_owned()))
            .collect();
        found.sort();
        found
    }

    #[test]
    fn the_german_catalogue_has_every_key_with_the_same_placeholders() {
        let mut problems = Vec::new();
        for (key, english) in ENGLISH.iter() {
            match GERMAN.get(key) {
                None => problems.push(format!("missing: {key}")),
                Some(german) if placeholders(german) != placeholders(english) => {
                    problems.push(format!("placeholders differ: {key}"));
                }
                Some(_) => {}
            }
        }
        for key in GERMAN.keys() {
            if !ENGLISH.contains_key(key) {
                problems.push(format!("only in German: {key}"));
            }
        }
        problems.sort();
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn the_interface_lists_exactly_the_shipped_catalogues() {
        let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
        let script = fs::read_to_string(ui.join("lib/i18n.js")).unwrap();
        assert!(script.contains(r#"SUPPORTED_LANGUAGES = ["en", "de"]"#));
        for code in ["en", "de"] {
            assert!(ui.join(format!("i18n/{code}.json")).is_file());
        }
    }

    #[test]
    fn only_shipped_languages_are_accepted() {
        assert_eq!(is_german("de"), Some(true));
        assert_eq!(is_german("en"), Some(false));
        assert_eq!(is_german("fr"), None);
        assert_eq!(text_in(true, "tray.open"), "Docsweep öffnen");
        assert_eq!(text_in(false, "tray.open"), "Open Docsweep");
        assert_eq!(text_in(true, "does.not.exist"), "does.not.exist");
    }

    #[test]
    fn unknown_keys_fall_back_to_the_key() {
        assert_eq!(text("does.not.exist"), "does.not.exist");
        assert_eq!(text("tray.open"), "Open Docsweep");
    }
}
