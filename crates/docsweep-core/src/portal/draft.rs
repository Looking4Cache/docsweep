//! Writing recipes: drafts from the recorder or a language model, and a recipe
//! with a repaired step.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::recipe::{Recipe, Step};

/// Where the draft of a company goes, next to the recipes.
pub fn draft_path(recipes_dir: &Path, company_id: &str) -> PathBuf {
    recipes_dir.join(format!("{company_id}.draft.yaml"))
}

/// Why a file could not be written.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("{0} already exists")]
    Exists(PathBuf),
    #[error("cannot write {path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("the recipe file has changed since this run started, so it was left alone")]
    ChangedOnDisk,
    #[error("the recipe could not be written: {0}")]
    Invalid(String),
}

fn atomic_write(path: &Path, text: &str) -> Result<(), WriteError> {
    let failed = |error: &dyn std::fmt::Display| WriteError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|e| failed(&e))?;
    let mut temporary = tempfile::NamedTempFile::new_in(dir).map_err(|e| failed(&e))?;
    temporary
        .write_all(text.as_bytes())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|e| failed(&e))?;
    temporary.persist(path).map_err(|e| failed(&e))?;
    Ok(())
}

/// Writes a draft. An existing file stays unless a person agreed to replace it.
pub fn write_draft(path: &Path, text: &str, overwrite: bool) -> Result<(), WriteError> {
    if path.exists() && !overwrite {
        return Err(WriteError::Exists(path.to_path_buf()));
    }
    atomic_write(path, text)
}

/// A recipe with a comment on top that says what it is and how to check it. A
/// model's own comments were dropped when it was parsed.
pub fn draft_text(origin: &str, notes: &[String], recipe: &Recipe) -> Result<String, WriteError> {
    let yaml = recipe
        .to_yaml()
        .map_err(|error| WriteError::Invalid(error.to_string()))?;
    let mut text = format!(
        "# Draft of a recipe, {origin}. Please read it before using it.\n\
         # Check it with: docsweep test-recipe <this file> --company <id>\n\
         # With `ai_fallback: repair_step`, a language model may repair a step later.\n"
    );
    for note in notes {
        for line in note.lines() {
            text.push_str("# ");
            text.push_str(line);
            text.push('\n');
        }
    }
    text.push_str(&yaml);
    Ok(text)
}

/// The file that keeps the recipe from before a repair.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".bak");
    PathBuf::from(name)
}

/// Replaces one step in a recipe file and keeps the previous file as `.bak`.
/// Writes go through the unfilled recipe, so that placeholders stay placeholders.
/// `resolved` is what the run used: if the file now says something else, a person
/// changed it in the meantime, and that wins.
pub fn save_repaired_step(
    path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    resolved: &Recipe,
    index: usize,
    step: &Step,
) -> Result<PathBuf, WriteError> {
    let current = Recipe::load(path).map_err(|_| WriteError::ChangedOnDisk)?;
    let same = current
        .with_params(params)
        .is_ok_and(|filled| filled.steps == resolved.steps);
    if !same {
        return Err(WriteError::ChangedOnDisk);
    }
    let mut repaired = current;
    *repaired
        .steps
        .get_mut(index)
        .ok_or(WriteError::ChangedOnDisk)? = step.clone();
    let issues = repaired.validate();
    if let Some(issue) = issues.first() {
        return Err(WriteError::Invalid(format!(
            "{}: {}",
            issue.path, issue.message
        )));
    }
    let text = repaired
        .to_yaml()
        .map_err(|error| WriteError::Invalid(error.to_string()))?;

    let backup = backup_path(path);
    fs::copy(path, &backup).map_err(|error| WriteError::Io {
        path: backup.clone(),
        message: error.to_string(),
    })?;
    atomic_write(path, &text)?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const RECIPE: &str = r##"# my comment
params: { domain: portal.test }
steps:
  - goto: "https://{domain}/login"
  - login: { user: "#u", password: "#p", submit: "#s" }
"##;

    fn new_login() -> Step {
        let Recipe { mut steps, .. } = Recipe::from_yaml(
            r##"steps: [ { login: { user: "#new", password: "#p", submit: "#s" } } ]"##,
        )
        .unwrap();
        steps.remove(0)
    }

    fn resolved() -> Recipe {
        Recipe::from_yaml(RECIPE)
            .unwrap()
            .with_params(&BTreeMap::new())
            .unwrap()
    }

    #[test]
    fn a_draft_does_not_replace_a_file_unless_told() {
        let dir = tempfile::tempdir().unwrap();
        let path = draft_path(&dir.path().join("recipes"), "acme");
        assert!(path.ends_with("acme.draft.yaml"));

        write_draft(&path, "first", false).unwrap();
        let refused = write_draft(&path, "second", false).unwrap_err();
        assert!(matches!(refused, WriteError::Exists(_)));
        assert_eq!(fs::read_to_string(&path).unwrap(), "first");

        write_draft(&path, "second", true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn a_draft_says_what_it_is_and_stays_a_valid_recipe() {
        let text = draft_text(
            "written by a language model",
            &["two\nlines".to_owned()],
            &resolved(),
        )
        .unwrap();
        assert!(text.starts_with("# Draft of a recipe, written by a language model."));
        assert!(text.contains("# two\n# lines\n"));
        assert_eq!(Recipe::from_yaml(&text).unwrap(), resolved());
    }

    #[test]
    fn a_repair_keeps_the_old_file_and_the_placeholders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acme.yaml");
        fs::write(&path, RECIPE).unwrap();

        let backup =
            save_repaired_step(&path, &BTreeMap::new(), &resolved(), 1, &new_login()).unwrap();

        assert_eq!(backup, dir.path().join("acme.yaml.bak"));
        assert_eq!(fs::read_to_string(&backup).unwrap(), RECIPE);
        let saved = Recipe::load(&path).unwrap();
        assert_eq!(saved.steps[1], new_login());
        assert_eq!(saved.steps[0], Step::Goto("https://{domain}/login".into()));
        assert_eq!(saved.params["domain"], "portal.test");
    }

    #[test]
    fn a_file_changed_during_the_run_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acme.yaml");
        let edited = RECIPE.replace("#u", "#edited");
        fs::write(&path, &edited).unwrap();

        let error =
            save_repaired_step(&path, &BTreeMap::new(), &resolved(), 1, &new_login()).unwrap_err();

        assert!(matches!(error, WriteError::ChangedOnDisk));
        assert_eq!(fs::read_to_string(&path).unwrap(), edited);
        assert!(!backup_path(&path).exists());
    }
}
