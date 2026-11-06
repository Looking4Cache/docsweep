//! Which hosts a recipe may talk to.
//!
//! This is the one check for recipes that come from somewhere other than the
//! person using them: what a language model wrote now, shared recipes later. A
//! login types a password into whatever page the `goto` before it opened, so a
//! changed host is the one change that could send credentials elsewhere. It only
//! passes if a person saw the old and the new host and agreed.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use url::Url;

use super::recipe::{Recipe, Step};

/// The hosts a company's portal is known to be on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostBinding {
    hosts: BTreeSet<String>,
}

/// A step that would leave the known hosts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostChange {
    /// `goto` or `login`.
    pub step: String,
    /// The known hosts.
    pub old: String,
    pub new: String,
}

/// `host` or `host:port`, in the form that browsers use. Everything before an
/// `@` is not part of it, whatever an address tries to make it look like.
fn host_of(address: &str) -> Option<String> {
    let url = Url::parse(address).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_owned();
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

impl HostBinding {
    /// The hosts of a recipe that is already trusted, with parameters filled in.
    pub fn of_recipe(recipe: &Recipe) -> Self {
        let hosts = recipe
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Goto(address) => host_of(address),
                _ => None,
            })
            .collect();
        Self { hosts }
    }

    /// The host of an address that a person gave.
    pub fn of_address(address: &str) -> Self {
        Self {
            hosts: host_of(address).into_iter().collect(),
        }
    }

    /// These hosts and those of another binding.
    pub fn merged(mut self, other: Self) -> Self {
        self.hosts.extend(other.hosts);
        self
    }

    /// Whether an address is on one of these hosts.
    pub fn allows(&self, address: &str) -> bool {
        host_of(address).is_some_and(|host| self.hosts.contains(&host))
    }

    fn describe(&self) -> String {
        let hosts: Vec<&str> = self.hosts.iter().map(String::as_str).collect();
        hosts.join(", ")
    }

    /// What a recipe would change, with its own parameters filled in. A login
    /// without an address before it counts too, because nobody can tell where it
    /// would type the password.
    pub fn changes_in(&self, recipe: &Recipe) -> Vec<HostChange> {
        const UNKNOWN: &str = "(unknown)";
        let filled = recipe.with_params(&BTreeMap::new());
        let steps = filled
            .as_ref()
            .map_or(&recipe.steps, |filled| &filled.steps);

        let mut changes: Vec<HostChange> = Vec::new();
        let mut note = |step: &str, new: &str| {
            let change = HostChange {
                step: step.to_owned(),
                old: self.describe(),
                new: new.to_owned(),
            };
            if !changes.contains(&change) {
                changes.push(change);
            }
        };
        let mut current: Option<String> = None;
        for step in steps {
            match step {
                Step::Goto(address) => {
                    current = host_of(address);
                    match &current {
                        Some(host) if self.hosts.contains(host) => {}
                        Some(host) => note("goto", host),
                        None => note("goto", UNKNOWN),
                    }
                }
                Step::Login(_) => match &current {
                    Some(host) if self.hosts.contains(host) => {}
                    Some(host) => note("login", host),
                    None => note("login", UNKNOWN),
                },
                _ => {}
            }
        }
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(yaml: &str) -> Recipe {
        Recipe::from_yaml(yaml).unwrap()
    }

    const TRUSTED: &str = r##"
steps:
  - goto: https://portal.test/login
  - login: { user: "#u", password: "#p", submit: "#s" }
"##;

    #[test]
    fn the_same_hosts_change_nothing() {
        let binding = HostBinding::of_recipe(&recipe(TRUSTED));
        assert!(binding.changes_in(&recipe(TRUSTED)).is_empty());
        let other_path = TRUSTED.replace("/login", "/anmelden");
        assert!(binding.changes_in(&recipe(&other_path)).is_empty());
    }

    #[test]
    fn a_new_host_names_goto_and_login_with_old_and_new() {
        let binding = HostBinding::of_recipe(&recipe(TRUSTED));
        let moved = TRUSTED.replace("portal.test", "evil.example");
        assert_eq!(
            binding.changes_in(&recipe(&moved)),
            [
                HostChange {
                    step: "goto".into(),
                    old: "portal.test".into(),
                    new: "evil.example".into(),
                },
                HostChange {
                    step: "login".into(),
                    old: "portal.test".into(),
                    new: "evil.example".into(),
                },
            ]
        );
    }

    #[test]
    fn tricks_with_addresses_do_not_pass() {
        let binding = HostBinding::of_address("https://portal.test/");
        for address in [
            "https://portal.test@evil.example/login",
            "https://portal.test.evil.example/login",
            "https://sub.portal.test/login",
            "https://portal.test:8443/login",
        ] {
            let changed = TRUSTED.replace("https://portal.test/login", address);
            assert!(
                !binding.changes_in(&recipe(&changed)).is_empty(),
                "{address}"
            );
            assert!(!binding.allows(address), "{address}");
        }
        assert!(binding.allows("HTTPS://PORTAL.test/x?y=z"));
        assert!(!binding.allows("file:///etc/passwd"));
    }

    #[test]
    fn parameters_cannot_hide_a_host() {
        let binding = HostBinding::of_address("https://portal.test/");
        let hidden = recipe(
            r#"
params: { domain: evil.example }
steps: [ { goto: "https://{domain}/login" } ]
"#,
        );
        assert_eq!(binding.changes_in(&hidden)[0].new, "evil.example");
    }

    #[test]
    fn a_login_without_an_address_before_it_counts_as_a_change() {
        let binding = HostBinding::of_address("https://portal.test/");
        let nowhere =
            recipe(r##"steps: [ { login: { user: "#u", password: "#p", submit: "#s" } } ]"##);
        let changes = binding.changes_in(&nowhere);
        assert_eq!(changes[0].step, "login");
        assert_eq!(changes[0].new, "(unknown)");
    }
}
