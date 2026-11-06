//! The real browser for portal downloads: installed Chrome or Edge through the
//! DevTools protocol, with its own profile per company so that sessions persist.
//!
//! This is an ordinary browser that identifies as automated (`--enable-automation`
//! stays on). There is deliberately no stealth mode and no bot-detection evasion:
//! if a portal wants a person, a person is asked. Chrome's sandbox stays on as
//! well, because portal pages are untrusted, and invalid certificates are refused.

mod chrome;
mod recorder;

use std::path::{Path, PathBuf};

use chrono::Local;
use docsweep_core::ai::provider;
use docsweep_core::config::{Company, Config};
use docsweep_core::index::Index;
use docsweep_core::paths::AppDirs;
use docsweep_core::portal::driver::{
    BrowserDriver, ConfirmationHandler, DriverError, PortalFetcher,
};
use docsweep_core::portal::engine::{
    FetchError, FetchJob, FetchReport, RecipeNotice, RepairSetup, run_recipe,
};
use docsweep_core::portal::recipe::{AiFallback, Recipe};
use docsweep_core::portal::recipe_path;
use docsweep_core::secrets::SecretStore;

pub use chrome::{ChromeDriver, ChromeOptions, WindowMode};
pub use recorder::Recorder;

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
) -> Result<(Recipe, PathBuf), String> {
    let portal = company
        .portal
        .as_ref()
        .ok_or_else(|| FetchError::NoPortal(company.id.clone()).to_string())?;
    if company.is_assisted() {
        return Err(FetchError::Assisted(company.id.clone()).to_string());
    }
    let file = match (&options.recipe_file, &portal.recipe) {
        (Some(file), _) => file.clone(),
        (None, Some(recipe)) => recipe_path(&paths.recipes_dir, recipe),
        (None, None) => return Err(FetchError::NoRecipe(company.id.clone()).to_string()),
    };
    Recipe::load(&file)
        .and_then(|recipe| recipe.with_params(&portal.params))
        .map(|recipe| (recipe, file))
        .map_err(|error| error.to_string())
}

/// Fetches the invoices of one company from its portal. Every problem ends up in
/// the report, so that it also reaches the run history.
pub async fn fetch_company<H: ConfirmationHandler>(
    config: &Config,
    company: &Company,
    secrets: &dyn SecretStore,
    paths: &FetchPaths,
    options: &FetchOptions,
    handler: &mut H,
) -> FetchReport {
    let failed = |error: String| FetchReport::failed(company, error);
    let (recipe, recipe_file) = match load_recipe(company, paths, options) {
        Ok(loaded) => loaded,
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
    let mut job = match job {
        Ok(job) => job,
        Err(error) => return failed(error.to_string()),
    };
    // Nothing is set up, and no key read, unless both the recipe and the
    // configuration ask for it.
    let mut setup_problem = None;
    if recipe.ai_fallback == AiFallback::RepairStep
        && let Some(llm) = &config.llm
    {
        match provider::from_config(llm, secrets) {
            Ok(ai) => job = job.with_repair(RepairSetup { ai, recipe_file }),
            Err(error) => setup_problem = Some(error.to_string()),
        }
    }
    let chrome_options = ChromeOptions {
        profile_dir: paths.profile_dir.clone(),
        window: options.window,
    };
    let mut driver = match ChromeDriver::launch(chrome_options).await {
        Ok(driver) => driver,
        Err(error) => return failed(error.to_string()),
    };
    let mut report = run_recipe(&job, &mut driver, handler).await;
    driver.close().await;
    if let Some(reason) = setup_problem
        && report.error.is_some()
    {
        report.notices.push(RecipeNotice::RepairNotDone {
            step: "recipe".to_owned(),
            reason,
        });
    }
    report
}

/// A page as a browser with this profile sees it, without typing anything. The
/// `html` is untrusted and must go through `ai::sanitize`.
#[derive(Debug, Clone)]
pub struct PageSnapshot {
    pub address: String,
    pub html: String,
}

/// How long a page gets to build itself after it has loaded.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

/// Opens one address without a window and reads what it shows. This logs nobody
/// in, so a page behind a login shows its login form. A saved session may open the
/// page itself.
pub async fn read_page(profile_dir: &Path, address: &str) -> Result<PageSnapshot, DriverError> {
    let mut driver = ChromeDriver::launch(ChromeOptions {
        profile_dir: profile_dir.to_path_buf(),
        window: WindowMode::Hidden,
    })
    .await?;
    let read = async {
        driver.goto(address).await?;
        tokio::time::sleep(SETTLE).await;
        Ok(PageSnapshot {
            address: driver.current_url().await?,
            html: driver.page_html().await?,
        })
    }
    .await;
    driver.close().await;
    read
}

/// Real portal downloads with the standard locations, for link mails and the
/// schedule.
pub struct BrowserFetcher<'a, H> {
    pub config: &'a Config,
    pub secrets: &'a dyn SecretStore,
    pub dirs: &'a AppDirs,
    pub index: &'a Path,
    pub window: WindowMode,
    pub handler: &'a mut H,
}

impl<H: ConfirmationHandler> PortalFetcher for BrowserFetcher<'_, H> {
    async fn fetch(&mut self, company: &Company) -> FetchReport {
        let paths = FetchPaths {
            index: self.index.to_path_buf(),
            ..FetchPaths::of(self.dirs, &company.id)
        };
        let options = FetchOptions {
            dry_run: false,
            window: self.window,
            recipe_file: None,
        };
        fetch_company(
            self.config,
            company,
            self.secrets,
            &paths,
            &options,
            self.handler,
        )
        .await
    }
}
