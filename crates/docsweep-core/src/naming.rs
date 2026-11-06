//! File name templates such as `{date:%Y-%m-%d} - Telekom - {invoice_no}.pdf`.

use std::fmt::Write;

use chrono::NaiveDate;

const FORBIDDEN_IN_FILENAME: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
const MAX_INVOICE_NO_CHARS: usize = 100;

/// A parsed and validated file name template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilenameTemplate {
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Literal(String),
    Date(String),
    InvoiceNo,
}

impl FilenameTemplate {
    /// Parses a template. The error is a readable description of the problem.
    pub fn parse(template: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut rest = template;
        while let Some(start) = rest.find('{') {
            if start > 0 {
                parts.push(Part::Literal(rest[..start].to_owned()));
            }
            let after = &rest[start + 1..];
            let end = after
                .find('}')
                .ok_or_else(|| "has a `{` without a closing `}`".to_owned())?;
            parts.push(parse_placeholder(&after[..end])?);
            rest = &after[end + 1..];
        }
        if !rest.is_empty() {
            parts.push(Part::Literal(rest.to_owned()));
        }

        let result = Self { parts };
        let sample = result.try_render(sample_date(), "1")?;
        if sample.contains(FORBIDDEN_IN_FILENAME) {
            return Err(format!(
                "must not produce any of these characters: {}",
                FORBIDDEN_IN_FILENAME.iter().collect::<String>()
            ));
        }
        if !sample.to_ascii_lowercase().ends_with(".pdf") {
            return Err("must end with `.pdf`".to_owned());
        }
        Ok(result)
    }

    /// Renders a file name. The invoice number is made safe for file systems.
    pub fn render(&self, date: NaiveDate, invoice_no: &str) -> String {
        self.try_render(date, &sanitize_invoice_no(invoice_no))
            .expect("date formats are checked when the template is parsed")
    }

    fn try_render(&self, date: NaiveDate, invoice_no: &str) -> Result<String, String> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(text) => out.push_str(text),
                Part::InvoiceNo => out.push_str(invoice_no),
                // `Display` of a date format fails on time fields such as `%H`,
                // and `to_string` would panic on that.
                Part::Date(format) => write!(out, "{}", date.format(format))
                    .map_err(|_| format!("has an unusable date format `{format}`"))?,
            }
        }
        Ok(out)
    }
}

fn parse_placeholder(placeholder: &str) -> Result<Part, String> {
    match placeholder.split_once(':') {
        None if placeholder == "invoice_no" => Ok(Part::InvoiceNo),
        None if placeholder == "date" => Ok(Part::Date("%Y-%m-%d".to_owned())),
        Some(("date", format)) => Ok(Part::Date(format.to_owned())),
        _ => Err(format!(
            "has an unknown placeholder `{{{placeholder}}}`; available are `{{date:<format>}}` and `{{invoice_no}}`"
        )),
    }
}

fn sample_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 1, 31).expect("valid date")
}

/// Makes text from an untrusted PDF safe to use inside a file name.
///
/// Separators and other reserved characters become `-`, so a crafted invoice
/// number cannot leave the target directory.
pub fn sanitize_invoice_no(invoice_no: &str) -> String {
    let cleaned: String = invoice_no
        .chars()
        .map(|c| {
            if FORBIDDEN_IN_FILENAME.contains(&c) || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .take(MAX_INVOICE_NO_CHARS)
        .collect();
    let cleaned = cleaned.replace("..", "-");
    cleaned
        .trim_matches(|c: char| c == '.' || c.is_whitespace())
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 3).unwrap()
    }

    #[test]
    fn renders_the_documented_schema() {
        let template =
            FilenameTemplate::parse("{date:%Y-%m-%d} - Telekom - {invoice_no}.pdf").unwrap();
        assert_eq!(
            template.render(date(), "R-123"),
            "2026-11-03 - Telekom - R-123.pdf"
        );
    }

    #[test]
    fn rejects_bad_templates() {
        for (template, expected) in [
            ("{nope}.pdf", "unknown placeholder"),
            ("{date:%Y.pdf", "without a closing"),
            ("{date:%H}.pdf", "unusable date format"),
            ("{date:%Q}.pdf", "unusable date format"),
            ("{invoice_no}.txt", "must end with `.pdf`"),
            ("sub/{invoice_no}.pdf", "must not produce"),
            ("{date:%D}.pdf", "must not produce"),
        ] {
            let error = FilenameTemplate::parse(template).unwrap_err();
            assert!(error.contains(expected), "{template}: {error}");
        }
    }

    #[test]
    fn crafted_invoice_numbers_cannot_escape_the_directory() {
        let template = FilenameTemplate::parse("{invoice_no}.pdf").unwrap();
        for evil in ["../../etc/passwd", "..\\..\\x", "a/b", "..", "C:\\x"] {
            let name = template.render(date(), evil);
            assert!(!name.contains('/') && !name.contains('\\'), "{name}");
            assert!(!name.contains(".."), "{name}");
        }
    }

    #[test]
    fn invoice_numbers_are_limited_and_cleaned() {
        assert_eq!(sanitize_invoice_no("  R\n1. "), "R-1");
        assert_eq!(
            sanitize_invoice_no(&"9".repeat(500)).len(),
            MAX_INVOICE_NO_CHARS
        );
    }
}
