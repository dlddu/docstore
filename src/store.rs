//! File-backed document store.
//!
//! Layout: `<root>/indices/<index>/<encoded id>.json`, one JSON file per
//! document holding `{"_index", "_id", "_source"}`. Writes go to a temporary
//! file in the same directory, are fsync-ed, then renamed over the target, so
//! a crash never leaves a half-written document (rename is atomic on NFS/EFS
//! within one directory). Everything is loaded into memory at start-up; the
//! data set is expected to be small (thousands of documents, not millions).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::analysis::AnalyzedDoc;
use crate::error::ApiError;

/// Characters kept verbatim in file names; everything else (including `.`,
/// so names can never be `..` or hidden) is percent-encoded.
const FILE_SAFE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'_').remove(b'-');
const MAX_ENCODED_NAME: usize = 200;
const MAX_ID_BYTES: usize = 512;
const MAX_INDEX_BYTES: usize = 255;
const TMP_PREFIX: &str = ".tmp-";

#[derive(Debug, Clone)]
pub struct Document {
    pub id: String,
    pub source: Value,
    pub analyzed: AnalyzedDoc,
}

#[derive(Serialize, Deserialize)]
struct StoredDocument {
    #[serde(rename = "_index")]
    index: String,
    #[serde(rename = "_id")]
    id: String,
    #[serde(rename = "_source")]
    source: Value,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PutOutcome {
    Created,
    Updated,
}

impl PutOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            PutOutcome::Created => "created",
            PutOutcome::Updated => "updated",
        }
    }
}

pub type IndexDocs = BTreeMap<String, Document>;
type Indices = BTreeMap<String, IndexDocs>;
/// Indices picked by a pattern, as `(name, documents)` pairs.
pub type Selection<'a> = Vec<(&'a str, &'a IndexDocs)>;

pub struct Store {
    root: PathBuf,
    indices: RwLock<Indices>,
}

impl Store {
    /// Opens (creating if needed) the store rooted at `root` and loads every
    /// document into memory. Stray temporary files from an interrupted write
    /// are removed.
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Store> {
        let root = root.into();
        let indices_dir = root.join("indices");
        fs::create_dir_all(&indices_dir)?;

        let mut indices = Indices::new();
        for entry in fs::read_dir(&indices_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let dir = entry.path();
            let mut docs = BTreeMap::new();
            for file in fs::read_dir(&dir)? {
                let path = file?.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with(TMP_PREFIX) {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if !name.ends_with(".json") {
                    continue;
                }
                let bytes = fs::read(&path)?;
                let stored: StoredDocument = serde_json::from_slice(&bytes).map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{}: {e}", path.display()),
                    )
                })?;
                let analyzed = AnalyzedDoc::from_source(&stored.source);
                docs.insert(
                    stored.id.clone(),
                    Document {
                        id: stored.id,
                        source: stored.source,
                        analyzed,
                    },
                );
            }
            if let Some(index) = entry.file_name().to_str() {
                indices.insert(index.to_owned(), docs);
            }
        }

        Ok(Store {
            root,
            indices: RwLock::new(indices),
        })
    }

    pub fn put(
        &self,
        index: &str,
        id: Option<&str>,
        source: Map<String, Value>,
    ) -> Result<(String, PutOutcome), ApiError> {
        validate_index(index)?;
        let id = match id {
            Some(id) => {
                validate_id(id)?;
                id.to_owned()
            }
            None => generate_id(),
        };
        let source = Value::Object(source);
        let stored = StoredDocument {
            index: index.to_owned(),
            id: id.clone(),
            source,
        };

        let mut indices = self.indices.write().unwrap_or_else(|e| e.into_inner());
        let dir = self.index_dir(index);
        fs::create_dir_all(&dir).map_err(ApiError::io)?;
        let bytes = serde_json::to_vec(&stored).map_err(|e| ApiError::internal(e.to_string()))?;
        write_atomic(&dir, &file_name(&id), &bytes).map_err(ApiError::io)?;

        let analyzed = AnalyzedDoc::from_source(&stored.source);
        let previous = indices.entry(index.to_owned()).or_default().insert(
            id.clone(),
            Document {
                id: id.clone(),
                source: stored.source,
                analyzed,
            },
        );
        let outcome = if previous.is_some() {
            PutOutcome::Updated
        } else {
            PutOutcome::Created
        };
        Ok((id, outcome))
    }

    /// Returns whether a document was deleted. A missing index or document is
    /// `Ok(false)`, never an error.
    pub fn delete(&self, index: &str, id: &str) -> Result<bool, ApiError> {
        let mut indices = self.indices.write().unwrap_or_else(|e| e.into_inner());
        let Some(docs) = indices.get_mut(index) else {
            return Ok(false);
        };
        if !docs.contains_key(id) {
            return Ok(false);
        }
        let path = self.index_dir(index).join(file_name(id));
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(ApiError::io(e)),
        }
        sync_dir(&self.index_dir(index)).map_err(ApiError::io)?;
        docs.remove(id);
        Ok(true)
    }

    pub fn get(&self, index: &str, id: &str) -> Option<Value> {
        let indices = self.indices.read().unwrap_or_else(|e| e.into_inner());
        indices.get(index)?.get(id).map(|d| d.source.clone())
    }

    /// Runs `f` with read access to the indices named by `pattern` (a
    /// comma-separated list that may contain `*` wildcards; `None` or `_all`
    /// means every index). A concrete name that does not exist is an
    /// `index_not_found_exception`, like OpenSearch.
    pub fn with_indices<R>(
        &self,
        pattern: Option<&str>,
        f: impl FnOnce(Selection<'_>) -> R,
    ) -> Result<R, ApiError> {
        let indices = self.indices.read().unwrap_or_else(|e| e.into_inner());
        let selected = select(&indices, pattern)?;
        Ok(f(selected))
    }

    fn index_dir(&self, index: &str) -> PathBuf {
        self.root.join("indices").join(index)
    }
}

fn select<'a>(indices: &'a Indices, pattern: Option<&str>) -> Result<Selection<'a>, ApiError> {
    let parts: Vec<&str> = match pattern {
        None | Some("_all") | Some("*") | Some("") => {
            return Ok(indices.iter().map(|(k, v)| (k.as_str(), v)).collect())
        }
        Some(p) => p
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect(),
    };
    let mut names: Vec<&str> = Vec::new();
    for part in parts {
        if part.contains('*') {
            names.extend(
                indices
                    .keys()
                    .map(String::as_str)
                    .filter(|name| glob_match(part, name)),
            );
        } else if indices.contains_key(part) {
            names.push(part);
        } else {
            return Err(ApiError::index_not_found(part));
        }
    }
    names.sort_unstable();
    names.dedup();
    Ok(names
        .into_iter()
        .map(|n| {
            let (k, v) = indices.get_key_value(n).expect("selected from keys");
            (k.as_str(), v)
        })
        .collect())
}

fn glob_match(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let segments: Vec<&str> = parts.collect();
    for (i, seg) in segments.iter().enumerate() {
        if i == segments.len() - 1 {
            return rest.ends_with(seg);
        }
        match rest.find(seg) {
            Some(pos) => rest = &rest[pos + seg.len()..],
            None => return false,
        }
    }
    rest.is_empty()
}

/// OpenSearch index-name rules.
pub fn validate_index(index: &str) -> Result<(), ApiError> {
    let reason = if index.is_empty() {
        Some("must not be empty")
    } else if index.len() > MAX_INDEX_BYTES {
        Some("index name is too long, (must be at most 255 bytes)")
    } else if index == "." || index == ".." {
        Some("must not be '.' or '..'")
    } else if index.starts_with(['_', '-', '+']) {
        Some("must not start with '_', '-', or '+'")
    } else if index.chars().any(|c| c.is_uppercase()) {
        Some("must be lowercase")
    } else if index.chars().any(|c| {
        matches!(
            c,
            '\\' | '/' | '*' | '?' | '"' | '<' | '>' | '|' | ' ' | ',' | '#' | ':'
        )
    }) {
        Some(r#"must not contain the following characters [ , ", *, \, <, |, ,, >, /, ?, #, :]"#)
    } else {
        None
    };
    match reason {
        Some(r) => Err(ApiError::invalid_index_name(index, r)),
        None => Ok(()),
    }
}

fn validate_id(id: &str) -> Result<(), ApiError> {
    if id.is_empty() {
        return Err(ApiError::bad_request(
            "illegal_argument_exception",
            "if _id is specified it must not be empty",
        ));
    }
    if id.len() > MAX_ID_BYTES {
        return Err(ApiError::bad_request(
            "illegal_argument_exception",
            format!(
                "id [{id}] is too long, must be no longer than 512 bytes but was: {}",
                id.len()
            ),
        ));
    }
    Ok(())
}

/// 20 random URL-safe characters, the same shape as OpenSearch auto ids.
fn generate_id() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(20)
        .map(char::from)
        .collect()
}

/// Deterministic, file-system-safe name for a document id.
fn file_name(id: &str) -> String {
    let encoded = utf8_percent_encode(id, FILE_SAFE).to_string();
    if encoded.len() <= MAX_ENCODED_NAME {
        format!("{encoded}.json")
    } else {
        let digest = Sha256::digest(id.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        format!("~{hex}.json")
    }
}

fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = dir.join(format!("{TMP_PREFIX}{}", generate_id()));
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, dir.join(name))?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    // Directory fsync makes the rename/unlink durable on local file systems.
    // Some network file systems refuse to open a directory for syncing; the
    // rename itself is already durable there, so that failure is ignored.
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn put_get_delete_roundtrip_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        let (id, outcome) = store
            .put("docs", Some("a/b"), obj(json!({"x": 1})))
            .unwrap();
        assert_eq!((id.as_str(), outcome), ("a/b", PutOutcome::Created));
        let (_, outcome) = store
            .put("docs", Some("a/b"), obj(json!({"x": 2})))
            .unwrap();
        assert_eq!(outcome, PutOutcome::Updated);
        let (auto, outcome) = store.put("docs", None, obj(json!({"y": true}))).unwrap();
        assert_eq!(outcome, PutOutcome::Created);
        assert_eq!(auto.len(), 20);

        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.get("docs", "a/b"), Some(json!({"x": 2})));
        assert_eq!(store.get("docs", &auto), Some(json!({"y": true})));

        assert!(store.delete("docs", "a/b").unwrap());
        assert!(!store.delete("docs", "a/b").unwrap());
        assert!(!store.delete("nope", "a/b").unwrap());
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.get("docs", "a/b"), None);
    }

    #[test]
    fn preserves_key_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let src: Map<String, Value> =
            serde_json::from_str(r#"{"zeta":1,"alpha":2,"mid":{"b":1,"a":2}}"#).unwrap();
        store.put("i", Some("1"), src).unwrap();
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(
            serde_json::to_string(&store.get("i", "1").unwrap()).unwrap(),
            r#"{"zeta":1,"alpha":2,"mid":{"b":1,"a":2}}"#
        );
    }

    #[test]
    fn removes_stray_temp_files_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("indices").join("docs");
        fs::create_dir_all(&idx).unwrap();
        fs::write(idx.join(".tmp-abc"), b"partial").unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(!idx.join(".tmp-abc").exists());
        assert!(store.with_indices(Some("docs"), |s| s.len()).unwrap() == 1);
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(
            file_name("20260801-sablier_setup"),
            "20260801-sablier_setup.json"
        );
        assert_eq!(file_name(".."), "%2E%2E.json");
        assert_eq!(file_name("a/b"), "a%2Fb.json");
        assert_eq!(file_name("한"), "%ED%95%9C.json");
        let long = "x".repeat(300);
        assert!(file_name(&long).starts_with('~'));
        assert!(file_name(&long).len() < 80);
    }

    #[test]
    fn index_validation() {
        assert!(validate_index("claude-docs").is_ok());
        assert!(validate_index(".hidden").is_ok());
        for bad in [
            "", ".", "..", "_x", "-x", "+x", "Upper", "a/b", "a*b", "a b", "a:b", "a,b",
        ] {
            assert!(validate_index(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn index_selection() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for i in ["logs-a", "logs-b", "docs"] {
            store.put(i, Some("1"), obj(json!({}))).unwrap();
        }
        let names = |p: Option<&str>| {
            store
                .with_indices(p, |s| {
                    s.iter().map(|(n, _)| n.to_string()).collect::<Vec<_>>()
                })
                .unwrap()
        };
        assert_eq!(names(None), ["docs", "logs-a", "logs-b"]);
        assert_eq!(names(Some("logs-*")), ["logs-a", "logs-b"]);
        assert_eq!(names(Some("docs,logs-b,docs")), ["docs", "logs-b"]);
        assert_eq!(names(Some("nomatch-*")), Vec::<String>::new());
        assert!(store.with_indices(Some("missing"), |_| ()).is_err());
        assert!(glob_match("a*c*e", "abcde"));
        assert!(!glob_match("a*c", "abcd"));
    }
}
