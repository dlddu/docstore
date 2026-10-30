//! Text analysis: a small approximation of how OpenSearch's default dynamic
//! mapping indexes a JSON document.
//!
//! Every string becomes a `text` field analyzed like the `standard` analyzer
//! and, when short, also a `<path>.keyword` field holding the exact value.
//! Numbers and booleans are exact-value fields. Strings that look like dates
//! are skipped, the way date detection maps them to `date` fields that
//! full-text queries leave alone.
//!
//! Tokens are maximal runs of alphanumeric characters (any script, so Hangul
//! and Latin behave alike) plus `_`, lower-cased. A `.` or `,` sitting between
//! two digits is kept inside the token so version numbers like `1.16.1` stay
//! one term, the way UAX#29 word segmentation treats them.

use serde_json::Value;

/// OpenSearch's dynamic mapping gives strings a keyword sub-field with
/// `ignore_above: 256`.
const KEYWORD_IGNORE_ABOVE: usize = 256;

/// Splits `text` into lower-cased terms.
pub fn tokenize(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut current = String::new();

    for (i, &c) in chars.iter().enumerate() {
        if is_word_char(c) {
            current.extend(c.to_lowercase());
            continue;
        }
        let joins_digits = (c == '.' || c == ',')
            && !current.is_empty()
            && i > 0
            && chars[i - 1].is_ascii_digit()
            && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit());
        if joins_digits {
            current.push(c);
            continue;
        }
        if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A `text` field: every value at one path (array elements included), each
/// value tokenized separately so a phrase never spans two values.
#[derive(Debug, Clone, PartialEq)]
pub struct TextField {
    pub path: String,
    pub values: Vec<Vec<String>>,
    /// Total token count, the field length BM25 normalizes by.
    pub len: usize,
}

/// An exact-value field (`keyword`, number, boolean): matched only by a query
/// term equal to a whole value, and scored without length normalization.
#[derive(Debug, Clone, PartialEq)]
pub struct ExactField {
    pub path: String,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AnalyzedDoc {
    pub text: Vec<TextField>,
    pub exact: Vec<ExactField>,
}

impl AnalyzedDoc {
    pub fn from_source(source: &Value) -> Self {
        let mut doc = AnalyzedDoc::default();
        doc.collect(source, "");
        doc
    }

    fn collect(&mut self, value: &Value, path: &str) {
        match value {
            Value::String(s) => {
                if looks_like_date(s) {
                    return;
                }
                let tokens = tokenize(s);
                if !tokens.is_empty() {
                    let field = text_field(&mut self.text, path);
                    field.len += tokens.len();
                    field.values.push(tokens);
                }
                if s.chars().count() <= KEYWORD_IGNORE_ABOVE {
                    exact_field(&mut self.exact, &format!("{path}.keyword")).push(s.clone());
                }
            }
            Value::Number(n) => exact_field(&mut self.exact, path).push(n.to_string()),
            Value::Bool(b) => exact_field(&mut self.exact, path).push(b.to_string()),
            Value::Array(items) => items.iter().for_each(|v| self.collect(v, path)),
            Value::Object(map) => {
                for (key, v) in map {
                    let child = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    self.collect(v, &child);
                }
            }
            Value::Null => {}
        }
    }
}

fn text_field<'a>(fields: &'a mut Vec<TextField>, path: &str) -> &'a mut TextField {
    match fields.iter().position(|f| f.path == path) {
        Some(i) => &mut fields[i],
        None => {
            fields.push(TextField {
                path: path.to_owned(),
                values: Vec::new(),
                len: 0,
            });
            fields.last_mut().expect("just pushed")
        }
    }
}

fn exact_field<'a>(fields: &'a mut Vec<ExactField>, path: &str) -> &'a mut Vec<String> {
    let i = match fields.iter().position(|f| f.path == path) {
        Some(i) => i,
        None => {
            fields.push(ExactField {
                path: path.to_owned(),
                values: Vec::new(),
            });
            fields.len() - 1
        }
    };
    &mut fields[i].values
}

/// The shapes OpenSearch's default `dynamic_date_formats` detect:
/// `strict_date_optional_time` (`2026-10-30`, `2026-10-30T14:00:00+09:00`, …)
/// and `yyyy/MM/dd[ HH:mm:ss Z]`.
fn looks_like_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 10 {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    let sep = b[4];
    // Check the ASCII date prefix before slicing: only then is byte 10 a
    // char boundary (the rest may be any UTF-8).
    if !(digits(0..4)
        && digits(5..7)
        && digits(8..10)
        && (sep == b'-' || sep == b'/')
        && b[7] == sep)
    {
        return false;
    }
    let rest = &s[10..];
    if rest.is_empty() {
        return true;
    }
    if sep == b'-' {
        return is_time_suffix(rest);
    }
    let r = rest.as_bytes();
    r.len() >= 9
        && r[0] == b' '
        && r[1..]
            .iter()
            .all(|c| c.is_ascii_digit() || b": +-".contains(c))
}

/// `THH[:mm[:ss[.fff]]][Z|±HH[:mm]]`
fn is_time_suffix(rest: &str) -> bool {
    let r = rest.as_bytes();
    if r[0] != b'T' || r.len() < 3 || !r[1..3].iter().all(u8::is_ascii_digit) {
        return false;
    }
    r[3..]
        .iter()
        .all(|c| c.is_ascii_digit() || b":.+-Z".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn splits_on_punctuation_and_lowercases() {
        assert_eq!(
            tokenize("Sablier scale-to-zero, K3s!"),
            ["sablier", "scale", "to", "zero", "k3s"]
        );
    }

    #[test]
    fn keeps_underscores_and_version_numbers() {
        assert_eq!(
            tokenize("opensearch_search v1.16.1 ends."),
            ["opensearch_search", "v1.16.1", "ends"]
        );
        assert_eq!(tokenize("1,000 docs"), ["1,000", "docs"]);
    }

    #[test]
    fn handles_hangul_as_words() {
        assert_eq!(
            tokenize("작업기록: 홈랩 k3s 배포"),
            ["작업기록", "홈랩", "k3s", "배포"]
        );
    }

    #[test]
    fn maps_values_to_fields_like_dynamic_mapping() {
        let doc = AnalyzedDoc::from_source(&json!({
            "title": "Hello World",
            "tags": ["a", "b c"],
            "n": 42,
            "nested": {"ok": true, "none": null, "deep": "x"},
            "created_at": "2026-10-30T14:00:00+09:00"
        }));
        let text: Vec<(&str, usize)> = doc.text.iter().map(|f| (f.path.as_str(), f.len)).collect();
        assert_eq!(text, [("title", 2), ("tags", 3), ("nested.deep", 1)]);
        assert_eq!(doc.text[1].values, vec![vec!["a"], vec!["b", "c"]]);
        let exact: Vec<(&str, Vec<&str>)> = doc
            .exact
            .iter()
            .map(|f| {
                (
                    f.path.as_str(),
                    f.values.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            exact,
            [
                ("title.keyword", vec!["Hello World"]),
                ("tags.keyword", vec!["a", "b c"]),
                ("n", vec!["42"]),
                ("nested.ok", vec!["true"]),
                ("nested.deep.keyword", vec!["x"]),
            ]
        );
    }

    #[test]
    fn long_strings_get_no_keyword_field() {
        let doc = AnalyzedDoc::from_source(&json!({"body": "word ".repeat(60)}));
        assert_eq!(doc.text[0].len, 60);
        assert!(doc.exact.is_empty());
    }

    #[test]
    fn detects_dates_like_opensearch() {
        for s in [
            "2026-10-30",
            "2026-10-30T14:00:00+09:00",
            "2026-07-31T00:00:00Z",
            "2026-10-30T14:00:00.123Z",
            "2026/10/30",
            "2026/10/30 14:00:00 +0900",
        ] {
            assert!(looks_like_date(s), "{s} should be a date");
        }
        for s in [
            "2026-10-30 release notes",
            "v1.16.1",
            "20261030-sablier",
            "2026",
            "2026-10",
        ] {
            assert!(!looks_like_date(s), "{s} should not be a date");
        }
    }
}
