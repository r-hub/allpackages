//! DCF (Debian Control File) handling for CRAN-style PACKAGES metadata.
//!
//! Serialization is delegated to the `deb822-fast` crate; this module adds the
//! CRAN-specific bits: a thin [`Record`] wrapper used to build new records. A
//! DCF stream is a sequence of records separated by blank lines. We only ever
//! *append* freshly built records to the existing metadata, never reparse or
//! rewrite it, so this module is write-only.

use deb822_fast::Paragraph;

/// A single package record (one deb822 paragraph). Field order is preserved.
#[derive(Debug, Clone)]
pub struct Record(Paragraph);

impl Default for Record {
    fn default() -> Self {
        Record(Paragraph { fields: Vec::new() })
    }
}

impl Record {
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.0.set(key, &value.into());
    }
}

/// Serialize records to DCF text, paragraphs separated by a blank line
/// (matching `deb822_fast::Deb822`'s own formatting).
pub fn write(records: &[Record]) -> String {
    let mut out = String::new();
    for (i, rec) in records.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        // `Paragraph`'s Display emits each field line-terminated, so the
        // paragraph text already ends in '\n'; the push above is the
        // blank-line separator between paragraphs.
        out.push_str(&rec.0.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(fields: &[(&str, &str)]) -> Record {
        let mut rec = Record::default();
        for (k, v) in fields {
            rec.set(k, *v);
        }
        rec
    }

    #[test]
    fn write_emits_fields_in_set_order() {
        let rec = record(&[("Package", "zoo"), ("Version", "1.8-12")]);
        assert_eq!(
            write(std::slice::from_ref(&rec)),
            "Package: zoo\nVersion: 1.8-12\n"
        );
    }

    #[test]
    fn write_separates_paragraphs_with_a_blank_line() {
        let a = record(&[("Package", "abc"), ("Version", "2.2.1")]);
        let b = record(&[("Package", "zoo"), ("Version", "1.8-12")]);
        assert_eq!(
            write(&[a, b]),
            "Package: abc\nVersion: 2.2.1\n\nPackage: zoo\nVersion: 1.8-12\n"
        );
    }

    #[test]
    fn write_of_no_records_is_empty() {
        assert_eq!(write(&[]), "");
    }
}
