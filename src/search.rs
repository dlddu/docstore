//! `simple_query_string` parsing and BM25 scoring.
//!
//! Scoring follows what OpenSearch does for `simple_query_string` over all
//! fields (`default_field: *`, `most_fields`): each query term is scored per
//! field with that field's own statistics (document count, average length,
//! document frequency) and the per-field scores are added. So a match in a
//! short, rare field such as a title or a tag weighs more than the same word
//! buried in a long body. Exact-value fields (`<path>.keyword`, numbers,
//! booleans) match a query term equal to a whole value, without length
//! normalization. BM25 is Lucene's (k1 = 1.2, b = 0.75, no `k1 + 1` factor).
//!
//! Supported syntax (a pragmatic subset of OpenSearch's):
//! - bare words are OR-ed together (the default operator);
//! - `+word` must match, `-word` must not match;
//! - `"a phrase"` matches the terms adjacently within one value;
//! - `word*` is a prefix query; a lone `*` matches every document;
//! - `|`, `(`, `)` and `~N` are accepted and ignored.
//!
//! Unlike OpenSearch, a query made only of `-word` clauses still matches
//! nothing unless something else does: a negation never turns into "match
//! everything else".

use std::collections::HashMap;

use crate::analysis::{tokenize, AnalyzedDoc, ExactField, TextField};

const K1: f64 = 1.2;
const B: f64 = 0.75;
/// Weight of an implicit prefix match on a non-ASCII term (Korean words carry
/// attached particles: `배포를` should still be found by `배포`).
const IMPLICIT_PREFIX_WEIGHT: f64 = 0.5;

#[derive(Debug, Clone, PartialEq)]
pub enum Matcher {
    /// One analyzed term; `prefix` is set for `word*`.
    Term { term: String, prefix: bool },
    /// Terms that must appear consecutively in one field.
    Phrase(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Copy)]
pub enum Occur {
    Should,
    Must,
    MustNot,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Clause {
    pub occur: Occur,
    /// Alternatives: a word like `foo-bar` analyzes into several terms, any
    /// of which may satisfy the clause.
    pub any_of: Vec<Matcher>,
    /// The clause text before analysis (operator, `*` and `~N` stripped),
    /// matched against exact-value fields, which are not analyzed.
    pub raw: String,
    /// `word*`: also a prefix query on exact-value fields.
    pub prefix: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    MatchAll,
    /// Matches nothing (e.g. the query string had no searchable terms).
    MatchNone,
    Clauses(Vec<Clause>),
}

impl Query {
    pub fn parse(input: &str) -> Query {
        let trimmed = input.trim();
        if trimmed == "*" {
            return Query::MatchAll;
        }
        let clauses: Vec<Clause> = lex(trimmed)
            .into_iter()
            .filter_map(|raw| raw.into_clause())
            .collect();
        if clauses.iter().all(|c| c.occur == Occur::MustNot) {
            return Query::MatchNone;
        }
        Query::Clauses(clauses)
    }
}

struct RawClause {
    occur: Occur,
    text: String,
    phrase: bool,
}

impl RawClause {
    fn into_clause(self) -> Option<Clause> {
        if self.phrase {
            let terms = tokenize(&self.text);
            let any_of = match terms.len() {
                0 => return None,
                1 => vec![Matcher::Term {
                    term: terms.into_iter().next().unwrap(),
                    prefix: false,
                }],
                _ => vec![Matcher::Phrase(terms)],
            };
            return Some(Clause {
                occur: self.occur,
                any_of,
                raw: self.text,
                prefix: false,
            });
        }
        let text = strip_fuzziness(&self.text);
        let prefix = text.ends_with('*');
        let raw = text.trim_end_matches('*').to_owned();
        let terms = tokenize(&raw);
        if terms.is_empty() {
            return None;
        }
        let last = terms.len() - 1;
        let any_of = terms
            .into_iter()
            .enumerate()
            .map(|(i, term)| Matcher::Term {
                term,
                prefix: prefix && i == last,
            })
            .collect();
        Some(Clause {
            occur: self.occur,
            any_of,
            raw,
            prefix,
        })
    }
}

/// `word~2` → `word` (fuzziness/slop is not supported, the operand is kept).
fn strip_fuzziness(text: &str) -> &str {
    match text.rfind('~') {
        Some(pos) if text[pos + 1..].chars().all(|c| c.is_ascii_digit()) => &text[..pos],
        _ => text,
    }
}

fn lex(input: &str) -> Vec<RawClause> {
    let mut out = Vec::new();
    let mut chars = input.chars().peekable();
    let mut pending = Occur::Should;

    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() || c == '|' || c == '(' || c == ')' => {
                chars.next();
            }
            '+' => {
                chars.next();
                pending = Occur::Must;
            }
            '-' => {
                chars.next();
                pending = Occur::MustNot;
            }
            '"' => {
                chars.next();
                let mut text = String::new();
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    text.push(c);
                }
                // A trailing `~N` slop after the closing quote is ignored.
                if chars.peek() == Some(&'~') {
                    chars.next();
                    while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                        chars.next();
                    }
                }
                out.push(RawClause {
                    occur: std::mem::replace(&mut pending, Occur::Should),
                    text,
                    phrase: true,
                });
            }
            _ => {
                let mut text = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || matches!(c, '|' | '(' | ')' | '"') {
                        break;
                    }
                    text.push(c);
                    chars.next();
                }
                out.push(RawClause {
                    occur: std::mem::replace(&mut pending, Occur::Should),
                    text,
                    phrase: false,
                });
            }
        }
    }
    out
}

/// Per-field collection statistics for BM25 over the searched documents.
#[derive(Default)]
struct FieldStats {
    /// Documents that have this field.
    docs: usize,
    /// Sum of the field's length over those documents (text fields only).
    total_len: usize,
}

impl FieldStats {
    fn avg_len(&self) -> f64 {
        if self.docs == 0 {
            1.0
        } else {
            (self.total_len as f64 / self.docs as f64).max(1.0)
        }
    }
}

/// Lucene's BM25 idf: `ln(1 + (N - df + 0.5) / (df + 0.5))`, N being the
/// documents that have the field.
fn idf(docs_with_field: usize, df: usize) -> f64 {
    let (n, df) = (docs_with_field as f64, df as f64);
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

pub struct Corpus<'a> {
    docs: Vec<&'a AnalyzedDoc>,
    text_stats: HashMap<&'a str, FieldStats>,
    exact_stats: HashMap<&'a str, FieldStats>,
}

impl<'a> Corpus<'a> {
    pub fn new(docs: Vec<&'a AnalyzedDoc>) -> Self {
        let mut text_stats: HashMap<&str, FieldStats> = HashMap::new();
        let mut exact_stats: HashMap<&str, FieldStats> = HashMap::new();
        for doc in &docs {
            for f in &doc.text {
                let st = text_stats.entry(f.path.as_str()).or_default();
                st.docs += 1;
                st.total_len += f.len;
            }
            for f in &doc.exact {
                exact_stats.entry(f.path.as_str()).or_default().docs += 1;
            }
        }
        Corpus {
            docs,
            text_stats,
            exact_stats,
        }
    }

    /// Scores every document of the corpus, returning `(position, score)` for
    /// the matching ones in corpus order.
    pub fn score_all(&self, query: &Query) -> Vec<(usize, f64)> {
        let clauses = match query {
            Query::MatchNone => return Vec::new(),
            Query::MatchAll => return (0..self.docs.len()).map(|i| (i, 1.0)).collect(),
            Query::Clauses(clauses) => clauses,
        };

        let mut total = vec![0.0; self.docs.len()];
        let mut alive = vec![true; self.docs.len()];
        let mut matched_positive = vec![false; self.docs.len()];

        for clause in clauses {
            let scores = self.clause_scores(clause);
            for (i, &sc) in scores.iter().enumerate() {
                let hit = sc > 0.0;
                match clause.occur {
                    Occur::MustNot if hit => alive[i] = false,
                    Occur::MustNot => {}
                    Occur::Must if !hit => alive[i] = false,
                    _ if hit => {
                        matched_positive[i] = true;
                        total[i] += sc;
                    }
                    _ => {}
                }
            }
        }

        (0..self.docs.len())
            .filter(|&i| alive[i] && matched_positive[i])
            .map(|i| (i, total[i]))
            .collect()
    }

    /// One clause's score in every document: the sum over its matchers and
    /// over every field they hit (`most_fields`).
    fn clause_scores(&self, clause: &Clause) -> Vec<f64> {
        let mut scores = vec![0.0; self.docs.len()];
        for matcher in &clause.any_of {
            self.add_text_scores(matcher, &mut scores);
        }
        self.add_exact_scores(clause, &mut scores);
        scores
    }

    fn add_text_scores(&self, matcher: &Matcher, scores: &mut [f64]) {
        // tf of this matcher per (doc, field), then df per field.
        let tfs: Vec<Vec<(&TextField, f64)>> = self
            .docs
            .iter()
            .map(|doc| {
                doc.text
                    .iter()
                    .map(|f| (f, text_frequency(f, matcher)))
                    .filter(|(_, tf)| *tf > 0.0)
                    .collect()
            })
            .collect();
        let mut df: HashMap<&str, usize> = HashMap::new();
        for fields in &tfs {
            for (f, _) in fields {
                *df.entry(f.path.as_str()).or_default() += 1;
            }
        }
        for (i, fields) in tfs.iter().enumerate() {
            for (f, tf) in fields {
                let st = &self.text_stats[f.path.as_str()];
                let norm = K1 * (1.0 - B + B * f.len as f64 / st.avg_len());
                scores[i] += idf(st.docs, df[f.path.as_str()]) * tf / (tf + norm);
            }
        }
    }

    fn add_exact_scores(&self, clause: &Clause, scores: &mut [f64]) {
        let tfs: Vec<Vec<(&ExactField, f64)>> = self
            .docs
            .iter()
            .map(|doc| {
                doc.exact
                    .iter()
                    .map(|f| (f, exact_frequency(f, &clause.raw, clause.prefix)))
                    .filter(|(_, tf)| *tf > 0.0)
                    .collect()
            })
            .collect();
        let mut df: HashMap<&str, usize> = HashMap::new();
        for fields in &tfs {
            for (f, _) in fields {
                *df.entry(f.path.as_str()).or_default() += 1;
            }
        }
        for (i, fields) in tfs.iter().enumerate() {
            for (f, tf) in fields {
                let st = &self.exact_stats[f.path.as_str()];
                // Exact-value fields carry no length norms.
                scores[i] += idf(st.docs, df[f.path.as_str()]) * tf / (tf + K1);
            }
        }
    }
}

/// Weighted term frequency of `matcher` in one text field.
fn text_frequency(field: &TextField, matcher: &Matcher) -> f64 {
    match matcher {
        Matcher::Term { term, prefix } => field
            .values
            .iter()
            .flatten()
            .map(|t| term_weight(t, term, *prefix))
            .sum(),
        Matcher::Phrase(terms) => field
            .values
            .iter()
            .map(|value| {
                value
                    .windows(terms.len())
                    .filter(|w| w.iter().zip(terms).all(|(a, b)| a == b))
                    .count()
            })
            .sum::<usize>() as f64,
    }
}

/// How many values of an exact-value field the raw clause text equals (or,
/// for `word*`, prefixes). Case-sensitive, like a `keyword` field.
fn exact_frequency(field: &ExactField, raw: &str, prefix: bool) -> f64 {
    if raw.is_empty() {
        return 0.0;
    }
    field
        .values
        .iter()
        .filter(|v| {
            if prefix {
                v.starts_with(raw)
            } else {
                v.as_str() == raw
            }
        })
        .count() as f64
}

fn term_weight(doc_term: &str, query_term: &str, explicit_prefix: bool) -> f64 {
    if doc_term == query_term {
        return 1.0;
    }
    if doc_term.starts_with(query_term) {
        if explicit_prefix {
            return 1.0;
        }
        if !query_term.is_ascii() {
            return IMPLICIT_PREFIX_WEIGHT;
        }
    }
    0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn term(t: &str) -> Matcher {
        Matcher::Term {
            term: t.into(),
            prefix: false,
        }
    }

    #[test]
    fn parses_operators() {
        let q = Query::parse(r#"alpha +beta -gamma "two words" pre* x~2 (y | z)"#);
        let Query::Clauses(c) = q else {
            panic!("expected clauses")
        };
        assert_eq!(c[0].occur, Occur::Should);
        assert_eq!(c[0].any_of, vec![term("alpha")]);
        assert_eq!(c[1].occur, Occur::Must);
        assert_eq!(c[2].occur, Occur::MustNot);
        assert_eq!(
            c[3].any_of,
            vec![Matcher::Phrase(vec!["two".into(), "words".into()])]
        );
        assert_eq!(
            c[4].any_of,
            vec![Matcher::Term {
                term: "pre".into(),
                prefix: true
            }]
        );
        assert_eq!(c[5].any_of, vec![term("x")]);
        assert_eq!(c[6].any_of, vec![term("y")]);
        assert_eq!(c[7].any_of, vec![term("z")]);
    }

    #[test]
    fn special_queries() {
        assert_eq!(Query::parse(" * "), Query::MatchAll);
        assert_eq!(Query::parse("-only -negations"), Query::MatchNone);
        assert_eq!(Query::parse("!!!"), Query::MatchNone);
    }

    #[test]
    fn hyphenated_word_matches_any_part() {
        let Query::Clauses(c) = Query::parse("scale-to-zero") else {
            panic!()
        };
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].any_of, vec![term("scale"), term("to"), term("zero")]);
    }

    fn docs() -> Vec<AnalyzedDoc> {
        [
            json!({"title": "sablier setup", "body": "scale to zero with traefik"}),
            json!({"title": "efs storage", "body": "sablier groups and efs"}),
            json!({"title": "unrelated", "body": "nothing here"}),
            json!({"title": "한국어 문서", "body": "배포를 완료했다"}),
        ]
        .iter()
        .map(AnalyzedDoc::from_source)
        .collect()
    }

    fn matches(query: &str) -> Vec<usize> {
        let docs = docs();
        let corpus = Corpus::new(docs.iter().collect());
        let mut hits = corpus.score_all(&Query::parse(query));
        hits.sort_by(|a, b| b.1.total_cmp(&a.1));
        hits.into_iter().map(|(i, _)| i).collect()
    }

    #[test]
    fn or_must_and_must_not() {
        assert_eq!(matches("traefik"), vec![0]);
        let mut both = matches("sablier");
        both.sort();
        assert_eq!(both, vec![0, 1]);
        assert_eq!(matches("sablier +efs"), vec![1]);
        assert_eq!(matches("sablier -efs"), vec![0]);
        assert!(matches("missingterm").is_empty());
    }

    #[test]
    fn phrase_requires_adjacency_within_a_field() {
        assert_eq!(matches(r#""scale to zero""#), vec![0]);
        assert!(matches(r#""zero scale""#).is_empty());
        // "setup" and "scale" live in different fields: never a phrase.
        assert!(matches(r#""setup scale""#).is_empty());
    }

    #[test]
    fn prefixes() {
        assert_eq!(matches("traef*"), vec![0]);
        assert!(matches("traef").is_empty());
        // Non-ASCII terms also match words carrying a particle.
        assert_eq!(matches("배포"), vec![3]);
    }

    #[test]
    fn rarer_terms_score_higher() {
        // "efs" appears twice in doc 1 and nowhere else; "sablier" in 2 docs.
        assert_eq!(matches("efs sablier")[0], 1);
        assert_eq!(matches("*").len(), 4);
    }

    fn rank(docs: &[serde_json::Value], query: &str) -> Vec<usize> {
        let analyzed: Vec<AnalyzedDoc> = docs.iter().map(AnalyzedDoc::from_source).collect();
        let corpus = Corpus::new(analyzed.iter().collect());
        let mut hits = corpus.score_all(&Query::parse(query));
        hits.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        hits.into_iter().map(|(i, _)| i).collect()
    }

    #[test]
    fn short_field_match_outranks_long_body_match() {
        let filler = "lorem ipsum dolor sit amet ".repeat(40);
        let docs = [
            json!({"title": "notes on storage", "content": format!("{filler} sablier {filler}")}),
            json!({"title": "sablier setup", "content": filler.clone()}),
            json!({"title": "other", "content": filler}),
        ];
        assert_eq!(rank(&docs, "sablier"), vec![1, 0]);
    }

    #[test]
    fn whole_value_keyword_match_adds_weight() {
        let docs = [
            json!({"title": "a", "content": "traefik notes", "tags": ["k3s"]}),
            json!({"title": "b", "content": "traefik notes", "tags": ["traefik"]}),
        ];
        // Same text, but doc 1 also matches tags.keyword exactly.
        assert_eq!(rank(&docs, "traefik"), vec![1, 0]);
        // keyword fields are case-sensitive: `Traefik` still hits the text
        // fields (lower-cased) but loses the exact tags.keyword bonus.
        let analyzed: Vec<AnalyzedDoc> = docs.iter().map(AnalyzedDoc::from_source).collect();
        let corpus = Corpus::new(analyzed.iter().collect());
        let score_of_doc1 = |q| {
            corpus
                .score_all(&Query::parse(q))
                .into_iter()
                .find(|(i, _)| *i == 1)
                .map(|(_, s)| s)
                .unwrap()
        };
        assert!(score_of_doc1("Traefik") < score_of_doc1("traefik"));
    }

    #[test]
    fn numbers_and_booleans_match_exactly_and_dates_are_skipped() {
        let docs = [
            json!({"n": 42, "ok": true, "created_at": "2026-10-30T14:00:00+09:00"}),
            json!({"n": 7, "ok": false, "created_at": "2026-10-29"}),
        ];
        assert_eq!(rank(&docs, "42"), vec![0]);
        assert_eq!(rank(&docs, "false"), vec![1]);
        assert!(rank(&docs, "2026").is_empty());
        assert!(rank(&docs, "30").is_empty());
    }

    #[test]
    fn keyword_prefix_and_phrase_match_whole_values() {
        let docs = [
            json!({"tags": ["scale-to-zero"]}),
            json!({"tags": ["zero"]}),
        ];
        // `scale-to-zero` is three text terms in both fields, plus an exact
        // tags.keyword hit only in doc 0.
        assert_eq!(rank(&docs, "scale-to-zero"), vec![0, 1]);
        assert_eq!(rank(&docs, r#""scale to zero""#), vec![0]);
        // Prefix applies to the last term: `scale` + `to*`, neither in doc 1.
        assert_eq!(rank(&docs, "scale-to*"), vec![0]);
    }
}
