//! HTTP contract tests. Request and response shapes mirror what the
//! homelab-k3s-mcp opensearch client (internal/opensearch) sends and parses.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use docstore::{api, store::Store};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

struct Harness {
    dir: tempfile::TempDir,
    app: Router,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let app = api::router(Arc::new(Store::open(dir.path()).unwrap()));
        Harness { dir, app }
    }

    /// Simulates a pod restart: a fresh store over the same directory.
    fn restart(self) -> Self {
        let app = api::router(Arc::new(Store::open(self.dir.path()).unwrap()));
        Harness { dir: self.dir, app }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&b).unwrap())
            }
            None => Body::empty(),
        };
        let resp = self
            .app
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, value)
    }

    /// The exact body internal/opensearch.Client.Search sends.
    async fn search(&self, index: Option<&str>, query: &str, size: i64) -> (StatusCode, Value) {
        let path = match index {
            Some(i) => format!("/{i}/_search"),
            None => "/_search".to_string(),
        };
        let body = json!({"size": size, "query": {"simple_query_string": {"query": query}}});
        self.call(Method::POST, &path, Some(body)).await
    }
}

fn hit_ids(resp: &Value) -> Vec<String> {
    resp["hits"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn put_with_id_creates_then_updates() {
    let h = Harness::new();
    let (s, b) = h
        .call(
            Method::PUT,
            "/notes/_doc/n1",
            Some(json!({"title": "first"})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(
        b,
        json!({"_index": "notes", "_id": "n1", "result": "created"})
    );

    let (s, b) = h
        .call(
            Method::PUT,
            "/notes/_doc/n1",
            Some(json!({"title": "second"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["result"], "updated");

    let (s, b) = h.call(Method::GET, "/notes/_doc/n1", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["_source"], json!({"title": "second"}));
}

#[tokio::test]
async fn post_without_id_generates_one() {
    let h = Harness::new();
    let (s, b) = h
        .call(Method::POST, "/notes/_doc", Some(json!({"title": "auto"})))
        .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(b["result"], "created");
    let id = b["_id"].as_str().unwrap();
    assert_eq!(id.len(), 20);
    let (s, _) = h
        .call(Method::GET, &format!("/notes/_doc/{id}"), None)
        .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn ids_with_reserved_characters_round_trip() {
    let h = Harness::new();
    // The Go client url.PathEscape-s ids, so "a/b c" arrives as a%2Fb%20c.
    let (s, b) = h
        .call(
            Method::PUT,
            "/notes/_doc/a%2Fb%20c",
            Some(json!({"k": "v"})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(b["_id"], "a/b c");
    let (_, found) = h.search(Some("notes"), "v", 10).await;
    assert_eq!(hit_ids(&found), ["a/b c"]);
    let (s, b) = h.call(Method::DELETE, "/notes/_doc/a%2Fb%20c", None).await;
    assert_eq!((s, b["result"].clone()), (StatusCode::OK, json!("deleted")));
}

#[tokio::test]
async fn delete_reports_deleted_then_not_found() {
    let h = Harness::new();
    h.call(Method::PUT, "/notes/_doc/d1", Some(json!({"x": 1})))
        .await;

    let (s, b) = h.call(Method::DELETE, "/notes/_doc/d1", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["result"], "deleted");

    let (s, b) = h.call(Method::DELETE, "/notes/_doc/d1", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["result"], "not_found");

    // A missing index is also a 404, which the client maps to not_found.
    let (s, _) = h.call(Method::DELETE, "/no-such-index/_doc/d1", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn search_response_has_the_shape_the_client_parses() {
    let h = Harness::new();
    h.call(
        Method::PUT,
        "/claude-docs/_doc/doc-1",
        Some(json!({"title": "Sablier setup", "tags": ["k3s", "traefik"], "content": "scale to zero"})),
    )
    .await;
    h.call(
        Method::PUT,
        "/claude-docs/_doc/doc-2",
        Some(json!({"title": "EFS storage", "content": "persistent volumes"})),
    )
    .await;

    let (s, b) = h.search(Some("claude-docs"), "sablier traefik", 10).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["hits"]["total"]["value"], 1);
    let hit = &b["hits"]["hits"][0];
    assert_eq!(hit["_index"], "claude-docs");
    assert_eq!(hit["_id"], "doc-1");
    assert!(hit["_score"].as_f64().unwrap() > 0.0);
    assert_eq!(hit["_source"]["tags"], json!(["k3s", "traefik"]));
}

#[tokio::test]
async fn search_scopes_to_an_index_or_spans_all() {
    let h = Harness::new();
    h.call(Method::PUT, "/a/_doc/1", Some(json!({"t": "shared token"})))
        .await;
    h.call(Method::PUT, "/b/_doc/2", Some(json!({"t": "shared other"})))
        .await;

    let (_, all) = h.search(None, "shared", 10).await;
    assert_eq!(all["hits"]["total"]["value"], 2);
    let (_, only_a) = h.search(Some("a"), "shared", 10).await;
    assert_eq!(hit_ids(&only_a), ["1"]);

    let (s, b) = h.search(Some("missing"), "shared", 10).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["error"]["type"], "index_not_found_exception");
}

#[tokio::test]
async fn size_limits_hits_but_not_total() {
    let h = Harness::new();
    for i in 0..15 {
        h.call(
            Method::PUT,
            &format!("/bulk/_doc/{i}"),
            Some(json!({"t": format!("common item{i}")})),
        )
        .await;
    }
    let (_, b) = h.search(Some("bulk"), "common", 10).await;
    assert_eq!(b["hits"]["hits"].as_array().unwrap().len(), 10);
    assert_eq!(b["hits"]["total"]["value"], 15);

    let (_, b) = h
        .call(
            Method::POST,
            "/bulk/_search",
            Some(json!({"size": 5, "from": 12})),
        )
        .await;
    assert_eq!(b["hits"]["hits"].as_array().unwrap().len(), 3);

    let (s, _) = h
        .call(Method::POST, "/_search", Some(json!({"size": 10001})))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn match_all_and_empty_body() {
    let h = Harness::new();
    h.call(Method::PUT, "/x/_doc/1", Some(json!({"a": 1})))
        .await;
    let (s, b) = h.call(Method::POST, "/_search", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["hits"]["total"]["value"], 1);
    let (_, b) = h.search(None, "*", 10).await;
    assert_eq!(b["hits"]["hits"][0]["_score"], 1.0);
}

#[tokio::test]
async fn rejects_invalid_input() {
    let h = Harness::new();
    let (s, b) = h.call(Method::PUT, "/Bad/_doc/1", Some(json!({}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["type"], "invalid_index_name_exception");

    let (s, b) = h.call(Method::PUT, "/ok/_doc/1", Some(json!([1, 2]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["type"], "mapper_parsing_exception");

    let (s, _) = h
        .call(
            Method::POST,
            "/_search",
            Some(json!({"query": {"term": {"a": 1}}})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, _) = h.call(Method::GET, "/ok/_mapping", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn data_survives_a_restart() {
    let h = Harness::new();
    h.call(
        Method::PUT,
        "/keep/_doc/k1",
        Some(json!({"body": "durable words"})),
    )
    .await;
    h.call(
        Method::PUT,
        "/keep/_doc/k2",
        Some(json!({"body": "gone soon"})),
    )
    .await;
    h.call(Method::DELETE, "/keep/_doc/k2", None).await;

    let h = h.restart();
    let (_, b) = h.search(Some("keep"), "durable", 10).await;
    assert_eq!(hit_ids(&b), ["k1"]);
    let (_, b) = h.call(Method::GET, "/keep/_count", None).await;
    assert_eq!(b["count"], 1);
    let (_, b) = h.call(Method::GET, "/_cat/indices", None).await;
    assert_eq!(b, json!([{"index": "keep", "docs.count": "1"}]));
}

#[tokio::test]
async fn health_endpoints() {
    let h = Harness::new();
    let (s, b) = h.call(Method::GET, "/healthz", None).await;
    assert_eq!((s, b), (StatusCode::OK, json!("ok")));
    let (s, b) = h.call(Method::GET, "/", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["name"], "docstore");
}
