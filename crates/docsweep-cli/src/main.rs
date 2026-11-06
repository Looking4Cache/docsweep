//! The `docsweep` command line tool.

mod confirm;
mod report;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::Local;
use clap::{Parser, Subcommand};
use docsweep_browser::{
    BrowserFetcher, ChromeOptions, FetchOptions, FetchPaths, Recorder, WindowMode, fetch_company,
    read_page,
};
use docsweep_core::ai::generate::{generation_prompt, known_hosts, request_draft};
use docsweep_core::ai::provider;
use docsweep_core::ai::sanitize::RawPage;
use docsweep_core::config::{Company, Config};
use docsweep_core::mail::execute::{
    AccountReport, ScanReport, preview_mail_scan, run_mail_scan, summarize,
};
use docsweep_core::names;
use docsweep_core::paths::AppDirs;
use docsweep_core::portal::credentials::{PortalCredentials, TotpGenerator};
use docsweep_core::portal::draft::{WriteError, draft_path, draft_text, write_draft};
use docsweep_core::portal::engine::FetchReport;
use docsweep_core::portal::recipe::Recipe;
use docsweep_core::portal::recording::build_draft;
use docsweep_core::secrets::{KeyringStore, SecretStore, SecretString};

#[derive(Parser)]
#[command(name = names::LOWERCASE_NAME, version, about = "Collects incoming invoices")]
struct Cli {
    /// Path of the configuration file (or set DOCSWEEP_CONFIG)
    #[arg(long, global = true, env = names::ENV_CONFIG, value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Saves invoices from mails and cleans up the mails
    MailScan {
        /// Show what would happen without changing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Fetches a company's invoices from its portal
    Fetch {
        /// The id of the company in the configuration
        company: String,
        /// List what would be downloaded without downloading anything
        #[arg(long)]
        dry_run: bool,
        /// Show the browser window
        #[arg(long)]
        visible: bool,
    },
    /// Checks a recipe file; with --company, also tries it as a dry run
    TestRecipe {
        /// The recipe file
        file: PathBuf,
        /// Try the recipe in the browser with this company's login
        #[arg(long)]
        company: Option<String>,
        /// Show the browser window
        #[arg(long)]
        visible: bool,
    },
    /// Creates the draft of a recipe, for a person to check with test-recipe
    Recipe {
        #[command(subcommand)]
        command: RecipeCommand,
    },
    /// Manages passwords and keys in the system keychain
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
}

#[derive(Subcommand)]
enum RecipeCommand {
    /// Lets the configured AI provider write a draft from a portal's page
    Generate {
        /// The id of the company in the configuration
        company: String,
        /// The address of the portal's login or list page
        #[arg(long)]
        url: String,
        /// What the provider should know, such as where the invoices are
        #[arg(long)]
        describe: Option<String>,
    },
    /// Opens a browser and writes a draft from what you do in it
    Record {
        /// The id of the company in the configuration
        company: String,
        /// The address to start at
        #[arg(long)]
        url: String,
    },
}

#[derive(Subcommand)]
enum SecretCommand {
    /// Stores a secret; it is asked for without being shown
    Set {
        /// The id used in the configuration as `keyring:<id>`
        id: String,
    },
    /// Stores user name and password of a portal; the password is not shown
    SetPortal {
        /// The id used in `portal.credentials` as `keyring:<id>`
        id: String,
    },
    /// Stores the TOTP setup key of a portal; it is not shown
    SetTotp {
        /// The id used in `portal.totp` as `keyring:<id>`
        id: String,
    },
}

/// What went wrong; printed once, without credentials.
type CliError = Box<dyn std::error::Error>;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, CliError> {
    match cli.command {
        Command::MailScan { dry_run } => mail_scan(cli.config, dry_run),
        Command::Fetch {
            company,
            dry_run,
            visible,
        } => fetch(cli.config, &company, dry_run, visible, None),
        Command::TestRecipe {
            file,
            company,
            visible,
        } => test_recipe(cli.config, &file, company.as_deref(), visible),
        Command::Recipe { command } => match command {
            RecipeCommand::Generate {
                company,
                url,
                describe,
            } => recipe_generate(cli.config, &company, &url, describe.as_deref()),
            RecipeCommand::Record { company, url } => recipe_record(cli.config, &company, &url),
        },
        Command::Secret { command } => match command {
            SecretCommand::Set { id } => secret_set(&id),
            SecretCommand::SetPortal { id } => secret_set_portal(&id),
            SecretCommand::SetTotp { id } => secret_set_totp(&id),
        },
    }
}

fn find_company<'a>(config: &'a Config, id: &str) -> Result<&'a Company, CliError> {
    config
        .companies
        .iter()
        .find(|company| company.id == id)
        .ok_or_else(|| format!("no company `{id}` in the configuration").into())
}

fn fetch(
    config_path: Option<PathBuf>,
    company_id: &str,
    dry_run: bool,
    visible: bool,
    recipe_file: Option<PathBuf>,
) -> Result<ExitCode, CliError> {
    let dirs = AppDirs::discover().ok_or("no home directory found")?;
    let config = Config::load(&config_path.unwrap_or_else(|| dirs.config_file()))?;
    let company = find_company(&config, company_id)?;
    let options = FetchOptions {
        dry_run,
        window: if visible {
            WindowMode::Visible
        } else {
            WindowMode::OnDemand
        },
        recipe_file,
    };
    let paths = FetchPaths::of(&dirs, &company.id);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let report: FetchReport = runtime.block_on(fetch_company(
        &config,
        company,
        &KeyringStore,
        &paths,
        &options,
        &mut confirm::TerminalConfirmation,
    ));

    print!("{}", report::render_fetch(&report, dry_run));
    Ok(if report.has_failures() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn test_recipe(
    config_path: Option<PathBuf>,
    file: &Path,
    company: Option<&str>,
    visible: bool,
) -> Result<ExitCode, CliError> {
    let recipe = Recipe::load(file)?;
    println!("{} is valid:", file.display());
    for line in recipe.outline() {
        println!("  {line}");
    }
    match company {
        Some(company) => fetch(
            config_path,
            company,
            true,
            visible,
            Some(file.to_path_buf()),
        ),
        None => Ok(ExitCode::SUCCESS),
    }
}

fn check_web_address(address: &str) -> Result<(), CliError> {
    let lower = address.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        Ok(())
    } else {
        Err("--url must be an `http` or `https` address".into())
    }
}

/// Writes a draft, and asks before replacing a file that is already there.
async fn write_draft_asking(path: &Path, text: &str) -> Result<bool, CliError> {
    match write_draft(path, text, false) {
        Ok(()) => Ok(true),
        Err(WriteError::Exists(_)) => {
            let question = format!("{} already exists. Replace it? [y/N] ", path.display());
            if confirm::ask_yes_no(&question).await {
                write_draft(path, text, true)?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn say_how_to_go_on(path: &Path, company: &str) {
    println!("Draft written to {}.", path.display());
    println!("Read it, and check it with:");
    println!(
        "  docsweep test-recipe '{}' --company {company}",
        path.display()
    );
    println!("To use it, point `portal.recipe` of this company at it.");
}

fn recipe_generate(
    config_path: Option<PathBuf>,
    company_id: &str,
    address: &str,
    description: Option<&str>,
) -> Result<ExitCode, CliError> {
    check_web_address(address)?;
    let dirs = AppDirs::discover().ok_or("no home directory found")?;
    let config = Config::load(&config_path.unwrap_or_else(|| dirs.config_file()))?;
    let company = find_company(&config, company_id)?;
    let llm = config
        .llm
        .as_ref()
        .ok_or("no AI provider is configured; add an `llm` section to the configuration")?;
    let ai = provider::from_config(llm, &KeyringStore)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        eprintln!("Looking at the page, without logging in...");
        let page = read_page(&dirs.profile_dir(company_id), address).await?;
        let prompt = generation_prompt(
            &ai,
            company,
            RawPage {
                url: &page.address,
                html: &page.html,
            },
            description,
        )?;
        if !confirm::ask_to_send(&ai.label, prompt.text()).await {
            println!("Nothing was sent.");
            return Ok(ExitCode::SUCCESS);
        }
        let binding = known_hosts(company, &dirs.recipes_dir(), address);
        let draft = request_draft(&ai, &prompt, &binding).await?;
        if !draft.host_changes.is_empty() {
            eprintln!("\nThis draft would leave the portal's known host:");
            for change in &draft.host_changes {
                eprintln!("  {}: {} -> {}", change.step, change.old, change.new);
            }
            eprintln!("A portal page can try to talk a model into this, so please look closely.");
            if !confirm::ask_yes_no("Keep this draft with the new host? [y/N] ").await {
                println!("Nothing was written.");
                return Ok(ExitCode::SUCCESS);
            }
        }
        let path = draft_path(&dirs.recipes_dir(), company_id);
        if write_draft_asking(&path, &draft.text).await? {
            say_how_to_go_on(&path, company_id);
        } else {
            println!("Nothing was written.");
        }
        Ok(ExitCode::SUCCESS)
    })
}

fn recipe_record(
    config_path: Option<PathBuf>,
    company_id: &str,
    address: &str,
) -> Result<ExitCode, CliError> {
    check_web_address(address)?;
    let dirs = AppDirs::discover().ok_or("no home directory found")?;
    let config = Config::load(&config_path.unwrap_or_else(|| dirs.config_file()))?;
    find_company(&config, company_id)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let mut recorder = Recorder::start(
            ChromeOptions {
                profile_dir: dirs.profile_dir(company_id),
                window: WindowMode::Visible,
            },
            address,
        )
        .await?;
        eprintln!(
            "A browser has opened. Log in as usual, go to the invoices, and click one document link in the list."
        );
        eprintln!(
            "Docsweep notes where you click and type, but never what you type. Press Enter here when you are done."
        );
        recorder
            .wait_for_end(async {
                confirm::ask_yes_no("").await;
            })
            .await;
        let events = recorder.finish().await;
        let draft = build_draft(address, &events)?;
        let text = draft_text("recorded in the browser", &draft.notes, &draft.recipe)?;
        let path = draft_path(&dirs.recipes_dir(), company_id);
        if write_draft_asking(&path, &text).await? {
            say_how_to_go_on(&path, company_id);
        } else {
            println!("Nothing was written.");
        }
        Ok(ExitCode::SUCCESS)
    })
}

fn mail_scan(config_path: Option<PathBuf>, dry_run: bool) -> Result<ExitCode, CliError> {
    let dirs = AppDirs::discover().ok_or("no home directory found")?;
    let config_path = config_path.unwrap_or_else(|| dirs.config_file());
    let config = Config::load(&config_path)?;
    let index_path = dirs.index_file();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let today = Local::now().date_naive();
    if dry_run {
        let reports: Vec<AccountReport> = runtime.block_on(preview_mail_scan(
            &config,
            &KeyringStore,
            &index_path,
            today,
        ))?;
        print!("{}", report::render(&reports, true));
        return Ok(exit_code(summarize(&reports).failed > 0));
    }

    let mut fetcher = BrowserFetcher {
        config: &config,
        secrets: &KeyringStore,
        dirs: &dirs,
        index: &index_path,
        window: WindowMode::OnDemand,
        handler: &mut confirm::TerminalConfirmation,
    };
    let scan: ScanReport = runtime.block_on(run_mail_scan(
        &config,
        &KeyringStore,
        &index_path,
        today,
        &mut fetcher,
    ))?;
    print!("{}", report::render(&scan.accounts, false));
    for fetch in &scan.fetches {
        print!("{}", report::render_fetch(fetch, false));
    }
    Ok(exit_code(scan.has_failures()))
}

fn exit_code(failed: bool) -> ExitCode {
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn secret_set(id: &str) -> Result<ExitCode, CliError> {
    let value = rpassword::prompt_password(format!("Secret for `{id}`: "))?;
    if value.is_empty() {
        return Err("nothing stored: the secret is empty".into());
    }
    KeyringStore.set(id, &SecretString::from(value))?;
    println!("Stored `{id}`. Refer to it in the configuration as `keyring:{id}`.");
    Ok(ExitCode::SUCCESS)
}

fn secret_set_portal(id: &str) -> Result<ExitCode, CliError> {
    print!("User name for `{id}`: ");
    std::io::stdout().flush()?;
    let mut user = String::new();
    std::io::stdin().lock().read_line(&mut user)?;
    let password = rpassword::prompt_password(format!("Password for `{id}`: "))?;
    let credentials = PortalCredentials::new(user.trim().to_owned(), password)?;
    KeyringStore.set(id, &credentials.to_stored())?;
    println!("Stored `{id}`. Refer to it as `portal.credentials: keyring:{id}`.");
    Ok(ExitCode::SUCCESS)
}

fn secret_set_totp(id: &str) -> Result<ExitCode, CliError> {
    let key = rpassword::prompt_password(format!("TOTP setup key for `{id}`: "))?;
    let key = SecretString::from(key);
    TotpGenerator::new(&key)?;
    KeyringStore.set(id, &key)?;
    println!("Stored `{id}`. Refer to it as `portal.totp: keyring:{id}`.");
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn dry_run_flag_is_understood() {
        let cli = Cli::try_parse_from(["docsweep", "mail-scan", "--dry-run"]).unwrap();
        assert!(matches!(cli.command, Command::MailScan { dry_run: true }));
    }

    #[test]
    fn secret_values_cannot_be_passed_as_arguments() {
        assert!(Cli::try_parse_from(["docsweep", "secret", "set", "id", "value"]).is_err());
    }

    #[test]
    fn fetch_flags_are_understood() {
        let cli =
            Cli::try_parse_from(["docsweep", "fetch", "acme", "--dry-run", "--visible"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Fetch {
                dry_run: true,
                visible: true,
                ..
            }
        ));
    }

    #[test]
    fn portal_secrets_cannot_be_passed_as_arguments() {
        for command in ["set-portal", "set-totp"] {
            assert!(Cli::try_parse_from(["docsweep", "secret", command, "id", "value"]).is_err());
        }
    }

    #[test]
    fn recipe_commands_are_understood() {
        let cli = Cli::try_parse_from([
            "docsweep",
            "recipe",
            "generate",
            "acme",
            "--url",
            "https://portal.test/",
            "--describe",
            "under Billing",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Recipe {
                command: RecipeCommand::Generate { .. }
            }
        ));
        let cli = Cli::try_parse_from([
            "docsweep",
            "recipe",
            "record",
            "acme",
            "--url",
            "https://portal.test/",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Recipe {
                command: RecipeCommand::Record { .. }
            }
        ));
        assert!(Cli::try_parse_from(["docsweep", "recipe", "record", "acme"]).is_err());
    }

    #[test]
    fn only_web_addresses_are_opened() {
        assert!(check_web_address("https://portal.test/").is_ok());
        assert!(check_web_address("file:///etc/passwd").is_err());
        assert!(check_web_address("javascript:alert(1)").is_err());
    }

    #[test]
    fn config_path_can_follow_the_subcommand() {
        let cli =
            Cli::try_parse_from(["docsweep", "mail-scan", "--config", "/x/docsweep.yaml"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/x/docsweep.yaml")));
    }
}
