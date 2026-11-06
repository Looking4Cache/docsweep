//! The `docsweep` command line tool.

mod confirm;
mod report;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use docsweep_browser::{FetchOptions, FetchPaths, WindowMode, fetch_company};
use docsweep_core::config::{Company, Config};
use docsweep_core::mail::execute::{AccountReport, Outcome, run_mail_scan};
use docsweep_core::names;
use docsweep_core::paths::AppDirs;
use docsweep_core::portal::credentials::{PortalCredentials, TotpGenerator};
use docsweep_core::portal::engine::FetchReport;
use docsweep_core::portal::recipe::Recipe;
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
    /// Manages passwords and keys in the system keychain
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
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

fn mail_scan(config_path: Option<PathBuf>, dry_run: bool) -> Result<ExitCode, CliError> {
    let dirs = AppDirs::discover().ok_or("no home directory found")?;
    let config_path = config_path.unwrap_or_else(|| dirs.config_file());
    let config = Config::load(&config_path)?;
    let index_path = dirs.index_file();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let reports = runtime.block_on(run_mail_scan(&config, &KeyringStore, &index_path, dry_run))?;

    print!("{}", report::render(&reports, dry_run));
    Ok(exit_code(&reports))
}

fn exit_code(reports: &[AccountReport]) -> ExitCode {
    let failed = reports.iter().any(|account| {
        account.error.is_some()
            || account
                .mails
                .iter()
                .any(|mail| matches!(mail.outcome, Outcome::Failed { .. }))
    });
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
    fn config_path_can_follow_the_subcommand() {
        let cli =
            Cli::try_parse_from(["docsweep", "mail-scan", "--config", "/x/docsweep.yaml"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/x/docsweep.yaml")));
    }
}
