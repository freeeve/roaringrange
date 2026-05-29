#!/usr/bin/env bash
#
# download.sh — fetch a SUBSET of the OpenAlex Works snapshot for the
# roaringrange search demo.
#
# The OpenAlex bulk snapshot is public (CC0) and lives in the `openalex` S3
# bucket, no AWS account required (use --no-sign-request). The Works entity is
# laid out as:
#
#   s3://openalex/data/works/manifest
#   s3://openalex/data/works/updated_date=YYYY-MM-DD/<NNNN>_part_<NN>.gz
#
# Each .gz is gzip-compressed JSON Lines — one Work per line. The full Works
# dump is ~330 GB compressed (~1.6 TB decompressed) and is refreshed quarterly.
# Docs: https://developers.openalex.org/download-all-data/snapshot-data-format
#
# This script pulls only the FIRST K partition folders (sorted by date) into
# $DEST, which is plenty to exercise the demo without the full download.
#
# Requirements: awscli v2 (`aws --version`). No credentials needed.
#
# Usage:
#   ./download.sh                 # first 1 partition -> /tmp/openalex/works
#   PARTITIONS=3 ./download.sh    # first 3 partitions
#   DEST=/data/oa ./download.sh   # custom destination
#
# After download, build the index with:
#   cd /Users/efreeman/rr-e2e
#   go run ./openalexbuild -in "/tmp/openalex/works/*/*.gz" -limit 2000000
#
set -euo pipefail

# Number of leading updated_date partitions to download (smallest subset = 1).
PARTITIONS="${PARTITIONS:-1}"
# Local destination for the works/ subtree.
DEST="${DEST:-/tmp/openalex/works}"
# Source prefix in the public bucket.
SRC="s3://openalex/data/works"

if ! command -v aws >/dev/null 2>&1; then
  echo "error: aws CLI not found. Install awscli v2 first." >&2
  exit 1
fi

mkdir -p "$DEST"

echo "Listing partitions under $SRC/ ..."
# The works/ prefix contains one PRE (common prefix) per updated_date=... folder
# plus the manifest. Grab the partition folder names, sorted, take the first K.
mapfile -t parts < <(
  aws s3 ls "$SRC/" --no-sign-request \
    | awk '/ PRE / {print $2}' \
    | sed 's:/$::' \
    | sort \
    | head -n "$PARTITIONS"
)

if [ "${#parts[@]}" -eq 0 ]; then
  echo "error: no partitions found under $SRC/ (is awscli configured for v2?)" >&2
  exit 1
fi

echo "Downloading ${#parts[@]} partition(s): ${parts[*]}"
for p in "${parts[@]}"; do
  echo "  -> $p"
  # Recursive copy of one partition; only the .gz data files (no manifest).
  aws s3 cp "$SRC/$p/" "$DEST/$p/" \
    --no-sign-request \
    --recursive \
    --exclude "*" \
    --include "*.gz"
done

echo
echo "Done. Files under: $DEST"
du -sh "$DEST" 2>/dev/null || true
echo
echo "Next: build the index"
echo "  cd /Users/efreeman/rr-e2e"
echo "  go run ./openalexbuild -in \"$DEST/*/*.gz\" -limit 2000000"

# -----------------------------------------------------------------------------
# Pulling the FULL Works snapshot (disk: ~330 GB compressed, ~1.6 TB raw)
# -----------------------------------------------------------------------------
# To mirror the entire Works entity instead of a subset, sync the whole prefix:
#
#   aws s3 sync "s3://openalex/data/works/" "$DEST/" \
#     --no-sign-request --exclude "*" --include "*.gz"
#
# Or the complete snapshot (all entities, ~hundreds of GB more):
#
#   aws s3 sync "s3://openalex/data/" /data/openalex/ --no-sign-request
#
# Plan for >330 GB free for the compressed Works dump alone. The openalexbuild
# loader reads the .gz files directly (streaming, gzip JSON Lines) — there is no
# need to decompress to disk. Use -limit on openalexbuild to cap memory/output
# while developing; popularity ranking (cited_by_count desc) keeps the most
# relevant works at the head regardless of the cap.
