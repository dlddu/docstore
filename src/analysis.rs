//! Text analysis: a small approximation of OpenSearch's `standard` analyzer.
//!
//! Tokens are maximal runs of alphanumeric characters (any script, so Hangul
//! and Latin behave alike) plus `_`, lower-cased. A `.` or `,` sitting between
//! two digits is kept inside the token so version numbers like `1.16.1` stay
//! one term, the way UAX#29 word segmentation treats them.

use serde_json::Value;

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

/// Analyzed form of one document: every scalar value becomes its own field
/// (so phrases never match across two different values), plus the total term
/// count used for BM25 length normalisation.
#[derive(Debug, Clone, Default)]
pub struct AnalyzedDoc {
    pub fields: Vec<Vec<String>>,
    pub length: usize,
}

impl AnalyzedDoc {
    pub fn from_source(source: &Value) -> Self {
        let mut fields = Vec::new();
        collect(source, &mut fields);
        let length = fields.iter().map(Vec::len).sum();
        AnalyzedDoc { fields, length }
    }

    pub fn terms(&self) -> impl Iterator<Item = &String> {
        self.fields.iter().flatten()
    }
}

fn collect(value: &Value, out: &mut Vec<Vec<String>>) {
    match value {
        Value::String(s) => push(out, tokenize(s)),
        Value::Number(n) => push(out, tokenize(&n.to_string())),
        Value::Bool(b) => push(out, vec![b.to_string()]),
        Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect(v, out)),
        Value::Null => {}
    }
}

fn push(out: &mut Vec<Vec<String>>, tokens: Vec<String>) {
    if !tokens.is_empty() {
        out.push(tokens);
    }
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
    fn analyzes_nested_values_into_separate_fields() {
        let doc = AnalyzedDoc::from_source(&json!({
            "title": "Hello World",
            "tags": ["a", "b c"],
            "n": 42,
            "nested": {"ok": true, "none": null}
        }));
        assert_eq!(
            doc.fields,
            vec![
                vec!["hello", "world"],
                vec!["a"],
                vec!["b", "c"],
                vec!["42"],
                vec!["true"],
            ]
        );
        assert_eq!(doc.length, 7);
    }
}
