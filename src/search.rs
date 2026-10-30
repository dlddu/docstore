//! `simple_query_string` parsing and BM25 scoring.
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

use crate::analysis::{tokenize, AnalyzedDoc};

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
            return match terms.len() {
                0 => None,
                1 => Some(Clause {
                    occur: self.occur,
                    any_of: vec![Matcher::Term {
                        term: terms.into_iter().next().unwrap(),
                        prefix: false,
                    }],
                }),
                _ => Some(Clause {
                    occur: self.occur,
                    any_of: vec![Matcher::Phrase(terms)],
                }),
            };
        }
        let text = strip_fuzziness(&self.text);
        let prefix = text.ends_with('*');
        let terms = tokenize(text.trim_end_matches('*'));
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

/// Collection-level statistics for BM25, computed over the searched docs.
pub struct Corpus<'a> {
    docs: Vec<&'a AnalyzedDoc>,
    avg_len: f64,
}

impl<'a> Corpus<'a> {
    pub fn new(docs: Vec<&'a AnalyzedDoc>) -> Self {
        let total: usize = docs.iter().map(|d| d.length).sum();
        let avg_len = if docs.is_empty() {
            1.0
        } else {
            (total as f64 / docs.len() as f64).max(1.0)
        };
        Corpus { docs, avg_len }
    }

    fn idf(&self, matcher: &Matcher) -> f64 {
        let n = self.docs.len() as f64;
        let df = self
            .docs
            .iter()
            .filter(|d| frequency(d, matcher) > 0.0)
            .count() as f64;
        ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
    }

    /// Scores every document of the corpus, returning `(position, score)` for
    /// the matching ones in corpus order.
    pub fn score_all(&self, query: &Query) -> Vec<(usize, f64)> {
        match query {
            Query::MatchNone => Vec::new(),
            Query::MatchAll => (0..self.docs.len()).map(|i| (i, 1.0)).collect(),
            Query::Clauses(clauses) => {
                let idfs: Vec<Vec<f64>> = clauses
                    .iter()
                    .map(|c| c.any_of.iter().map(|m| self.idf(m)).collect())
                    .collect();
                self.docs
                    .iter()
                    .enumerate()
                    .filter_map(|(i, doc)| {
                        self.score_doc(doc, clauses, &idfs).map(|score| (i, score))
                    })
                    .collect()
            }
        }
    }

    fn score_doc(&self, doc: &AnalyzedDoc, clauses: &[Clause], idfs: &[Vec<f64>]) -> Option<f64> {
        let norm = K1 * (1.0 - B + B * doc.length as f64 / self.avg_len);
        let mut score = 0.0;
        let mut matched_positive = false;

        for (clause, clause_idfs) in clauses.iter().zip(idfs) {
            let mut clause_score = 0.0;
            for (matcher, idf) in clause.any_of.iter().zip(clause_idfs) {
                let tf = frequency(doc, matcher);
                if tf > 0.0 {
                    clause_score += idf * tf * (K1 + 1.0) / (tf + norm);
                }
            }
            let hit = clause_score > 0.0;
            match clause.occur {
                Occur::MustNot if hit => return None,
                Occur::MustNot => {}
                Occur::Must if !hit => return None,
                _ => {
                    if hit {
                        matched_positive = true;
                        score += clause_score;
                    }
                }
            }
        }
        matched_positive.then_some(score)
    }
}

/// Weighted term frequency of `matcher` in `doc`.
fn frequency(doc: &AnalyzedDoc, matcher: &Matcher) -> f64 {
    match matcher {
        Matcher::Term { term, prefix } => doc.terms().map(|t| term_weight(t, term, *prefix)).sum(),
        Matcher::Phrase(terms) => doc
            .fields
            .iter()
            .map(|field| {
                field
                    .windows(terms.len())
                    .filter(|w| w.iter().zip(terms).all(|(a, b)| a == b))
                    .count()
            })
            .sum::<usize>() as f64,
    }
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
}
