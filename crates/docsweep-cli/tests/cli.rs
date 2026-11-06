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

const VALID_RECIPE: &str =
    "steps:\n  - goto: https://portal.test/login\n  - login: { user: a, password: b, submit: c }\n";

#[test]
fn test_recipe_shows_the_steps_of_a_valid_file() {
    let home = tempfile::tempdir().unwrap();
    let recipe = home.path().join("recipe.yaml");
    fs::write(&recipe, VALID_RECIPE).unwrap();
    let output = docsweep(home.path(), &["test-recipe", recipe.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("goto https://portal.test/login"),
        "{stdout}"
    );
    assert!(stdout.contains("login"), "{stdout}");
}

#[test]
fn test_recipe_lists_the_problems_of_an_invalid_file() {
    let home = tempfile::tempdir().unwrap();
    let recipe = home.path().join("recipe.yaml");
    fs::write(&recipe, "steps: [ { goto: \"file:///etc/passwd\" } ]\n").unwrap();
    let output = docsweep(home.path(), &["test-recipe", recipe.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("steps[0].goto"), "{stderr}");
}

fn config_with_portal(home: &std::path::Path, recipe: &std::path::Path) -> std::path::PathBuf {
    let path = home.join("docsweep.yaml");
    fs::write(
        &path,
        format!(
            r#"
mail:
  accounts:
    - {{ id: a, imap: "127.0.0.1:1", user: u, password: "keyring:docsweep-test-absent", watch: INBOX }}
  disposal: {{ mode: all_delete }}
companies:
  - id: acme
    name: Acme
    portal: {{ credentials: "keyring:docsweep-test-absent", recipe: '{}' }}
    output: {{ dir: '{}', filename: "{{invoice_no}}.pdf" }}
"#,
            recipe.display(),
            home.join("invoices").display()
        ),
    )
    .unwrap();
    path
}

#[test]
fn fetch_names_an_unknown_company() {
    let home = tempfile::tempdir().unwrap();
    let config = config_with_portal(home.path(), &home.path().join("recipe.yaml"));
    let output = docsweep(
        home.path(),
        &["fetch", "nope", "--config", config.to_str().unwrap()],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("no company `nope`"), "{stderr}");
}

#[test]
fn fetch_explains_a_missing_recipe_before_starting_a_browser() {
    let home = tempfile::tempdir().unwrap();
    let missing = home.path().join("missing.yaml");
    let config = config_with_portal(home.path(), &missing);
    let output = docsweep(
        home.path(),
        &[
            "fetch",
            "acme",
            "--dry-run",
            "--config",
            config.to_str().unwrap(),
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success());
    assert!(stdout.contains("ERROR: cannot read recipe"), "{stdout}");
    assert!(stdout.contains("missing.yaml"), "{stdout}");
    assert!(!home.path().join("invoices").exists());
}

#[test]
fn fetch_without_stored_credentials_fails_before_starting_a_browser() {
    let home = tempfile::tempdir().unwrap();
    let recipe = home.path().join("recipe.yaml");
    fs::write(&recipe, VALID_RECIPE).unwrap();
    let config = config_with_portal(home.path(), &recipe);
    let output = docsweep(
        home.path(),
        &[
            "fetch",
            "acme",
            "--dry-run",
            "--config",
            config.to_str().unwrap(),
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success());
    assert!(stdout.contains("ERROR:"), "{stdout}");
    assert!(stdout.contains("docsweep-test-absent"), "{stdout}");
    assert!(!home.path().join("invoices").exists());
}

#[test]
fn help_lists_the_commands() {
    let home = tempfile::tempdir().unwrap();
    let output = docsweep(home.path(), &["--help"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("mail-scan"), "{stdout}");
    assert!(stdout.contains("secret"), "{stdout}");
    assert!(stdout.contains("fetch"), "{stdout}");
    assert!(stdout.contains("test-recipe"), "{stdout}");
}
