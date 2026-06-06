#!/usr/bin/env bash
#
# Deploy the OpenAlex roaringrange demo to its S3 bucket + CloudFront.
#
# Usage:
#   ./deploy.sh                  deploy web assets (html/js/wasm/svg) + invalidate
#   ./deploy.sh --data DIR       ALSO upload the built index/record files from DIR
#   ./deploy.sh --splits DIR     ALSO upload a split set (.rrss + per-split .rrs/.rrf) from DIR
#   BUCKET=… DISTRIBUTION=… ./deploy.sh   override the defaults below
#
# The wasm reader (roaringrange.js + roaringrange_bg.wasm) is uploaded under
# content-hashed names (roaringrange.<hash>.js / roaringrange.<hash>_bg.wasm,
# immutable) and the HTML is rewritten to reference them, then served no-cache. So
# a reader rebuild always gets fresh URLs and a cached HTML can never pair with a
# mismatched reader (no stale-import errors). Content-types are explicit: .wasm
# needs application/wasm for streaming compilation, ES-module .js a JS MIME type.
#
# The data files (openalex-47m.{rrs,rrf}, records-47m.{idx,bin}) total ~20 GB,
# rarely change, and are built locally by `go run . …` (see main.go). They are
# left untouched unless --data DIR is given, and even then only changed files
# upload. Their cache is never invalidated (versioned, effectively immutable),
# so a deploy never churns the multi-GB objects the demo range-reads constantly.
set -euo pipefail

BUCKET="${BUCKET:-openalex-eve}"
DISTRIBUTION="${DISTRIBUTION:-E3H4W2Y0UYDT7E}"
CACHE="${CACHE:-public, max-age=300}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WEB="$HERE/web"

DATA_DIR=""
SPLITS_DIR=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --data) DATA_DIR="${2:?--data needs a directory}"; shift 2 ;;
    --splits) SPLITS_DIR="${2:?--splits needs a directory}"; shift 2 ;;
    -h|--help) sed -n '3,11p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

sync_ct() { # content-type, then sync globs that follow as --include patterns
  local ct="$1"; shift
  local args=(--exclude "*")
  local g; for g in "$@"; do args+=(--include "$g"); done
  aws s3 sync "$WEB/" "s3://$BUCKET/" "${args[@]}" \
    --content-type "$ct" --cache-control "$CACHE"
}

# Content hash of the wasm reader (JS + wasm), so a rebuild gets fresh asset URLs
# and a cached HTML can never pair with a mismatched reader.
ASSET_HASH="$(cat "$WEB/roaringrange.js" "$WEB/roaringrange_bg.wasm" | shasum -a 256 | cut -c1-10)"
HTMLCACHE="no-cache"                               # entry HTML always revalidates
ASSETCACHE="public, max-age=31536000, immutable"   # hashed reader names never change

echo "==> web assets -> s3://$BUCKET/ (reader hash $ASSET_HASH)"
sync_ct "image/svg+xml" "*.svg"

# Reader under content-hashed names (immutable); the HTML below points at these.
aws s3 cp "$WEB/roaringrange.js" "s3://$BUCKET/roaringrange.$ASSET_HASH.js" \
  --content-type "text/javascript; charset=utf-8" --cache-control "$ASSETCACHE"
aws s3 cp "$WEB/roaringrange_bg.wasm" "s3://$BUCKET/roaringrange.${ASSET_HASH}_bg.wasm" \
  --content-type "application/wasm" --cache-control "$ASSETCACHE"

# Rewrite each HTML page to reference the hashed reader, then upload it no-cache so
# the browser always picks up the current build (and its matching reader).
for h in index.html how-it-works.html splitset.html; do
  [[ -f "$WEB/$h" ]] || continue
  tmp_html="$(mktemp)"
  sed -e "s|\./roaringrange\.js|./roaringrange.$ASSET_HASH.js|g" \
      -e "s|roaringrange_bg\.wasm|roaringrange.${ASSET_HASH}_bg.wasm|g" \
      "$WEB/$h" > "$tmp_html"
  aws s3 cp "$tmp_html" "s3://$BUCKET/$h" \
    --content-type "text/html; charset=utf-8" --cache-control "$HTMLCACHE"
  rm -f "$tmp_html"
done

if [[ -n "$DATA_DIR" ]]; then
  echo "==> data files <- $DATA_DIR (large; only changed files upload)"
  for f in openalex-47m.rrs openalex-47m.rrf records-47m.idx records-47m.bin; do
    if [[ -f "$DATA_DIR/$f" ]]; then
      aws s3 cp "$DATA_DIR/$f" "s3://$BUCKET/$f" \
        --cache-control "public, max-age=31536000, immutable"
    else
      echo "   skip $f (not in $DATA_DIR)"
    fi
  done
fi

# Split-set artifacts: the `.rrss` manifest, the per-split `.rrs`/`.rrf` files, and the split
# set's OWN record store `*-records.{idx,bin}` (all built by `openalex-build -split-set -out DIR`).
# Uploaded under the `openalex-split/` prefix so its `openalex-records.{idx,bin}` never collides
# with a same-corpus monolith's records at the root, and to match the URLs in splitset.html.
# Versioned/immutable like the monolith data files, so the cache is never invalidated. (The doc
# ids match the monolith's ONLY when both are built over the same corpus, since both rank by
# cited_by_count via the same rank_rows — but each reader uses its own record store regardless.)
if [[ -n "$SPLITS_DIR" ]]; then
  echo "==> split-set artifacts <- $SPLITS_DIR -> s3://$BUCKET/openalex-split/ (immutable; only changed files upload)"
  aws s3 sync "$SPLITS_DIR/" "s3://$BUCKET/openalex-split/" \
    --exclude "*" --include "*.rrss" --include "*-s*.rrs" --include "*-s*.rrt" \
    --include "*-s*.rrf" --include "*-records.idx" --include "*-records.bin" \
    --cache-control "public, max-age=31536000, immutable"
fi

echo "==> invalidating HTML on $DISTRIBUTION (hashed reader + data left cached)"
aws cloudfront create-invalidation \
  --distribution-id "$DISTRIBUTION" \
  --paths /index.html /how-it-works.html /splitset.html \
  --query "Invalidation.{Id:Id,Status:Status}" --output table

echo "==> done — https://openalex.evefreeman.com/"
