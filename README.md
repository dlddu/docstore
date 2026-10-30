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
| `POST`/`GET` | `/_refresh`, `/{index}/_refresh` | No-op (writes are already visible); accepted so OpenSearch clients run unchanged |
| `GET` | `/healthz`, `/` | Liveness / version |

Errors use the OpenSearch shape `{"error": {"type", "reason"}, "status"}`.
Index names follow OpenSearch rules (lowercase, no `\/*?"<>| ,#:`, no leading
`_ - +`). Documents must be JSON objects; key order is preserved.

### Search

`simple_query_string` subset: words are OR-ed, `+word` must match, `-word`
must not, `"a phrase"` must appear adjacently within one value, `word*` is a
prefix query and a lone `*` matches everything (score 1). `|`, `(`, `)` and
`~N` are accepted and ignored.

Documents are indexed the way OpenSearch's default dynamic mapping would:
every string is a `text` field (analyzed like the `standard` analyzer:
alphanumeric runs in any script, lower-cased) and, up to 256 characters, also
a `<path>.keyword` field holding the exact value; numbers and booleans are
exact-value fields; date-looking strings (`2026-10-30`,
`2026-10-30T14:00:00+09:00`, `2026/10/30`) are skipped, as date detection
would map them to `date` fields. Ranking is per-field BM25 summed over fields
(`most_fields`, Lucene's formula, k1 1.2, b 0.75), so a hit in a short, rare
field such as a title or a tag outweighs the same word deep in a long body.
Exact-value fields match a query word equal to a whole value (case-sensitive)
and carry no length normalization. Non-ASCII words additionally match longer
words they prefix at half weight, so `배포` finds `배포를`.

Writes are visible to the next search immediately (no refresh interval).

### Compatibility with OpenSearch

`tests/compat/compat.py` sends the same requests to a real OpenSearch 2.x
(security plugin off) and to docstore, and CI fails if statuses, write
results, matched document sets, totals or `_source` round trips differ, or if
the top hit differs on corpora built so that per-field scoring decides it. So a
client of this API can be pointed back at OpenSearch, and data moves either
way with `match_all` + `from/size` reads and `_doc` PUTs.

Known differences (not compared):

- **Exact scores.** Ranking uses the same per-field BM25, but scores are not
  bit-identical (Lucene quantizes field lengths; numeric and boolean matches
  score a constant there).
- **`-word` alone.** With the default `OR` operator, OpenSearch treats
  `a -b` as "a, or anything without b"; docstore requires a positive match.
- **Non-ASCII prefixes.** docstore also matches longer non-ASCII words a
  query word prefixes (`배포` → `배포를`); OpenSearch's standard analyzer
  does not.
- **Mapping is per value, not per index.** OpenSearch fixes a field's type
  from the first document that has it; docstore decides per value (a date
  string in a field that is otherwise text is still skipped).
- **Refresh.** OpenSearch needs a refresh (about 1 s) before a write is
  searchable; docstore does not.
- **Errors.** A delete in a missing index is `404` on both, but OpenSearch
  answers with an `index_not_found_exception` body rather than
  `{"result": "not_found"}`.

## Storage

```
$DATA_DIR/indices/<index>/<percent-encoded id>.json   # {"_index","_id","_source"}
```

Each write goes to a temp file, is fsync-ed and renamed into place, so a crash
never leaves a torn document. All documents are loaded into memory at start-up
(milliseconds for thousands of documents). Run **one** replica: the process
owns its directory and does not coordinate with other writers.

Backups are plain files: `tar` the data directory.

To move documents between docstore and OpenSearch in either direction (each
side optionally basic or SigV4 `es`/`aoss` authenticated), use
`scripts/copy_documents.py`; it copies every document with its index and id
and reads each one back to verify. It is stdlib-only, so it runs as a bare
`python:3.12-slim` Job.

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
python3 tests/compat/compat.py <opensearch-url> <docstore-url>
```

## CI and deployment

Pull requests have one required check, `pr-gate` (`.github/workflows/pr.yml`).
It first publishes `ghcr.io/dlddu/docstore:<head sha>` (arm64, `image.yml`), then
runs `test.yml` against that exact image:

- `fmt + clippy + test`
- `OpenSearch compatibility` — `tests/compat/compat.py` against a real OpenSearch
- `e2e (kind)` — deploys `deploy/overlays/e2e` with the published image and runs
  `scripts/smoke.sh` over HTTP, including a pod restart on a persistent volume

Tests do not run again on `main`. A push to `main` that touches the image
(`Dockerfile`, `Cargo.*`, `src/`) or the production manifests (`deploy/base`,
`deploy/overlays/prod`) publishes `ghcr.io/dlddu/docstore:<sha>` and
force-pushes the `deploy` branch: that commit
plus one commit pinning the image in `deploy/` to `<sha>` (trailer
`Source-Commit: <sha>`). Production follows `deploy`: Flux in
[dlddu/flux-cd-apps](https://github.com/dlddu/flux-cd-apps) applies
`deploy/overlays/prod` from it (`cluster/docstore.yaml`, which also adds the
Sablier scale-to-zero labels). The IngressRoute, Sablier middleware and VPA live
in that repo under `apps/docstore`.

- Image tags are commit SHAs only; there is no `latest`. The `image:` line on
  `main` is a placeholder and does not affect production.
- Roll back by merging a revert PR into `main`; that publishes and pins a new SHA.
- Merges that only change docs, tests, scripts, the e2e overlay or other
  workflows are skipped (`paths` in `image.yml`), so they do not restart the
  pod and `deploy` may lag `main`. To see what production runs, read the
  `Source-Commit` trailer of the tip of `deploy`.
