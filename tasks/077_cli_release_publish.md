# 077 — Publish the roaringrange CLI (secret + release tag)

Pending final step to make the `cmd/roaringrange` CLI installable. All the wiring is
already committed (task 076 + commit c9a8536): `.goreleaser.yaml` (builds
macOS/Linux/Windows × amd64/arm64 binaries + checksums + a Homebrew cask) and
`.github/workflows/release-cli.yml` (runs GoReleaser on a `v*` tag, separate from the
PyPI `release.yml`). The tap repo `freeeve/homebrew-tap` already exists (public, has
`Formula/ptop.rb`; the cask lands at `Casks/roaringrange.rb` and coexists).

`go install github.com/freeeve/roaringrange/cmd/roaringrange@<tag>` works with none of
the below — it builds from source at the tag.

## What's left (both are user-triggered)

1. **Add the tap-push secret** `HOMEBREW_TAP_GITHUB_TOKEN` on `freeeve/roaringrange`
   (only needed for the brew cask; binaries + go install work without it). The default
   Actions `GITHUB_TOKEN` can't write to the separate tap repo, so this needs a token
   with `contents:write` on `freeeve/homebrew-tap`:
   - Recommended: a fine-grained PAT scoped to `homebrew-tap` only, Contents: R/W, then
     `gh secret set HOMEBREW_TAP_GITHUB_TOKEN -R freeeve/roaringrange` (paste at prompt).
   - Quick/broad: `gh auth token | gh secret set HOMEBREW_TAP_GITHUB_TOKEN -R freeeve/roaringrange`
     (reuses the personal gh token — broader scope; swap for a scoped PAT later).

2. **Cut the release tag.** `cmd/roaringrange` postdates `v0.28.0`, so a new tag is
   required for `@latest`/brew to see it. ⚠ The same tag ALSO publishes a PyPI wheel of
   that version (shared `release.yml`), so bump intentionally:
   `git tag v0.29.0 && git push origin v0.29.0`.

## Verify after tagging

- GH Release has 6 archives + `checksums.txt`.
- `go install github.com/freeeve/roaringrange/cmd/roaringrange@v0.29.0 && roaringrange version`.
- With the secret set: `Casks/roaringrange.rb` updated in the tap; `brew install freeeve/tap/roaringrange`.
- Config was validated locally with `goreleaser check` + a full `--snapshot` release
  (all binaries, archives, checksums, cask generated OK).
