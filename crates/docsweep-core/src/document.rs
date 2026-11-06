//! Reads invoice facts from PDF files: ZUGFeRD/Factur-X XML first, text second.
//!
//! PDFs come from mails and are untrusted. Parsers may panic on malformed files,
//! so every extraction runs behind `catch_unwind`, and a failure just means that
//! the facts are unknown. XML entities are not expanded by `quick-xml`.

use std::panic::{self, AssertUnwindSafe};
use std::sync::LazyLock;

use chrono::{Datelike, NaiveDate};
use lopdf::{Dictionary, Document, Object};
use quick_xml::Reader;
use quick_xml::events::Event;
use regex::{Regex, RegexBuilder};
use serde::Serialize;

/// Names under which hybrid invoices embed their XML.
const EMBEDDED_XML_NAMES: &[&str] = &[
    "factur-x.xml",
    "zugferd-invoice.xml",
    "xrechnung.xml",
    "zugferd.xml",
];

/// What could be read from a document. Every field may be unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct InvoiceFacts {
    pub invoice_no: Option<String>,
    pub date: Option<NaiveDate>,
    /// Decimal text exactly as written in the XML; only set from structured data.
    pub amount: Option<String>,
    pub currency: Option<String>,
}

impl InvoiceFacts {
    fn fill_gaps_from(mut self, other: Self) -> Self {
        self.invoice_no = self.invoice_no.or(other.invoice_no);
        self.date = self.date.or(other.date);
        self.amount = self.amount.or(other.amount);
        self.currency = self.currency.or(other.currency);
        self
    }
}

/// Extracts the facts of a PDF. Never panics; unreadable files yield empty facts.
pub fn extract_facts(pdf: &[u8]) -> InvoiceFacts {
    let from_xml = guarded(|| {
        embedded_invoice_xml(pdf).map_or_else(Default::default, |x| parse_invoice_xml(&x))
    });
    if from_xml.invoice_no.is_some() && from_xml.date.is_some() {
        return from_xml;
    }
    let from_text = guarded(|| {
        pdf_extract::extract_text_from_mem(pdf)
            .map(|text| parse_invoice_text(&text))
            .unwrap_or_default()
    });
    from_xml.fill_gaps_from(from_text)
}

fn guarded(extract: impl FnOnce() -> InvoiceFacts) -> InvoiceFacts {
    panic::catch_unwind(AssertUnwindSafe(extract)).unwrap_or_default()
}

fn embedded_invoice_xml(pdf: &[u8]) -> Option<Vec<u8>> {
    let document = Document::load_mem(pdf).ok()?;
    document.objects.values().find_map(|object| {
        let dictionary = object.as_dict().ok()?;
        embedded_xml_of_file_spec(&document, dictionary)
    })
}

fn embedded_xml_of_file_spec(document: &Document, spec: &Dictionary) -> Option<Vec<u8>> {
    let name = [b"UF".as_slice(), b"F".as_slice()]
        .iter()
        .find_map(|key| spec.get(key).ok()?.as_str().ok())?;
    let name = String::from_utf8_lossy(name).to_ascii_lowercase();
    if !EMBEDDED_XML_NAMES.contains(&name.as_str()) {
        return None;
    }
    let embedded = resolve(document, spec.get(b"EF").ok()?)?.as_dict().ok()?;
    let reference = embedded.get(b"F").or_else(|_| embedded.get(b"UF")).ok()?;
    let stream = resolve(document, reference)?.as_stream().ok()?;
    stream
        .decompressed_content()
        .ok()
        .or_else(|| Some(stream.content.clone()))
}

fn resolve<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Object> {
    match object {
        Object::Reference(id) => document.get_object(*id).ok(),
        direct => Some(direct),
    }
}

/// Reads CII invoices (ZUGFeRD 1 and 2, Factur-X, XRechnung CII).
fn parse_invoice_xml(xml: &[u8]) -> InvoiceFacts {
    let mut reader = Reader::from_reader(xml);
    let mut facts = InvoiceFacts::default();
    let mut path: Vec<String> = Vec::new();
    let mut buffer = Vec::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(start)) => {
                let name = start.local_name().as_ref().to_owned();
                if name == "GrandTotalAmount"
                    && let Some(currency) = start
                        .attributes()
                        .flatten()
                        .find(|a| a.key.local_name().as_ref() == "currencyID")
                {
                    facts.currency = Some(currency.value.trim().to_owned());
                }
                path.push(name);
            }
            Ok(Event::End(_)) => {
                path.pop();
            }
            Ok(Event::Text(text)) => {
                let value = text.as_ref().trim().to_owned();
                if !value.is_empty() {
                    apply_xml_value(&path, value, &mut facts);
                }
            }
            Ok(Event::Eof) | Err(_) => return facts,
            Ok(_) => {}
        }
        buffer.clear();
    }
}

fn apply_xml_value(path: &[String], value: String, facts: &mut InvoiceFacts) {
    let is_in = |ancestor: &str| path.iter().any(|segment| segment == ancestor);
    let in_header = is_in("ExchangedDocument") || is_in("HeaderExchangedDocument");
    match path.last().map(String::as_str) {
        Some("ID") if in_header && path.len() >= 2 && is_header(&path[path.len() - 2]) => {
            facts.invoice_no.get_or_insert(value);
        }
        Some("DateTimeString") if in_header && is_in("IssueDateTime") => {
            if let Ok(date) = NaiveDate::parse_from_str(&value, "%Y%m%d") {
                facts.date.get_or_insert(date);
            }
        }
        Some("GrandTotalAmount") => {
            facts.amount.get_or_insert(value);
        }
        Some("InvoiceCurrencyCode") => {
            facts.currency.get_or_insert(value);
        }
        _ => {}
    }
}

fn is_header(segment: &str) -> bool {
    segment == "ExchangedDocument" || segment == "HeaderExchangedDocument"
}

static NUMBER_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    case_insensitive(
        r"(?:rechnungs?\s*-?\s*(?:nummer|nr\.?|no\.?)|rechnung\s+nr\.?|belegnummer|beleg-nr\.?|invoice\s*(?:number|no\.?|nr\.?|\#))\s*[:#.]?\s*([A-Za-z0-9][A-Za-z0-9/_.\-]*)",
    )
});

static DATE_PATTERNS: LazyLock<[Regex; 2]> = LazyLock::new(|| {
    let date = r"(\d{1,2}\.\s?\d{1,2}\.\s?\d{2,4}|\d{4}-\d{2}-\d{2}|\d{1,2}/\d{1,2}/\d{4})";
    [
        case_insensitive(&format!(
            r"(?:rechnungsdatum|belegdatum|invoice\s+date)\s*[:.]?\s*{date}"
        )),
        case_insensitive(&format!(r"\b(?:datum|date)\s*[:.]?\s*{date}")),
    ]
});

fn case_insensitive(pattern: &str) -> Regex {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .expect("built-in pattern is valid")
}

fn parse_invoice_text(text: &str) -> InvoiceFacts {
    let invoice_no = NUMBER_PATTERN
        .captures_iter(text)
        .map(|captures| captures[1].trim_end_matches(['.', '-', '/']).to_owned())
        .find(|candidate| candidate.chars().any(|c| c.is_ascii_digit()));
    let date = DATE_PATTERNS.iter().find_map(|pattern| {
        pattern
            .captures_iter(text)
            .find_map(|captures| parse_date(&captures[1]))
    });
    InvoiceFacts {
        invoice_no,
        date,
        amount: None,
        currency: None,
    }
}

pub(crate) fn parse_date(value: &str) -> Option<NaiveDate> {
    let compact: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    // The plausibility check matters: `%Y` also reads the `26` of `03.11.26` as
    // the year 26, and only then does the two-digit format get its turn.
    ["%d.%m.%Y", "%Y-%m-%d", "%d/%m/%Y", "%d.%m.%y"]
        .iter()
        .filter_map(|format| NaiveDate::parse_from_str(&compact, format).ok())
        .find(|date| (2000..=2100).contains(&date.year()))
}

/// Whether the bytes look like a PDF. Viewers accept the marker within the first
/// kilobyte, so this does too.
pub fn looks_like_pdf(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(1024)]
        .windows(5)
        .any(|window| window == b"%PDF-")
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_pdf {
    //! Builds dummy PDFs at test time, so no invoice files are stored in the repo.

    use lopdf::content::{Content, Operation};
    use lopdf::{Dictionary, Document, Object, Stream, dictionary};

    /// A one-page PDF with the given lines of text.
    pub fn with_text(lines: &[&str]) -> Vec<u8> {
        save(build(lines, None))
    }

    /// A one-page PDF with text and an embedded XML file.
    pub fn with_xml(lines: &[&str], xml_name: &str, xml: &str) -> Vec<u8> {
        save(build(lines, Some((xml_name, xml))))
    }

    fn save(mut document: Document) -> Vec<u8> {
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    fn build(lines: &[&str], xml: Option<(&str, &str)>) -> Document {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });

        let mut operations = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![50.into(), 700.into()]),
            Operation::new("TL", vec![16.into()]),
        ];
        for line in lines {
            operations.push(Operation::new("Tj", vec![Object::string_literal(*line)]));
            operations.push(Operation::new("T*", vec![]));
        }
        operations.push(Operation::new("ET", vec![]));
        let content = Content { operations };
        let content_id = doc.add_object(Stream::new(Dictionary::new(), content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let mut catalog = dictionary! { "Type" => "Catalog", "Pages" => pages_id };

        if let Some((name, xml)) = xml {
            let file_id = doc.add_object(Stream::new(
                dictionary! { "Type" => "EmbeddedFile" },
                xml.as_bytes().to_vec(),
            ));
            let spec_id = doc.add_object(dictionary! {
                "Type" => "Filespec",
                "F" => Object::string_literal(name),
                "UF" => Object::string_literal(name),
                "EF" => dictionary! { "F" => file_id },
            });
            catalog.set("AF", vec![spec_id.into()]);
        }

        let catalog_id = doc.add_object(catalog);
        doc.trailer.set("Root", catalog_id);
        doc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZUGFERD_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rsm:CrossIndustryInvoice xmlns:rsm="urn:rsm" xmlns:ram="urn:ram" xmlns:udt="urn:udt">
  <rsm:ExchangedDocumentContext>
    <ram:GuidelineSpecifiedDocumentContextParameter>
      <ram:ID>urn:factur-x.eu:1p0:basic</ram:ID>
    </ram:GuidelineSpecifiedDocumentContextParameter>
  </rsm:ExchangedDocumentContext>
  <rsm:ExchangedDocument>
    <ram:ID>ZF-2026-0815</ram:ID>
    <ram:TypeCode>380</ram:TypeCode>
    <ram:IssueDateTime><udt:DateTimeString format="102">20261103</udt:DateTimeString></ram:IssueDateTime>
  </rsm:ExchangedDocument>
  <rsm:SupplyChainTradeTransaction>
    <ram:IncludedSupplyChainTradeLineItem>
      <ram:AssociatedDocumentLineDocument><ram:LineID>1</ram:LineID></ram:AssociatedDocumentLineDocument>
    </ram:IncludedSupplyChainTradeLineItem>
    <ram:ApplicableHeaderTradeSettlement>
      <ram:InvoiceCurrencyCode>EUR</ram:InvoiceCurrencyCode>
      <ram:SpecifiedTradeSettlementHeaderMonetarySummation>
        <ram:GrandTotalAmount>119.00</ram:GrandTotalAmount>
      </ram:SpecifiedTradeSettlementHeaderMonetarySummation>
    </ram:ApplicableHeaderTradeSettlement>
  </rsm:SupplyChainTradeTransaction>
</rsm:CrossIndustryInvoice>"#;

    fn date(y: i32, m: u32, d: u32) -> Option<NaiveDate> {
        NaiveDate::from_ymd_opt(y, m, d)
    }

    #[test]
    fn reads_facts_from_zugferd_xml() {
        let facts = parse_invoice_xml(ZUGFERD_XML.as_bytes());
        assert_eq!(facts.invoice_no.as_deref(), Some("ZF-2026-0815"));
        assert_eq!(facts.date, date(2026, 11, 3));
        assert_eq!(facts.amount.as_deref(), Some("119.00"));
        assert_eq!(facts.currency.as_deref(), Some("EUR"));
    }

    #[test]
    fn reads_zugferd_1_header_names() {
        let xml = r#"<x><rsm:HeaderExchangedDocument xmlns:rsm="u" xmlns:ram="v" xmlns:udt="w">
            <ram:ID>ZF1-77</ram:ID>
            <ram:IssueDateTime><udt:DateTimeString format="102">20200131</udt:DateTimeString></ram:IssueDateTime>
            </rsm:HeaderExchangedDocument></x>"#;
        let facts = parse_invoice_xml(xml.as_bytes());
        assert_eq!(facts.invoice_no.as_deref(), Some("ZF1-77"));
        assert_eq!(facts.date, date(2020, 1, 31));
    }

    #[test]
    fn embedded_xml_wins_over_text_in_a_real_pdf() {
        let pdf = test_pdf::with_xml(
            &["Rechnungsnummer: TEXT-1", "Rechnungsdatum: 01.01.2020"],
            "factur-x.xml",
            ZUGFERD_XML,
        );
        let facts = extract_facts(&pdf);
        assert_eq!(facts.invoice_no.as_deref(), Some("ZF-2026-0815"));
        assert_eq!(facts.date, date(2026, 11, 3));
        assert_eq!(facts.amount.as_deref(), Some("119.00"));
    }

    #[test]
    fn other_embedded_files_are_ignored() {
        let pdf = test_pdf::with_xml(
            &["Rechnungsnummer: TEXT-1"],
            "something-else.xml",
            ZUGFERD_XML,
        );
        assert_eq!(extract_facts(&pdf).invoice_no.as_deref(), Some("TEXT-1"));
    }

    #[test]
    fn reads_facts_from_text_in_a_real_pdf() {
        let pdf = test_pdf::with_text(&[
            "Beispiel GmbH",
            "Rechnungsnummer: 2026-1234",
            "Rechnungsdatum: 03.11.2026",
        ]);
        let facts = extract_facts(&pdf);
        assert_eq!(facts.invoice_no.as_deref(), Some("2026-1234"));
        assert_eq!(facts.date, date(2026, 11, 3));
        assert_eq!(facts.amount, None);
    }

    #[test]
    fn understands_common_number_labels() {
        for text in [
            "Rechnungsnummer: R-100",
            "Rechnungs-Nr. R-100",
            "Rechnung Nr. R-100",
            "RECHNUNGSNR R-100",
            "Belegnummer: R-100",
            "Invoice No. R-100",
            "Invoice number: R-100",
            "Invoice #R-100",
        ] {
            let facts = parse_invoice_text(text);
            assert_eq!(facts.invoice_no.as_deref(), Some("R-100"), "{text}");
        }
    }

    #[test]
    fn number_must_contain_a_digit() {
        let text = "Bitte Rechnungsnummer angeben. Rechnungsnummer: 4711.";
        assert_eq!(parse_invoice_text(text).invoice_no.as_deref(), Some("4711"));
        assert_eq!(
            parse_invoice_text("Rechnungsnummer angeben").invoice_no,
            None
        );
    }

    #[test]
    fn invoice_date_wins_over_other_dates() {
        let text = "Datum: 01.02.2026\nFällig am: 15.03.2026\nRechnungsdatum: 03.11.2026";
        assert_eq!(parse_invoice_text(text).date, date(2026, 11, 3));
    }

    #[test]
    fn unlabelled_dates_are_not_guessed() {
        assert_eq!(parse_invoice_text("Fällig am 15.03.2026").date, None);
    }

    #[test]
    fn understands_date_formats() {
        for (input, expected) in [
            ("Datum: 3.11.2026", date(2026, 11, 3)),
            ("Datum: 03.11.26", date(2026, 11, 3)),
            ("Date: 2026-11-03", date(2026, 11, 3)),
            ("Invoice date: 03/11/2026", date(2026, 11, 3)),
            ("Datum: 99.99.2026", None),
        ] {
            assert_eq!(parse_invoice_text(input).date, expected, "{input}");
        }
    }

    #[test]
    fn garbage_yields_empty_facts_instead_of_a_panic() {
        assert_eq!(extract_facts(b""), InvoiceFacts::default());
        assert_eq!(extract_facts(b"%PDF-1.7 garbage"), InvoiceFacts::default());
        assert_eq!(extract_facts(&[0xFF; 4096]), InvoiceFacts::default());
    }

    #[test]
    fn broken_xml_keeps_what_was_read() {
        let facts = parse_invoice_xml(b"<a><ExchangedDocument><ID>X-1</ID><broken");
        assert_eq!(facts.invoice_no.as_deref(), Some("X-1"));
    }

    #[test]
    fn recognises_pdf_marker() {
        assert!(looks_like_pdf(b"%PDF-1.7"));
        assert!(looks_like_pdf(b"\n\n%PDF-1.4"));
        assert!(!looks_like_pdf(b"<html>"));
        assert!(!looks_like_pdf(b""));
    }
}
