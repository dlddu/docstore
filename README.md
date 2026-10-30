# docstore

A tiny document store that speaks the slice of the OpenSearch REST API used by
the [homelab-k3s-mcp](https://github.com/dlddu/homelab-k3s-mcp) `opensearch_*`
tools, and keeps every document as a JSON file on disk (an EFS volume in the
homelab). It replaces an OpenSearch Serverless collection whose minimum cost was
out of proportion to a few hundred notes.

It is **not** a general OpenSearch replacement: no mappings, aggregations,
bulk API, or query DSL beyond `simple_query_string` / `match_all`.

## API

| Method | Path | Behaviour |
| --- | --- | --- |
| `POST`/`GET` | `/_search`, `/{index}/_search` | Body `{"size", "from", "query": {"simple_query_string": {"query"}}}`. `size` defaults to 10, `from + size` ≤ 10 000. `{index}` may be a comma list with `*` wildcards; a missing concrete index is `404 index_not_found_exception`. |
| `POST` | `/{index}/_doc` | Create with a generated 20-char id → `201 {"result":"created"}` |
| `PUT`/`POST` | `/{index}/_doc/{id}` | Upsert → `201 created` or `200 updated`. The index is created on first write. |
| `GET` | `/{index}/_doc/{id}` | `{"found": true, "_source"}` or `404 {"found": false}` |
| `DELETE` | `/{index}/_doc/{id}` | `200 {"result":"deleted"}` or `404 {"result":"not_found"}` (also for a missing index) |
| `GET` | `/_count`, `/{index}/_count` | `{"count": n}` |
| `GET` | `/_cat/indices` | `[{"index", "docs.count"}]` |
| `GET` | `/healthz`, `/` | Liveness / version |

Errors use the OpenSearch shape `{"error": {"type", "reason"}, "status"}`.
Index names follow OpenSearch rules (lowercase, no `\/*?"<>| ,#:`, no leading
`_ - +`). Documents must be JSON objects; key order is preserved.

### Search

`simple_query_string` subset: words are OR-ed, `+word` must match, `-word`
must not, `"a phrase"` must appear adjacently within one value, `word*` is a
prefix query and a lone `*` matches everything (score 1). `|`, `(`, `)` and
`~N` are accepted and ignored. Text is analyzed like OpenSearch's `standard`
analyzer (alphanumeric runs in any script, lower-cased). Results are ranked
with BM25 over all values of the document. Non-ASCII words additionally match
longer words they prefix at half weight, so `배포` finds `배포를`.

Writes are visible to the next search immediately (no refresh interval).

## Storage

```
$DATA_DIR/indices/<index>/<percent-encoded id>.json   # {"_index","_id","_source"}
```

Each write goes to a temp file, is fsync-ed and renamed into place, so a crash
never leaves a torn document. All documents are loaded into memory at start-up
(milliseconds for thousands of documents). Run **one** replica: the process
owns its directory and does not coordinate with other writers.

Backups are plain files: `tar` the data directory.

## Configuration

| Env | Default | |
| --- | --- | --- |
| `DATA_DIR` | `/data` | Store root |
| `LISTEN_ADDR` | `0.0.0.0:9200` | HTTP listen address |
| `RUST_LOG` | `info` | Log filter (JSON logs to stdout) |

There is no authentication; expose it only inside the cluster.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
./scripts/smoke.sh <image>   # container test with a volume and a restart
```

CI runs fmt, clippy, unit/HTTP tests and the container smoke test on every
push and PR; pushes to `main` publish `ghcr.io/dlddu/docstore:<sha>` (arm64).
