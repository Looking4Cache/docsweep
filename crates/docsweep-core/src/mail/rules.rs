//! Matches mails to companies: sender domain, subject and attachment patterns.

use regex::{Regex, RegexBuilder};

use super::IncomingMail;
use crate::config::{Company, Config};

/// A company rule with compiled patterns.
#[derive(Debug)]
struct CompiledRule<'a> {
    company: &'a Company,
    domain: String,
    subject: Option<Regex>,
    attachment: Option<Regex>,
}

/// All rules of a configuration, in configuration order.
#[derive(Debug)]
pub struct RuleSet<'a> {
    rules: Vec<CompiledRule<'a>>,
    invoice_hint: Regex,
}

/// A company whose rule matches, with the PDFs that count as its documents.
#[derive(Debug, Clone, Copy)]
pub struct RuleMatch<'a> {
    pub company: &'a Company,
}

impl<'a> RuleSet<'a> {
    /// Compiles the rules. Patterns are valid because loading validated them.
    pub fn new(config: &'a Config) -> Self {
        let rules = config
            .companies
            .iter()
            .filter_map(|company| {
                let rule = company.mail.as_ref()?;
                Some(CompiledRule {
                    company,
                    domain: rule.from_domain.to_ascii_lowercase(),
                    subject: rule.subject.as_deref().map(validated_regex),
                    attachment: rule.attachment.as_deref().map(validated_regex),
                })
            })
            .collect();
        let invoice_hint = RegexBuilder::new(r"rechnung|invoice")
            .case_insensitive(true)
            .build()
            .expect("built-in pattern is valid");
        Self {
            rules,
            invoice_hint,
        }
    }

    /// Decides from the headers alone whether the full mail is worth fetching.
    pub fn is_candidate(&self, headers: &IncomingMail) -> bool {
        self.invoice_hint.is_match(&headers.subject)
            || self.rules.iter().any(|rule| rule.matches_headers(headers))
    }

    /// The first company whose whole rule matches.
    pub fn find(&self, mail: &IncomingMail) -> Option<RuleMatch<'a>> {
        self.rules
            .iter()
            .find(|rule| rule.matches_headers(mail) && rule.matches_attachments(mail))
            .map(|rule| RuleMatch {
                company: rule.company,
            })
    }

    /// The indices of the attachments a company's rule treats as invoices.
    pub fn documents_of(&self, company: &Company, mail: &IncomingMail) -> Vec<usize> {
        let pattern = self
            .rules
            .iter()
            .find(|rule| rule.company.id == company.id)
            .and_then(|rule| rule.attachment.as_ref());
        mail.attachments
            .iter()
            .enumerate()
            .filter(|(_, attachment)| attachment.is_pdf())
            .filter(|(_, attachment)| pattern.is_none_or(|p| p.is_match(&attachment.name)))
            .map(|(position, _)| position)
            .collect()
    }

    /// Whether an unmatched mail looks like an invoice worth pointing out.
    pub fn looks_like_unknown_invoice(&self, mail: &IncomingMail) -> bool {
        let pdfs = || mail.attachments.iter().filter(|a| a.is_pdf());
        pdfs().next().is_some()
            && (self.invoice_hint.is_match(&mail.subject)
                || pdfs().any(|a| self.invoice_hint.is_match(&a.name)))
    }
}

impl CompiledRule<'_> {
    fn matches_headers(&self, mail: &IncomingMail) -> bool {
        let domain_matches = mail
            .from_domain
            .as_deref()
            .is_some_and(|domain| is_same_or_subdomain(domain, &self.domain));
        let subject_matches = self
            .subject
            .as_ref()
            .is_none_or(|pattern| pattern.is_match(&mail.subject));
        domain_matches && subject_matches
    }

    fn matches_attachments(&self, mail: &IncomingMail) -> bool {
        self.attachment.as_ref().is_none_or(|pattern| {
            mail.attachments
                .iter()
                .any(|attachment| pattern.is_match(&attachment.name))
        })
    }
}

fn is_same_or_subdomain(domain: &str, parent: &str) -> bool {
    domain == parent
        || domain
            .strip_suffix(parent)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn validated_regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("patterns are validated when the configuration is loaded")
}

#[cfg(test)]
mod tests {
    use super::super::Attachment;
    use super::*;

    fn config() -> Config {
        Config::from_yaml(
            r#"
mail:
  accounts:
    - { id: a, imap: "i.example:993", user: u, password: "keyring:p", watch: INBOX }
  disposal: { mode: all_delete }
companies:
  - id: telekom
    name: Telekom
    mail: { from_domain: telekom.de, subject: "(?i)rechnung" }
    output: { dir: /x, filename: "{invoice_no}.pdf" }
  - id: hetzner
    name: Hetzner
    mail: { from_domain: hetzner.com, attachment: "(?i).*\\.pdf" }
    output: { dir: /x, filename: "{invoice_no}.pdf" }
  - id: first
    name: First
    mail: { from_domain: shared.example }
    output: { dir: /x, filename: "{invoice_no}.pdf" }
  - id: second
    name: Second
    mail: { from_domain: shared.example }
    output: { dir: /x, filename: "{invoice_no}.pdf" }
"#,
        )
        .unwrap()
    }

    fn mail(domain: &str, subject: &str, attachments: &[&str]) -> IncomingMail {
        IncomingMail {
            uid: 1,
            message_id: "<1>".into(),
            from: format!("billing@{domain}"),
            from_domain: Some(domain.into()),
            subject: subject.into(),
            date: None,
            attachments: attachments
                .iter()
                .map(|name| Attachment {
                    name: (*name).into(),
                    bytes: b"%PDF-1.7".to_vec(),
                })
                .collect(),
        }
    }

    fn found(mail: &IncomingMail) -> Option<String> {
        let config = config();
        let rules = RuleSet::new(&config);
        rules.find(mail).map(|m| m.company.id.clone())
    }

    #[test]
    fn all_conditions_must_match() {
        assert_eq!(
            found(&mail("telekom.de", "Ihre RECHNUNG", &[])).as_deref(),
            Some("telekom")
        );
        assert_eq!(found(&mail("telekom.de", "Newsletter", &[])), None);
        assert_eq!(found(&mail("other.de", "Rechnung", &[])), None);
    }

    #[test]
    fn subdomains_match_but_lookalikes_do_not() {
        assert!(found(&mail("mail.telekom.de", "Rechnung", &[])).is_some());
        assert_eq!(found(&mail("nottelekom.de", "Rechnung", &[])), None);
        assert_eq!(
            found(&mail("telekom.de.evil.example", "Rechnung", &[])),
            None
        );
    }

    #[test]
    fn attachment_pattern_must_match_an_attachment() {
        assert_eq!(
            found(&mail("hetzner.com", "x", &["a.PDF"])).as_deref(),
            Some("hetzner")
        );
        assert_eq!(found(&mail("hetzner.com", "x", &[])), None);
        assert_eq!(found(&mail("hetzner.com", "x", &["a.txt"])), None);
    }

    #[test]
    fn first_company_in_configuration_order_wins() {
        assert_eq!(
            found(&mail("shared.example", "x", &[])).as_deref(),
            Some("first")
        );
    }

    #[test]
    fn missing_sender_never_matches() {
        let mut m = mail("telekom.de", "Rechnung", &[]);
        m.from_domain = None;
        assert_eq!(found(&m), None);
    }

    #[test]
    fn documents_respect_the_attachment_pattern_and_require_pdfs() {
        let config = config();
        let rules = RuleSet::new(&config);
        let mut m = mail("hetzner.com", "x", &["a.pdf", "agb.txt", "b.PDF"]);
        m.attachments[2].bytes = b"not a pdf at all".to_vec();
        let hetzner = &config.companies[1];
        assert_eq!(rules.documents_of(hetzner, &m), [0]);
    }

    #[test]
    fn unknown_invoice_needs_a_pdf_and_a_hint() {
        let config = config();
        let rules = RuleSet::new(&config);
        assert!(rules.looks_like_unknown_invoice(&mail("x.de", "Ihre Rechnung", &["a.pdf"])));
        assert!(rules.looks_like_unknown_invoice(&mail("x.de", "Post", &["Invoice-1.pdf"])));
        assert!(!rules.looks_like_unknown_invoice(&mail("x.de", "Ihre Rechnung", &[])));
        assert!(!rules.looks_like_unknown_invoice(&mail("x.de", "Foto", &["a.pdf"])));
    }

    #[test]
    fn candidates_are_found_from_headers() {
        let config = config();
        let rules = RuleSet::new(&config);
        assert!(rules.is_candidate(&mail("hetzner.com", "x", &[])));
        assert!(rules.is_candidate(&mail("x.de", "Ihre Rechnung", &[])));
        assert!(!rules.is_candidate(&mail("x.de", "Urlaub", &[])));
    }
}
