//! The real browser for portal downloads: installed Chrome or Edge through the
//! DevTools protocol, with its own profile per company so that sessions persist.
//!
//! This is an ordinary browser that identifies as automated (`--enable-automation`
//! stays on). There is deliberately no stealth mode and no bot-detection evasion:
//! if a portal wants a person, a person is asked. Chrome's sandbox stays on as
//! well, because portal pages are untrusted, and invalid certificates are refused.

mod chrome;

use std::path::PathBuf;

use chrono::Local;
use docsweep_core::config::{Company, Config};
use docsweep_core::index::Index;
use docsweep_core::paths::AppDirs;
use docsweep_core::portal::driver::ConfirmationHandler;
use docsweep_core::portal::engine::{FetchError, FetchJob, FetchReport, run_recipe};
use docsweep_core::portal::recipe::Recipe;
use docsweep_core::portal::recipe_path;
use docsweep_core::secrets::SecretStore;

pub use chrome::{ChromeDriver, ChromeOptions, WindowMode};

/// The files a fetch works with.
#[derive(Debug, Clone)]
pub struct FetchPaths {
    pub index: PathBuf,
    pub recipes_dir: PathBuf,
    /// The browser profile of this company.
    pub profile_dir: PathBuf,
}

impl FetchPaths {
    /// The standard locations for a company.
    pub fn of(dirs: &AppDirs, company_id: &str) -> Self {
        Self {
            index: dirs.index_file(),
            recipes_dir: dirs.recipes_dir(),
            profile_dir: dirs.profile_dir(company_id),
        }
    }
}

/// How a fetch runs.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// List what would be downloaded, without downloading or storing anything.
    /// The browser still logs in, so its own profile does change.
    pub dry_run: bool,
    pub window: WindowMode,
    /// Use this recipe instead of the company's `portal.recipe`.
    pub recipe_file: Option<PathBuf>,
}

fn load_recipe(
    company: &Company,
    paths: &FetchPaths,
    options: &FetchOptions,
) -> Result<Recipe, String> {
    let portal = company
        .portal
        .as_ref()
        .ok_or_else(|| FetchError::NoPortal(company.id.clone()).to_string())?;
    let file = match &options.recipe_file {
        Some(file) => file.clone(),
        None => recipe_path(&paths.recipes_dir, &portal.recipe),
    };
    Recipe::load(&file)
        .and_then(|recipe| recipe.with_params(&portal.params))
        .map_err(|error| error.to_string())
}

/// Fetches the invoices of one company from its portal. Every problem ends up in
/// the report, so that it also reaches the run history.
///
/// Step 6 (link mails starting recipes) hooks in after this returns: a company's
/// `portal_fetch_required` rows can be resolved once a report has no failures.
pub async fn fetch_company<H: ConfirmationHandler>(
    config: &Config,
    company: &Company,
    secrets: &dyn SecretStore,
    paths: &FetchPaths,
    options: &FetchOptions,
    handler: &mut H,
) -> FetchReport {
    let failed = |error: String| FetchReport::failed(company, error);
    let recipe = match load_recipe(company, paths, options) {
        Ok(recipe) => recipe,
        Err(error) => return failed(error),
    };
    let opened = if options.dry_run {
        Index::open_read_only(&paths.index)
    } else {
        Index::open(&paths.index).map(Some)
    };
    let index = match opened {
        Ok(index) => index,
        Err(error) => return failed(error.to_string()),
    };
    let job = FetchJob::new(
        config,
        company,
        &recipe,
        secrets,
        index.as_ref(),
        Local::now().date_naive(),
        options.dry_run,
    );
    let job = match job {
        Ok(job) => job,
        Err(error) => return failed(error.to_string()),
    };
    let chrome_options = ChromeOptions {
        profile_dir: paths.profile_dir.clone(),
        window: options.window,
    };
    let mut driver = match ChromeDriver::launch(chrome_options).await {
        Ok(driver) => driver,
        Err(error) => return failed(error.to_string()),
    };
    let report = run_recipe(&job, &mut driver, handler).await;
    driver.close().await;
    report
}
