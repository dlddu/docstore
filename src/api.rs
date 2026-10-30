//! HTTP surface: the subset of the OpenSearch REST API that the
//! homelab-k3s-mcp `opensearch_*` tools use, plus a few read-only helpers for
//! operations (single-document get, counts, index listing).

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tower_http::trace::TraceLayer;

use crate::error::ApiError;
use crate::search::{Corpus, Query};
use crate::store::{PutOutcome, Store};

/// OpenSearch's default `index.max_result_window`.
pub const MAX_RESULT_WINDOW: usize = 10_000;
const DEFAULT_SIZE: usize = 10;
const BODY_LIMIT: usize = 16 * 1024 * 1024;

type AppState = Arc<Store>;

pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/_search", get(search_all).post(search_all))
        .route("/_count", get(count_all).post(count_all))
        .route("/_cat/indices", get(cat_indices))
        .route("/_refresh", get(refresh_all).post(refresh_all))
        .route("/{index}/_search", get(search_index).post(search_index))
        .route("/{index}/_count", get(count_index).post(count_index))
        .route("/{index}/_refresh", get(refresh_index).post(refresh_index))
        .route("/{index}/_doc", post(create_doc))
        .route(
            "/{index}/_doc/{id}",
            get(get_doc).put(put_doc).post(put_doc).delete(delete_doc),
        )
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(TraceLayer::new_for_http())
        .with_state(store)
}

async fn root() -> Json<Value> {
    Json(json!({
        "name": "docstore",
        "version": {"number": env!("CARGO_PKG_VERSION"), "distribution": "docstore"},
        "tagline": "an OpenSearch REST subset on plain files",
    }))
}

async fn healthz() -> &'static str {
    "ok"
}

async fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "unsupported_operation_exception",
        "endpoint not supported by docstore",
    )
}

// ---------------------------------------------------------------- search

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SearchBody {
    size: Option<i64>,
    from: Option<i64>,
    query: Option<QueryBody>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryBody {
    simple_query_string: Option<SimpleQueryString>,
    query_string: Option<SimpleQueryString>,
    match_all: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
struct SimpleQueryString {
    query: String,
}

fn parse_body<T: for<'de> Deserialize<'de> + Default>(body: &Bytes) -> Result<T, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|e| {
        ApiError::bad_request("parsing_exception", format!("invalid request body: {e}"))
    })
}

fn to_query(body: Option<QueryBody>) -> Result<Query, ApiError> {
    let Some(q) = body else {
        return Ok(Query::MatchAll);
    };
    match (q.simple_query_string.or(q.query_string), q.match_all) {
        (Some(s), None) => Ok(Query::parse(&s.query)),
        (None, Some(_)) => Ok(Query::MatchAll),
        _ => Err(ApiError::bad_request(
            "parsing_exception",
            "query must contain exactly one of [simple_query_string, query_string, match_all]",
        )),
    }
}

async fn search_all(State(store): State<AppState>, body: Bytes) -> Result<Json<Value>, ApiError> {
    search(&store, None, &body)
}

async fn search_index(
    State(store): State<AppState>,
    Path(index): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    search(&store, Some(&index), &body)
}

fn search(store: &Store, index: Option<&str>, body: &Bytes) -> Result<Json<Value>, ApiError> {
    let started = Instant::now();
    let req: SearchBody = parse_body(body)?;
    let size = req.size.unwrap_or(DEFAULT_SIZE as i64);
    let from = req.from.unwrap_or(0);
    if size < 0 || from < 0 {
        return Err(ApiError::bad_request(
            "illegal_argument_exception",
            "[size] and [from] must be non-negative",
        ));
    }
    let (size, from) = (size as usize, from as usize);
    if size + from > MAX_RESULT_WINDOW {
        return Err(ApiError::bad_request(
            "illegal_argument_exception",
            format!("Result window is too large, from + size must be less than or equal to: [{MAX_RESULT_WINDOW}] but was [{}]", size + from),
        ));
    }
    let query = to_query(req.query)?;

    store.with_indices(index, |indices| {
        let entries: Vec<(&str, &crate::store::Document)> = indices
            .iter()
            .flat_map(|(name, docs)| docs.values().map(move |d| (*name, d)))
            .collect();
        let corpus = Corpus::new(entries.iter().map(|(_, d)| &d.analyzed).collect());
        let mut scored = corpus.score_all(&query);
        // Highest score first; ties in a stable (index, id) order.
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

        let total = scored.len();
        let max_score = scored.first().map(|(_, s)| *s);
        let hits: Vec<Value> = scored
            .into_iter()
            .skip(from)
            .take(size)
            .map(|(pos, score)| {
                let (name, doc) = entries[pos];
                json!({
                    "_index": name,
                    "_id": doc.id,
                    "_score": round_score(score),
                    "_source": doc.source,
                })
            })
            .collect();

        Json(json!({
            "took": started.elapsed().as_millis() as u64,
            "timed_out": false,
            "_shards": {"total": 1, "successful": 1, "skipped": 0, "failed": 0},
            "hits": {
                "total": {"value": total, "relation": "eq"},
                "max_score": max_score.map(round_score),
                "hits": hits,
            },
        }))
    })
}

fn round_score(score: f64) -> f64 {
    (score * 1e6).round() / 1e6
}

// ----------------------------------------------------------------- count

async fn count_all(State(store): State<AppState>) -> Result<Json<Value>, ApiError> {
    count(&store, None)
}

async fn count_index(
    State(store): State<AppState>,
    Path(index): Path<String>,
) -> Result<Json<Value>, ApiError> {
    count(&store, Some(&index))
}

// --------------------------------------------------------------- refresh

// Writes are searchable immediately, so refresh is a no-op. It exists so that
// clients written for OpenSearch (which must refresh before reading their own
// writes) run unchanged against docstore.
async fn refresh_all(State(store): State<AppState>) -> Result<Json<Value>, ApiError> {
    refresh(&store, None)
}

async fn refresh_index(
    State(store): State<AppState>,
    Path(index): Path<String>,
) -> Result<Json<Value>, ApiError> {
    refresh(&store, Some(&index))
}

fn refresh(store: &Store, index: Option<&str>) -> Result<Json<Value>, ApiError> {
    store.with_indices(index, |_| {
        Json(json!({"_shards": {"total": 1, "successful": 1, "failed": 0}}))
    })
}

fn count(store: &Store, index: Option<&str>) -> Result<Json<Value>, ApiError> {
    store.with_indices(index, |indices| {
        let n: usize = indices.iter().map(|(_, docs)| docs.len()).sum();
        Json(json!({"count": n}))
    })
}

async fn cat_indices(State(store): State<AppState>) -> Result<Json<Value>, ApiError> {
    store.with_indices(None, |indices| {
        Json(Value::Array(
            indices
                .iter()
                .map(|(name, docs)| json!({"index": name, "docs.count": docs.len().to_string()}))
                .collect(),
        ))
    })
}

// ------------------------------------------------------------- documents

fn parse_document(body: &Bytes) -> Result<Map<String, Value>, ApiError> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(ApiError::bad_request(
            "mapper_parsing_exception",
            "document must be a JSON object",
        )),
        Err(e) => Err(ApiError::bad_request(
            "mapper_parsing_exception",
            format!("failed to parse document: {e}"),
        )),
    }
}

async fn create_doc(
    State(store): State<AppState>,
    Path(index): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    write_doc(store, index, None, body).await
}

async fn put_doc(
    State(store): State<AppState>,
    Path((index, id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Response, ApiError> {
    write_doc(store, index, Some(id), body).await
}

async fn write_doc(
    store: AppState,
    index: String,
    id: Option<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let source = parse_document(&body)?;
    let (id, outcome, index) = tokio::task::spawn_blocking(move || {
        store
            .put(&index, id.as_deref(), source)
            .map(|(id, outcome)| (id, outcome, index))
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))??;

    let status = match outcome {
        PutOutcome::Created => StatusCode::CREATED,
        PutOutcome::Updated => StatusCode::OK,
    };
    Ok((
        status,
        Json(json!({"_index": index, "_id": id, "result": outcome.as_str()})),
    )
        .into_response())
}

async fn get_doc(
    State(store): State<AppState>,
    Path((index, id)): Path<(String, String)>,
) -> Response {
    match store.get(&index, &id) {
        Some(source) => Json(json!({
            "_index": index, "_id": id, "found": true, "_source": source,
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"_index": index, "_id": id, "found": false})),
        )
            .into_response(),
    }
}

async fn delete_doc(
    State(store): State<AppState>,
    Path((index, id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let (index, id, deleted) =
        tokio::task::spawn_blocking(move || store.delete(&index, &id).map(|d| (index, id, d)))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))??;

    let (status, result) = if deleted {
        (StatusCode::OK, "deleted")
    } else {
        (StatusCode::NOT_FOUND, "not_found")
    };
    Ok((
        status,
        Json(json!({"_index": index, "_id": id, "result": result})),
    )
        .into_response())
}
