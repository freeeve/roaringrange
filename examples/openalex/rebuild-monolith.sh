#!/usr/bin/env bash
# Rebuild the v3 trigram monolith (openalex-full.rrs) + geo split from the S3-LIVE
# records, so doc-id == records position lines up with records-full/term/vector again
# (task 044). Runs locally and is resumable (downloads skip already-present files;
# the builder reuses cached chunk partials).
#
#   ./rebuild-monolith.sh           # phase A: download -> build -> VERIFY -> slice, then STOP
#   ./rebuild-monolith.sh --upload  # phase B: upload overwrite + CloudFront invalidate + live check
#
# Phase A never touches S3. Phase B overwrites the live monolith + split — run it only
# after phase A's verify printed OK and the local spot-check looks right.
set -euo pipefail

export AWS_PROFILE=openalex-admin
BUCKET=openalex-eve
DIST=E3H4W2Y0UYDT7E
N=${N:-484369476}                       # total docs (builder caps at store.len())
CHUNK_DOCS=${CHUNK_DOCS:-8000000}       # ~2 GB partial / chunk; 8M is safe at 128 GiB RAM
SAMPLES=${SAMPLES:-500}                  # alignment-verify sample count
WORK=${WORK:-$HOME/oa-rebuild}
RUST=/Users/efreeman/roaringrange/rust

mkdir -p "$WORK"

bin() { echo "$RUST/target/release/examples/$1"; }

upload_phase() {
  [ -f "$WORK/openalex-full.rrs" ] || { echo "no $WORK/openalex-full.rrs — run phase A first" >&2; exit 1; }
  ls "$WORK/out-geo"/*.rrss >/dev/null 2>&1 || { echo "no sliced split in $WORK/out-geo — run phase A first" >&2; exit 1; }
  echo "== uploading monolith ($(stat -f%z "$WORK/openalex-full.rrs") bytes) =="
  aws s3 cp "$WORK/openalex-full.rrs" "s3://$BUCKET/openalex-full.rrs" \
    --cache-control "public, max-age=31536000, immutable"
  echo "== uploading geo split ($(ls "$WORK/out-geo" | wc -l | tr -d ' ') files) =="
  aws s3 cp "$WORK/out-geo/" "s3://$BUCKET/openalex-trigram-geo/" --recursive \
    --cache-control "public, max-age=31536000, immutable"
  echo "== invalidating CloudFront =="
  aws cloudfront create-invalidation --distribution-id "$DIST" \
    --paths "/openalex-full.rrs" "/openalex-trigram-geo/*" "/index.html" >/dev/null
  echo "== live spot-check (give the invalidation a minute) =="
  curl -s -G "https://openalex.evefreeman.com/search" --data-urlencode "q=roaring bitmap" | head -c 240; echo
  echo "done — verify titles in the demo; compare /search ids vs /search-term."
}

if [ "${1:-}" = "--upload" ]; then upload_phase; exit 0; fi

# ---- phase A ---------------------------------------------------------------

# 1. Download the LIVE records (this is the correctness guarantee: build from the same
#    order-A records everything else uses). Skip a file already present at full size.
dl() {  # key dst
  local key=$1 dst=$2 want
  want=$(aws s3api head-object --bucket "$BUCKET" --key "$key" --query ContentLength --output text)
  if [ -f "$WORK/$dst" ] && [ "$(stat -f%z "$WORK/$dst")" = "$want" ]; then
    echo "have $dst ($want bytes)"; return
  fi
  echo "downloading $key -> $dst ($want bytes)…"
  aws s3 cp --no-progress "s3://$BUCKET/$key" "$WORK/$dst"
}
dl records-full.idx records-full.idx
dl openalex-full.dict openalex-full.dict
dl records-full.bin records-full.bin           # 115 GiB — the long one

# 2. Build the example binaries, then the monolith (resumable via cached partials).
echo "== building example binaries =="
( cd "$RUST" && cargo build --release --features zstd \
    --example build_trigram_monolith --example verify_monolith_aligned )
( cd "$RUST" && cargo build --release --features splits --example slice_trigram_monolith )

echo "== building monolith ($N docs, ${CHUNK_DOCS}-doc chunks) =="
"$(bin build_trigram_monolith)" \
  "$WORK/records-full.idx" "$WORK/records-full.bin" "$WORK/openalex-full.dict" \
  "$N" "$WORK/openalex-full.rrs" "$CHUNK_DOCS" "$WORK/openalex-full.rrwork"

# 3. GATE: doc-id alignment vs the same live records. `set -e` aborts before slice/upload
#    on a non-zero exit (MISALIGNED).
echo "== verifying alignment (the gate) =="
"$(bin verify_monolith_aligned)" \
  "$WORK/openalex-full.rrs" "$WORK/records-full.idx" "$WORK/records-full.bin" \
  "$WORK/openalex-full.dict" "$SAMPLES"

# 4. Slice the geo split from the verified monolith.
echo "== slicing geo split =="
rm -rf "$WORK/out-geo"; mkdir -p "$WORK/out-geo"
"$(bin slice_trigram_monolith)" \
  "$WORK/openalex-full.rrs" "$WORK/out-geo" openalex 2000000 32000000 "$N"

cat <<EOF

== phase A complete ==
monolith: $WORK/openalex-full.rrs ($(stat -f%z "$WORK/openalex-full.rrs") bytes)
split:    $WORK/out-geo ($(ls "$WORK/out-geo" | wc -l | tr -d ' ') files)
Alignment verify passed. Review, then upload with:
  ./rebuild-monolith.sh --upload
Note: openalex-global.bloom is referenced by the demo split config but is currently
404 — decide whether to generate it during the slice or drop the bloom: reference.
EOF
