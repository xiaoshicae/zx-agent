#!/usr/bin/env bash
# Assemble already verified packages into a draft. Published releases are immutable here.
set -euo pipefail

: "${GH_REPO:?GH_REPO is required}"
: "${RELEASE_TAG:?RELEASE_TAG is required}"
: "${RUNNER_TEMP:?RUNNER_TEMP is required}"

# Listing distinguishes an absent release from authentication or network failures.
# Pagination also finds old tags when rerunning a release.
gh api --paginate --slurp "repos/$GH_REPO/releases?per_page=100" > "$RUNNER_TEMP/releases.json"
release_state="$(jq -r --arg tag "$RELEASE_TAG" '
  [.[][] | select(.tag_name == $tag)] |
  if length == 0 then "missing"
  elif length == 1 and .[0].draft then "draft"
  else "published"
  end
' "$RUNNER_TEMP/releases.json")"

case "$release_state" in
  missing)
    gh release create "$RELEASE_TAG" --repo "$GH_REPO" --draft --verify-tag \
      --title "$RELEASE_TAG" --generate-notes
    ;;
  draft)
    ;;
  *)
    echo "Release $RELEASE_TAG is already published; refusing to replace its assets." >&2
    exit 1
    ;;
esac

# Only draft assets may be replaced, so a failed run can be retried before publication.
gh release upload "$RELEASE_TAG" dist/* --repo "$GH_REPO" --clobber
release_url="$(gh release view "$RELEASE_TAG" --repo "$GH_REPO" --json url --jq .url)"
printf 'Release draft: %s\n' "$release_url"
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  printf 'Release draft: %s\n' "$release_url" >> "$GITHUB_STEP_SUMMARY"
fi
