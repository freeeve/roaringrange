#!/usr/bin/env bash
#
# ec2-full-build.sh — build the OpenAlex roaringrange index ON an EC2 instance,
# then upload the artifact set to the demo's S3 bucket.
#
# This is an ON-INSTANCE recipe: launch a box yourself, get this repo onto it
# (git clone, or scp the checkout), and run this script there. It installs the
# toolchain, builds the Rust builder, runs it with the new flags (zstd records +
# abstracts + DOI lookup), uploads the six output files, and — when asked —
# self-terminates. Every knob is an env var with a sane default.
#
# -----------------------------------------------------------------------------
# Two intended runs
# -----------------------------------------------------------------------------
# 1) CHEAP DRY-RUN (~1M works, iron out the pipeline) — a small spot box, stream
#    a bounded number of shards straight from the public bucket, keep outputs in
#    a throwaway S3 prefix, do NOT self-terminate:
#
#      Instance: c7g.4xlarge (arm64) or m7i.2xlarge spot, ~50 GB gp3, us-east-1.
#      STREAM=1 MAXFILES=120 CHUNKS=4 \
#      IDX_PREFIX=openalex-dry REC_PREFIX=records-dry S3_PREFIX=dry/ \
#      TERMINATE=0 ./ec2-full-build.sh
#
#    MAXFILES bounds how many source shards are read (so it bounds cost); the
#    resulting work count is whatever those shards hold (shard density varies —
#    tune MAXFILES up/down to land near a target). STREAM=1 reads shards over
#    HTTPS with no local download.
#
# 2) FULL BUILD (~492M works) — the big box, sync the corpus to local NVMe once
#    (chunked builds re-read it per chunk, so local beats re-streaming), then
#    build chunked + zstd and self-terminate:
#
#      Instance: i4i.16xlarge spot (512 GB RAM, ~14 TB NVMe), us-east-1.
#      Launch with --instance-initiated-shutdown-behavior terminate so the final
#      `shutdown` ends the spot run. Instance profile must allow s3:PutObject on
#      the demo bucket (reads use --no-sign-request, so no creds needed for them).
#
#      STREAM=0 CHUNKS=8 TERMINATE=1 ./ec2-full-build.sh
#
#    zstd records now compose with CHUNKS>1 (the dictionary is trained from a
#    sample gathered during the chunk passes), so the cheap 512 GB box can build
#    the compressed store in bounded memory — no single-pass whole-index-in-RAM.
#
# -----------------------------------------------------------------------------
# Env vars (all optional)
# -----------------------------------------------------------------------------
#   BUCKET=openalex-eve        destination bucket for the artifacts
#   S3_PREFIX=                 key prefix under the bucket (e.g. dry/ for a test)
#   WORKS_SRC=s3://openalex/data/works   public OpenAlex Works snapshot
#   WORKDIR=/data              scratch dir for the repo clone, corpus, outputs
#   REPO=https://github.com/freeeve/roaringrange.git   clone source (if needed)
#   IDX_PREFIX=openalex-full   basename for .rrs/.rrf/.dict/.rril
#   REC_PREFIX=records-full    basename for the record store .bin/.idx
#   CHUNKS=8                   doc-ID chunks (bounds peak RAM; >1 needs the disk)
#   ABSTRACT_CAP=2000          stored-abstract byte cap (0 omits abstracts)
#   ZSTD_LEVEL=19  DICT_SIZE=114688   record-store zstd settings
#   MAXFILES=0                 cap input shards (0 = all); the dry-run knob
#   LIMIT=0                    cap ranked works (0 = all)
#   STREAM=0                   1 = stream shards from S3; 0 = sync to WORKDIR first
#   UPLOAD=1                   upload the six outputs to the bucket
#   TERMINATE=0                shutdown the instance when done (full unattended run)
#   AWS_DEFAULT_REGION=us-east-1
#
set -euo pipefail

BUCKET="${BUCKET:-openalex-eve}"
S3_PREFIX="${S3_PREFIX:-}"
WORKS_SRC="${WORKS_SRC:-s3://openalex/data/works}"
WORKDIR="${WORKDIR:-/data}"
REPO="${REPO:-https://github.com/freeeve/roaringrange.git}"
IDX_PREFIX="${IDX_PREFIX:-openalex-full}"
REC_PREFIX="${REC_PREFIX:-records-full}"
CHUNKS="${CHUNKS:-8}"
ABSTRACT_CAP="${ABSTRACT_CAP:-2000}"
ZSTD_LEVEL="${ZSTD_LEVEL:-19}"
DICT_SIZE="${DICT_SIZE:-114688}"
MAXFILES="${MAXFILES:-0}"
LIMIT="${LIMIT:-0}"
STREAM="${STREAM:-0}"
UPLOAD="${UPLOAD:-1}"
TERMINATE="${TERMINATE:-0}"
export AWS_DEFAULT_REGION="${AWS_DEFAULT_REGION:-us-east-1}"

# cloud-init/user-data may invoke this without HOME set; rustup and cargo both
# need it, and `set -u` would otherwise abort on the first $HOME reference. Fall
# back to the current user's passwd home (bash expands bare ~ from passwd when
# HOME is unset), then to /root.
[[ -n "${HOME:-}" ]] || export HOME="$( (cd ~ && pwd) 2>/dev/null || echo /root)"

log() { echo "==> $*"; }

# Installs the build toolchain (Rust, a C compiler for libzstd/ring, git, awscli)
# when missing. Supports dnf (Amazon Linux 2023) and apt (Ubuntu/Debian).
install_deps() {
  if command -v dnf >/dev/null 2>&1; then
    sudo dnf -y install gcc gcc-c++ clang git tar gzip unzip >/dev/null 2>&1 || true
  elif command -v apt-get >/dev/null 2>&1; then
    sudo apt-get update -y >/dev/null 2>&1 || true
    sudo apt-get install -y build-essential clang git curl tar gzip unzip >/dev/null 2>&1 || true
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    log "installing Rust toolchain via rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  fi
  # shellcheck disable=SC1091
  [[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
}

# Ensures the AWS CLI v2 is present — needed only to sync the corpus locally
# (STREAM=0) or to upload outputs (UPLOAD=1); a streaming build-only run needs no
# AWS CLI. Installs from the official zip when missing.
ensure_aws() {
  command -v aws >/dev/null 2>&1 && return 0
  log "installing AWS CLI v2" >&2
  local arch zip
  arch="$(uname -m)"
  if [[ "$arch" == "aarch64" ]]; then zip="awscli-exe-linux-aarch64.zip"; else zip="awscli-exe-linux-x86_64.zip"; fi
  curl -fsSL "https://awscli.amazonaws.com/$zip" -o /tmp/awscliv2.zip
  ( cd /tmp && unzip -q -o awscliv2.zip && sudo ./aws/install --update )
  command -v aws >/dev/null 2>&1 || { echo "error: AWS CLI install failed" >&2; exit 1; }
}

# Resolves the builder crate dir: use this checkout if the script lives inside the
# repo, otherwise clone REPO into WORKDIR. Echoes the builder crate path.
ensure_repo() {
  local here builder
  here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  builder="$here/builder"
  if [[ -f "$builder/Cargo.toml" ]]; then
    echo "$builder"; return
  fi
  mkdir -p "$WORKDIR"
  if [[ ! -d "$WORKDIR/roaringrange/.git" ]]; then
    log "cloning $REPO -> $WORKDIR/roaringrange" >&2
    git clone --depth 1 "$REPO" "$WORKDIR/roaringrange" >&2
  fi
  echo "$WORKDIR/roaringrange/examples/openalex/builder"
}

# Builds the release builder binary in $1 (the crate dir). Echoes the binary path.
build_binary() {
  local crate="$1"
  log "cargo build --release ($crate)" >&2
  ( cd "$crate" && cargo build --release >&2 )
  echo "$crate/target/release/openalex-build"
}

# Echoes the -in argument: a bounded S3 stream when STREAM=1, else a local glob,
# syncing the corpus into WORKDIR first (only the .gz data, no manifest).
resolve_input() {
  if [[ "$STREAM" == "1" ]]; then
    echo "$WORKS_SRC/"
    return
  fi
  local dest="$WORKDIR/oa-works"
  # Always sync: `aws s3 sync` is incremental, so a completed corpus costs one
  # listing pass, while skipping on a mere [[ -d ]] would silently build from a
  # partial corpus whenever an earlier sync was interrupted mid-transfer.
  ensure_aws >&2
  log "syncing corpus $WORKS_SRC -> $dest (no-sign-request, incremental)" >&2
  mkdir -p "$dest"
  aws s3 sync "$WORKS_SRC/" "$dest/" --no-sign-request \
    --exclude "*" --include "*.gz" >&2
  echo "$dest/*/*.gz"
}

# Runs the builder with the configured flags, writing the six outputs into OUTDIR.
run_build() {
  local bin="$1" in_arg="$2" outdir="$3"
  mkdir -p "$outdir"
  local args=(
    -in "$in_arg"
    -chunks "$CHUNKS"
    -abstract-cap "$ABSTRACT_CAP"
    -records-zstd -zstd-level "$ZSTD_LEVEL" -dict-size "$DICT_SIZE"
    -rrs "$outdir/$IDX_PREFIX.rrs"
    -facets "$outdir/$IDX_PREFIX.rrf"
    -bin "$outdir/$REC_PREFIX.bin"
    -idx "$outdir/$REC_PREFIX.idx"
    -dict "$outdir/$IDX_PREFIX.dict"
    -lookup "$outdir/$IDX_PREFIX.rril"
  )
  [[ "$MAXFILES" != "0" ]] && args+=(-maxfiles "$MAXFILES")
  [[ "$LIMIT" != "0" ]] && args+=(-limit "$LIMIT")
  log "building index: ${args[*]}"
  "$bin" "${args[@]}"
}

# Uploads the six artifacts to s3://BUCKET/S3_PREFIX with immutable caching (they
# are versioned by name and read by the demo via byte-range requests).
upload_outputs() {
  local outdir="$1" f
  [[ "$UPLOAD" == "1" ]] || { log "UPLOAD=0, skipping S3 upload"; return; }
  ensure_aws
  log "uploading outputs -> s3://$BUCKET/$S3_PREFIX"
  for f in "$IDX_PREFIX.rrs" "$IDX_PREFIX.rrf" "$IDX_PREFIX.dict" "$IDX_PREFIX.rril" \
           "$REC_PREFIX.bin" "$REC_PREFIX.idx"; do
    aws s3 cp "$outdir/$f" "s3://$BUCKET/$S3_PREFIX$f" \
      --cache-control "public, max-age=31536000, immutable"
  done
}

# Shuts the instance down when TERMINATE=1 (terminating it if the instance was
# launched with --instance-initiated-shutdown-behavior terminate).
maybe_terminate() {
  [[ "$TERMINATE" == "1" ]] || return 0
  log "TERMINATE=1 — shutting down"
  sudo shutdown -h now
}

main() {
  install_deps
  local crate bin in_arg outdir
  crate="$(ensure_repo)"
  bin="$(build_binary "$crate")"
  in_arg="$(resolve_input)"
  outdir="$WORKDIR/out"
  run_build "$bin" "$in_arg" "$outdir"
  log "outputs in $outdir:"
  ls -lh "$outdir"
  upload_outputs "$outdir"
  log "done"
  maybe_terminate
}

main "$@"
