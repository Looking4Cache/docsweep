//! The `docsweep` command line tool.

mod report;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use docsweep_core::config::Config;
use docsweep_core::mail::execute::{AccountReport, Outcome, run_mail_scan};
use docsweep_core::names;
use docsweep_core::paths::AppDirs;
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
        Command::Secret {
            command: SecretCommand::Set { id },
        } => secret_set(&id),
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
    fn config_path_can_follow_the_subcommand() {
        let cli =
            Cli::try_parse_from(["docsweep", "mail-scan", "--config", "/x/docsweep.yaml"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/x/docsweep.yaml")));
    }
}
