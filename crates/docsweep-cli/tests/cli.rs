//! Runs the real `docsweep` binary. A server that accepts it cannot be reached
//! here, because the CLI refuses unencrypted connections; the pipeline behind it
//! is covered in `docsweep-core/tests/imap.rs`.

use std::fs;
use std::process::{Command, Output};

fn docsweep(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_docsweep"))
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env_remove("DOCSWEEP_CONFIG")
        .output()
        .unwrap()
}

#[test]
fn missing_configuration_is_explained() {
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("missing.yaml");
    let output = docsweep(
        home.path(),
        &[
            "mail-scan",
            "--dry-run",
            "--config",
            missing.to_str().unwrap(),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("cannot read configuration file"),
        "{stderr}"
    );
}

#[test]
fn invalid_configuration_lists_problems_without_quoting_secrets() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("docsweep.yaml");
    fs::write(
        &path,
        "mail:\n  accounts:\n    - { id: a, imap: nope, user: u, password: hunter2-secret, watch: INBOX }\n  disposal: { mode: all_delete }\n",
    )
    .unwrap();
    let output = docsweep(
        home.path(),
        &["mail-scan", "--dry-run", "--config", path.to_str().unwrap()],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("mail.accounts[0].imap"), "{stderr}");
    assert!(stderr.contains("mail.accounts[0].password"), "{stderr}");
    assert!(!stderr.contains("hunter2-secret"), "{stderr}");
}

#[test]
fn account_problems_are_reported_and_a_dry_run_creates_no_index() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("docsweep.yaml");
    fs::write(
        &path,
        "mail:\n  accounts:\n    - { id: a, imap: \"127.0.0.1:1\", user: u, password: \"keyring:docsweep-test-absent\", watch: INBOX }\n  disposal: { mode: all_delete }\n",
    )
    .unwrap();
    let output = docsweep(
        home.path(),
        &["mail-scan", "--dry-run", "--config", path.to_str().unwrap()],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success());
    assert!(stdout.contains("Dry run: nothing was changed."), "{stdout}");
    assert!(stdout.contains("ERROR:"), "{stdout}");

    let created: Vec<_> = fs::read_dir(home.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        created,
        ["docsweep.yaml"],
        "a dry run must not create anything"
    );
}

#[test]
fn help_lists_the_commands() {
    let home = tempfile::tempdir().unwrap();
    let output = docsweep(home.path(), &["--help"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("mail-scan"), "{stdout}");
    assert!(stdout.contains("secret"), "{stdout}");
}
