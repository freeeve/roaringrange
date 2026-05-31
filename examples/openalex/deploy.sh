#!/usr/bin/env bash
#
# Deploy the OpenAlex roaringrange demo to its S3 bucket + CloudFront.
#
# Usage:
#   ./deploy.sh                  deploy web assets (html/js/wasm/svg) + invalidate
#   ./deploy.sh --data DIR       ALSO upload the built index/record files from DIR
#   BUCKET=… DISTRIBUTION=… ./deploy.sh   override the defaults below
#
# Web assets (this dir's web/) are small and change every build, so they are
# always synced. Content-types are set explicitly because the defaults matter:
# .wasm must be application/wasm for streaming compilation, and ES-module .js
# needs a JavaScript MIME type.
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
while [[ $# -gt 0 ]]; do
  case "$1" in
    --data) DATA_DIR="${2:?--data needs a directory}"; shift 2 ;;
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

echo "==> web assets -> s3://$BUCKET/"
sync_ct "text/html; charset=utf-8"       "*.html"
sync_ct "text/javascript; charset=utf-8" "*.js"
sync_ct "application/wasm"               "*.wasm"
sync_ct "image/svg+xml"                  "*.svg"

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

echo "==> invalidating web paths on $DISTRIBUTION (data objects left cached)"
aws cloudfront create-invalidation \
  --distribution-id "$DISTRIBUTION" \
  --paths /index.html /how-it-works.html "/*.js" "/*.wasm" "/*.svg" \
  --query "Invalidation.{Id:Id,Status:Status}" --output table

echo "==> done — https://openalex.evefreeman.com/"
