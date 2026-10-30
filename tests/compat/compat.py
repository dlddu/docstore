#!/usr/bin/env python3
"""Compatibility check: docstore vs a real OpenSearch.

Sends the same requests to both servers and compares what an OpenSearch client
would rely on: status codes, write results, matched document sets, totals and
`_source` round trips, plus the top hit for a few corpora built so that
per-field scoring decides it. Raw scores are not compared, nor is anything
listed under "Known differences" in the README.

Usage: compat.py <opensearch-url> <docstore-url>
Stdlib only. Exits 1 on any mismatch.
"""

from __future__ import annotations

import json
import sys
import urllib.error
import urllib.request

OPENSEARCH, DOCSTORE = (u.rstrip("/") for u in sys.argv[1:3])
failures: list[str] = []


def call(base: str, method: str, path: str, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            status, raw = resp.status, resp.read()
    except urllib.error.HTTPError as err:
        status, raw = err.code, err.read()
    try:
        return status, json.loads(raw) if raw else None
    except json.JSONDecodeError:
        return status, raw.decode("utf-8", "replace")


def both(method: str, path: str, body=None):
    return call(OPENSEARCH, method, path, body), call(DOCSTORE, method, path, body)


def check(name: str, ok: bool, detail: str) -> None:
    print(("ok   " if ok else "FAIL ") + name + ("" if ok else f" — {detail}"))
    if not ok:
        failures.append(name)


def refresh() -> None:
    for base in (OPENSEARCH, DOCSTORE):
        call(base, "POST", "/_refresh")


def same_status(name, method, path, body=None):
    (s1, b1), (s2, b2) = both(method, path, body)
    check(name, s1 == s2, f"opensearch {s1} {b1!r:.200} / docstore {s2} {b2!r:.200}")
    return (s1, b1), (s2, b2)


def same_error(name, method, path, body=None):
    (s1, b1), (s2, b2) = same_status(name + " [status]", method, path, body)
    t1 = (b1 or {}).get("error", {}).get("type") if isinstance(b1, dict) else None
    t2 = (b2 or {}).get("error", {}).get("type") if isinstance(b2, dict) else None
    check(name + " [error.type]", t1 == t2, f"opensearch {t1} / docstore {t2}")


def write(name, method, path, body, expect_id=True):
    (s1, b1), (s2, b2) = same_status(name + " [status]", method, path, body)
    for key in ("_index", "result") + (("_id",) if expect_id else ()):
        check(f"{name} [{key}]", b1.get(key) == b2.get(key), f"{b1.get(key)!r} / {b2.get(key)!r}")
    if not expect_id:
        check(name + " [_id generated]", isinstance(b2.get("_id"), str) and b2["_id"], repr(b2))
    return b1, b2


def hits(body):
    # Keyed by content, not id: auto-generated ids legitimately differ.
    return {
        (h["_index"], json.dumps(h["_source"], sort_keys=True, ensure_ascii=False)): h
        for h in body["hits"]["hits"]
    }


def search(name, query, index=None, size=10):
    path = f"/{index}/_search" if index else "/_search"
    req = {"size": size, "query": {"simple_query_string": {"query": query}}}
    (s1, b1), (s2, b2) = same_status(name + " [status]", "POST", path, req)
    if s1 != 200 or s2 != 200:
        return
    t1, t2 = b1["hits"]["total"]["value"], b2["hits"]["total"]["value"]
    check(name + " [total]", t1 == t2, f"{t1} / {t2}")
    h1, h2 = hits(b1), hits(b2)
    if size >= max(t1, t2):
        check(name + " [hit set]", h1.keys() == h2.keys(),
              f"only opensearch {sorted(h1.keys() - h2.keys())} / only docstore {sorted(h2.keys() - h1.keys())}")
        for key in h1.keys() & h2.keys():
            check(f"{name} [_score {h2[key]['_id']}]", isinstance(h2[key]["_score"], (int, float)),
                  repr(h2[key]["_score"]))
    else:
        check(name + " [hit count]", len(h1) == len(h2), f"{len(h1)} / {len(h2)}")


DOCS = {
    "c1": {"title": "quick brown fox", "body": "alpha beta", "tags": ["k3s", "traefik"], "n": 42},
    "c2": {"title": "lazy dog", "body": "alpha gamma", "tags": ["efs"], "n": 7},
    "c3": {"title": "Scale to zero", "body": "sablier scale-to-zero with traefik", "ok": True},
    "c4": {"title": "한국어 문서", "body": "배포 완료 그리고 점검", "nested": {"deep": "hidden needle"}},
    "c5": {"title": "versions", "body": "release v1.16.1 and k3s_snake_case"},
    "c6": {"title": "dated", "when": "2026-10-30T14:00:00+09:00"},
}

FILLER = "lorem ipsum dolor sit amet consectetur " * 30
# Each corpus is built so that per-field BM25 alone decides the top hit.
RANK_CASES = {
    "compat-rank-title": (
        "sablier",
        "r-title",
        {
            "r-body": {"title": "storage notes", "content": f"{FILLER} sablier {FILLER}"},
            "r-title": {"title": "sablier setup", "content": FILLER},
            "r-none": {"title": "other", "content": FILLER},
        },
    ),
    "compat-rank-tag": (
        "traefik",
        "t-tag",
        {
            "t-text": {"title": "x", "content": f"traefik notes {FILLER}", "tags": ["k3s"]},
            "t-tag": {"title": "y", "content": f"traefik notes {FILLER}", "tags": ["traefik"]},
        },
    ),
}


def same_top(name, query, index, expected):
    req = {"size": 3, "query": {"simple_query_string": {"query": query}}}
    (s1, b1), (s2, b2) = both("POST", f"/{index}/_search", req)
    top1 = b1["hits"]["hits"][0]["_id"] if s1 == 200 and b1["hits"]["hits"] else None
    top2 = b2["hits"]["hits"][0]["_id"] if s2 == 200 and b2["hits"]["hits"] else None
    check(name + " [top hit]", top1 == top2 == expected,
          f"expected {expected}: opensearch {top1} / docstore {top2}")


def main() -> None:
    idx, other = "compat-main", "compat-other"
    for doc_id, doc in DOCS.items():
        write(f"put {doc_id}", "PUT", f"/{idx}/_doc/{doc_id}", doc)
    write("re-put c5 (upsert)", "PUT", f"/{idx}/_doc/c5", {**DOCS["c5"], "extra": "upsertneedle"})
    for index, (_, _, docs) in RANK_CASES.items():
        for doc_id, doc in docs.items():
            write(f"put {index}/{doc_id}", "PUT", f"/{index}/_doc/{doc_id}", doc)
    write("post auto id", "POST", f"/{other}/_doc", {"title": "auto generated alpha"}, expect_id=False)
    write("put id with reserved chars", "PUT", f"/{other}/_doc/a%2Fb%20c", {"title": "slash space alpha"})
    refresh()

    (s1, b1), (s2, b2) = both("GET", f"/{idx}/_doc/c1")
    check("get c1", s1 == s2 == 200 and b1["_source"] == b2["_source"], f"{s1}/{s2}")
    (s1, b1), (s2, b2) = both("GET", f"/{idx}/_doc/c5")
    check("get c5 after upsert", b1["_source"] == b2["_source"], f"{b1} / {b2}")

    search("word", "alpha", idx)
    search("two words OR", "beta gamma", idx)
    search("case insensitive", "ALPHA", idx)
    search("must +", "+alpha +gamma", idx)
    search("phrase", '"quick brown"', idx)
    search("phrase wrong order", '"brown quick"', idx)
    search("phrase across values", '"fox alpha"', idx)
    search("prefix", "traef*", idx)
    search("hyphenated", "scale-to-zero", idx)
    search("array value", "efs", idx)
    search("nested object value", "needle", idx)
    search("boolean value", "true", idx)
    search("korean whole word", "배포", idx)
    search("underscore token", "k3s_snake_case", idx)
    search("version token", "v1.16.1", idx)
    search("upserted content", "upsertneedle", idx)
    search("number value", "42", idx)
    search("text next to a date field", "dated", idx)
    search("token only inside a date value", "14", idx)
    search("match all", "*", idx)
    search("no match", "zzznomatch", idx)
    search("all indices", "alpha")
    search("index list", "alpha", f"{idx},{other}")
    search("index wildcard", "alpha", "compat-*")
    search("size limits hits", "alpha", idx, size=1)
    same_error("search missing index", "POST", "/compat-missing/_search", {"query": {"match_all": {}}})
    search("wildcard matching nothing", "alpha", "nothing-here-*")

    for index, (query, expected, _) in RANK_CASES.items():
        same_top(f"ranking {index}", query, index, expected)

    (s1, b1), (s2, b2) = both("GET", f"/{idx}/_count")
    check("count", s1 == s2 == 200 and b1["count"] == b2["count"], f"{b1} / {b2}")

    (s1, b1), (s2, b2) = same_status("delete c2 [status]", "DELETE", f"/{idx}/_doc/c2")
    check("delete c2 [result]", b1.get("result") == b2.get("result") == "deleted", f"{b1} / {b2}")
    (s1, b1), (s2, b2) = same_status("delete c2 again [status]", "DELETE", f"/{idx}/_doc/c2")
    check("delete c2 again [result]", b1.get("result") == b2.get("result") == "not_found", f"{b1} / {b2}")
    same_status("delete in missing index", "DELETE", "/compat-missing/_doc/x")
    refresh()
    search("deleted doc gone", "gamma", idx)

    same_error("invalid index name", "PUT", "/Bad_Index/_doc/1", {"a": 1})
    same_error("index name with leading underscore", "PUT", "/_bad/_doc/1", {"a": 1})
    same_status("non-object document", "PUT", f"/{idx}/_doc/arr", [1, 2])
    same_status("size above result window", "POST", "/_search", {"size": 10001})

    print(f"\n{len(failures)} mismatch(es)")
    sys.exit(1 if failures else 0)


main()
