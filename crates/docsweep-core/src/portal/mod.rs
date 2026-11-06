//! Portal downloads: recipes, login secrets and the engine that runs them.
//!
//! The engine only knows the [`driver::BrowserDriver`] trait. The real browser
//! lives in `docsweep-browser`, which depends on this crate and never the other
//! way round.

pub mod credentials;
pub mod driver;
pub mod engine;
pub mod recipe;

use std::path::{Component, Path, PathBuf};

use crate::names;

/// Where a company's `portal.recipe` lives. Relative paths are relative to the
/// recipes directory. The concept writes them as `recipes/x.yaml`, so that
/// leading folder does not count twice.
pub fn recipe_path(recipes_dir: &Path, configured: &Path) -> PathBuf {
    if configured.is_absolute() {
        return configured.to_path_buf();
    }
    let mut components = configured.components();
    let without_folder = match components.next() {
        Some(Component::Normal(first)) if first == names::RECIPES_DIR => components.as_path(),
        _ => configured,
    };
    recipes_dir.join(without_folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_recipe_paths_resolve_against_the_recipes_directory() {
        let dir = Path::new("data").join("recipes");
        assert_eq!(recipe_path(&dir, Path::new("a.yaml")), dir.join("a.yaml"));
        assert_eq!(
            recipe_path(&dir, &Path::new("recipes").join("a.yaml")),
            dir.join("a.yaml")
        );
        assert_eq!(
            recipe_path(&dir, &Path::new("other").join("a.yaml")),
            dir.join("other").join("a.yaml")
        );
        let absolute = std::env::temp_dir().join("a.yaml");
        assert_eq!(recipe_path(&dir, &absolute), absolute);
    }
}
