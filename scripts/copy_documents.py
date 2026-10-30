#!/usr/bin/env python3
"""Copy every document between two OpenSearch-REST servers, either direction.

Used 2026-10-30 to move the AOSS collection into docstore (361 documents,
read back 361/361 identical). Moving back to OpenSearch is the same script
with SOURCE and TARGET swapped.

Each document keeps its index and id. Every copied document is read back
from the target and compared; the script exits 1 on any mismatch. Stdlib
only, so it runs in a bare `python:3.12-slim` Job.

Environment:
  SOURCE_URL, TARGET_URL       base URLs (e.g. http://docstore-external.docstore.svc.cluster.local)
  SOURCE_INDEX                 optional comma list / wildcard; default every index
  DRY_RUN=1                    read and count only

  Per side (prefix SOURCE_ or TARGET_), optional authentication:
  <SIDE>_USERNAME, <SIDE>_PASSWORD     basic auth
  <SIDE>_SIGV4_SERVICE                 "aoss" or "es": SigV4-sign that side
  <SIDE>_ROLE_ARN                      role to assume for SigV4 (else base credentials are used directly)
  AWS_REGION                           signing region (else parsed from the endpoint, else IMDS)

  Base AWS credentials: AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY[/AWS_SESSION_TOKEN],
  else the instance profile via IMDS (v2, falling back to v1 when a pod's hop
  limit drops the token reply).

OpenSearch note: dynamic mapping turns date-looking strings (e.g. created_at)
into `date` fields. That is what docstore assumes too, so search behaves the
same; create an index template first only if you want something else.
"""

from __future__ import annotations

import base64
import datetime
import hashlib
import hmac
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

PAGE = 500
# OpenSearch's default index.max_result_window (from + size).
MAX_WINDOW = 10_000


def http(method, url, body=None, headers=None, timeout=60):
    req = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()


def imds_credentials():
    headers = {}
    try:
        status, token = http("PUT", "http://169.254.169.254/latest/api/token",
                             headers={"X-aws-ec2-metadata-token-ttl-seconds": "300"}, timeout=3)
        if status == 200:
            headers = {"X-aws-ec2-metadata-token": token.decode()}
    except OSError as err:
        print(f"IMDSv2 token unavailable ({err}); falling back to IMDSv1", flush=True)
    base = "http://169.254.169.254/latest/meta-data/"
    _, region = http("GET", base + "placement/region", headers=headers)
    _, role = http("GET", base + "iam/security-credentials/", headers=headers)
    _, creds = http("GET", base + "iam/security-credentials/" + role.decode().strip(), headers=headers)
    c = json.loads(creds)
    return region.decode(), (c["AccessKeyId"], c["SecretAccessKey"], c["Token"])


def base_credentials():
    if os.environ.get("AWS_ACCESS_KEY_ID"):
        return None, (os.environ["AWS_ACCESS_KEY_ID"], os.environ["AWS_SECRET_ACCESS_KEY"],
                      os.environ.get("AWS_SESSION_TOKEN", ""))
    return imds_credentials()


def sign(method, url, body, creds, region, service, headers):
    access_key, secret_key, token = creds
    parts = urllib.parse.urlsplit(url)
    now = datetime.datetime.now(datetime.timezone.utc)
    amz_date, day = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
    payload = hashlib.sha256(body or b"").hexdigest()
    signed = {"host": parts.netloc, "x-amz-date": amz_date, "x-amz-content-sha256": payload}
    if token:
        signed["x-amz-security-token"] = token
    signed.update({k.lower(): v for k, v in headers.items()})
    query = "&".join(
        f"{urllib.parse.quote(k, safe='-_.~')}={urllib.parse.quote(v, safe='-_.~')}"
        for k, v in sorted(urllib.parse.parse_qsl(parts.query, keep_blank_values=True))
    )
    names = sorted(signed)
    # Services other than S3 sign the path URI-encoded once more (so an id
    # sent as a%2Fb is signed as a%252Fb), matching botocore and the Go SDK.
    canonical_path = urllib.parse.quote(parts.path or "/", safe="/~")
    canonical = "\n".join([
        method, canonical_path, query,
        "".join(f"{n}:{signed[n].strip()}\n" for n in names), ";".join(names), payload,
    ])
    scope = f"{day}/{region}/{service}/aws4_request"
    to_sign = "\n".join(["AWS4-HMAC-SHA256", amz_date, scope, hashlib.sha256(canonical.encode()).hexdigest()])
    key = ("AWS4" + secret_key).encode()
    for part in (day, region, service, "aws4_request"):
        key = hmac.new(key, part.encode(), hashlib.sha256).digest()
    signature = hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()
    signed["authorization"] = (f"AWS4-HMAC-SHA256 Credential={access_key}/{scope}, "
                               f"SignedHeaders={';'.join(names)}, Signature={signature}")
    return signed


def assume_role(role_arn, region, base):
    body = urllib.parse.urlencode({"Action": "AssumeRole", "Version": "2011-06-15",
                                   "RoleArn": role_arn, "RoleSessionName": "docstore-copy"}).encode()
    url = f"https://sts.{region}.amazonaws.com/"
    headers = sign("POST", url, body, base, region, "sts",
                   {"content-type": "application/x-www-form-urlencoded"})
    status, data = http("POST", url, body, headers)
    if status != 200:
        sys.exit(f"AssumeRole {role_arn} failed {status}: {data[:300]!r}")
    xml = data.decode()
    field = lambda tag: re.search(f"<{tag}>(.*?)</{tag}>", xml, re.S).group(1)  # noqa: E731
    return field("AccessKeyId"), field("SecretAccessKey"), field("SessionToken")


class Endpoint:
    def __init__(self, side):
        self.side = side
        self.url = os.environ[f"{side}_URL"].rstrip("/")
        user, password = os.environ.get(f"{side}_USERNAME"), os.environ.get(f"{side}_PASSWORD")
        self.basic = (base64.b64encode(f"{user}:{password}".encode()).decode()
                      if user and password else None)
        self.service = os.environ.get(f"{side}_SIGV4_SERVICE")
        self.creds = self.region = None
        if self.service:
            if self.service not in ("aoss", "es"):
                sys.exit(f"{side}_SIGV4_SERVICE must be aoss or es")
            imds_region, base = base_credentials()
            match = re.search(r"\.([a-z]{2}-[a-z]+-\d)\.(?:aoss|es)\.amazonaws\.com", self.url)
            self.region = os.environ.get("AWS_REGION") or (match and match.group(1)) or imds_region
            role = os.environ.get(f"{side}_ROLE_ARN")
            self.creds = assume_role(role, self.region, base) if role else base
        mode = "sigv4/" + self.service if self.service else ("basic" if self.basic else "none")
        print(f"{side}: {self.url} (auth {mode})", flush=True)

    def call(self, method, path, payload=None):
        body = json.dumps(payload, ensure_ascii=False).encode() if payload is not None else b""
        headers = {"content-type": "application/json"}
        url = self.url + path
        if self.service:
            headers = sign(method, url, body, self.creds, self.region, self.service, headers)
        elif self.basic:
            headers["authorization"] = "Basic " + self.basic
        status, data = http(method, url, body or None, headers, timeout=90)
        try:
            return status, json.loads(data) if data else None
        except json.JSONDecodeError:
            return status, data.decode("utf-8", "replace")


def doc_path(index, doc_id):
    return f"/{urllib.parse.quote(index, safe='')}/_doc/{urllib.parse.quote(doc_id, safe='')}"


def main():
    source, target = Endpoint("SOURCE"), Endpoint("TARGET")
    scope = os.environ.get("SOURCE_INDEX")
    search_path = f"/{scope}/_search" if scope else "/_search"

    hits, total = [], None
    while total is None or len(hits) < total:
        size = min(PAGE, MAX_WINDOW - len(hits))
        if size <= 0:
            sys.exit(f"more than {MAX_WINDOW} documents: copy per index (SOURCE_INDEX) instead")
        status, body = source.call("POST", search_path,
                                   {"size": size, "from": len(hits), "query": {"match_all": {}}})
        if status != 200:
            sys.exit(f"source search -> {status}: {str(body)[:500]}")
        batch = body["hits"]["hits"]
        total = body["hits"]["total"]["value"]
        hits.extend(batch)
        if not batch:
            break
    per_index = {}
    for h in hits:
        per_index[h["_index"]] = per_index.get(h["_index"], 0) + 1
    print(f"source total={total} fetched={len(hits)} per_index={per_index}", flush=True)
    if len(hits) != total or len({(h["_index"], h["_id"]) for h in hits}) != total:
        sys.exit("pagination mismatch — aborting before any write")
    if os.environ.get("DRY_RUN") == "1":
        print("DRY_RUN: no writes")
        return

    created = updated = 0
    for h in hits:
        status, body = target.call("PUT", doc_path(h["_index"], h["_id"]), h["_source"])
        if status not in (200, 201):
            sys.exit(f"target PUT {h['_index']}/{h['_id']} -> {status}: {str(body)[:300]}")
        created += status == 201
        updated += status == 200
    print(f"target writes: created={created} updated={updated}", flush=True)

    bad = 0
    for h in hits:
        status, body = target.call("GET", doc_path(h["_index"], h["_id"]))
        if status != 200 or body.get("_source") != h["_source"]:
            bad += 1
            print(f"MISMATCH {h['_index']}/{h['_id']} status={status}")
    print(f"verify: {len(hits) - bad}/{len(hits)} identical", flush=True)
    sys.exit(1 if bad else 0)


main()
