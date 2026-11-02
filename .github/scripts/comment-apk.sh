#!/usr/bin/env bash
# Download the built debug APK, commit it to branch apk/pr-<n> in this
# repository at app-name/debug.apk, and comment the file link on the PR.
# A separate branch avoids pushing to the PR head (which would re-trigger
# the build) and works for forked pull requests.
set -euo pipefail

: "${GITHUB_REPOSITORY:?}"
: "${WORKFLOW_CONCLUSION:?}"
: "${WORKFLOW_RUN_ID:?}"
: "${HEAD_REPO:?}"
: "${HEAD_BRANCH:?}"
: "${GH_TOKEN:?}"

marker="<!-- apk-artifact -->"
run_url="https://github.com/${GITHUB_REPOSITORY}/actions/runs/${WORKFLOW_RUN_ID}"
head_sha_short="${HEAD_SHA:-unknown}"
head_sha_short="${head_sha_short:0:7}"

owner="${HEAD_REPO%%/*}"
pr_number=$(
  gh api "repos/${GITHUB_REPOSITORY}/pulls?head=${owner}:${HEAD_BRANCH}&state=open" \
    --jq '.[0].number // empty'
)
if [[ -z "$pr_number" ]]; then
  echo "No open pull request for ${owner}:${HEAD_BRANCH}."
  exit 0
fi

comment_and_exit() {
  local body="$1"
  if [[ "${DRY_RUN:-0}" == "1" ]]; then
    echo "Would comment on #${pr_number}:"
    printf '%s\n' "$body"
    exit 0
  fi
  local existing
  existing=$(
    gh api --paginate "repos/${GITHUB_REPOSITORY}/issues/${pr_number}/comments" \
      --jq ".[] | select(.body | contains(\"${marker}\")) | .id" \
      | head -n 1
  )
  if [[ -n "$existing" ]]; then
    gh api --method PATCH "repos/${GITHUB_REPOSITORY}/issues/comments/${existing}" \
      -f body="$body" >/dev/null
    echo "Updated comment ${existing} on #${pr_number}."
  else
    gh api --method POST "repos/${GITHUB_REPOSITORY}/issues/${pr_number}/comments" \
      -f body="$body" >/dev/null
    echo "Commented on #${pr_number}."
  fi
  exit 0
}

if [[ "$WORKFLOW_CONCLUSION" != "success" ]]; then
  comment_and_exit "$(cat <<EOF
${marker}
The APK build failed. Log: ${run_url}
EOF
)"
fi

tmpdir=$(mktemp -d)
work=""
cleanup() {
  rm -rf "$tmpdir"
  [[ -n "$work" ]] && rm -rf "$work"
}
trap cleanup EXIT

if ! gh run download "$WORKFLOW_RUN_ID" \
  --repo "$GITHUB_REPOSITORY" \
  --name debug-apk \
  --dir "$tmpdir"; then
  comment_and_exit "$(cat <<EOF
${marker}
The APK build finished without a debug-apk artifact. Log: ${run_url}
EOF
)"
fi

meta=$(find "$tmpdir" -name apk-meta.env -type f | head -n 1 || true)
if [[ -z "$meta" ]]; then
  comment_and_exit "$(cat <<EOF
${marker}
The APK artifact was missing metadata. Log: ${run_url}
EOF
)"
fi

# shellcheck disable=SC1090
source "$meta"
: "${APP_NAME:?apk-meta.env must set APP_NAME}"

apk=$(find "$tmpdir" -type f -name 'debug.apk' | head -n 1 || true)
if [[ -z "$apk" || ! -s "$apk" ]]; then
  comment_and_exit "$(cat <<EOF
${marker}
The APK artifact did not contain debug.apk. Log: ${run_url}
EOF
)"
fi

review_branch="apk/pr-${pr_number}"
work=$(mktemp -d)

git clone --depth 1 "https://x-access-token:${GH_TOKEN}@github.com/${GITHUB_REPOSITORY}.git" "$work"
cd "$work"

git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"

if git ls-remote --exit-code --heads origin "$review_branch" >/dev/null 2>&1; then
  git fetch --depth 1 origin "$review_branch"
  git checkout -B "$review_branch" FETCH_HEAD
else
  git checkout --orphan "$review_branch"
  git rm -rf . >/dev/null 2>&1 || true
fi

mkdir -p "$APP_NAME"
cp "$apk" "$APP_NAME/debug.apk"
git add -f "$APP_NAME/debug.apk"

if git diff --cached --quiet; then
  echo "APK unchanged on ${review_branch}."
  commit_sha=$(git rev-parse HEAD)
else
  git commit -m "Add ${APP_NAME}/debug.apk for pull request #${pr_number}"
  git push -u origin "HEAD:${review_branch}" --force
  commit_sha=$(git rev-parse HEAD)
fi

apk_url="https://github.com/${GITHUB_REPOSITORY}/raw/${commit_sha}/${APP_NAME}/debug.apk"
blob_url="https://github.com/${GITHUB_REPOSITORY}/blob/${commit_sha}/${APP_NAME}/debug.apk"

comment_and_exit "$(cat <<EOF
${marker}
Debug APK is ready for \`${APP_NAME}\`:

- Download: ${apk_url}
- In repo: ${blob_url}

Built from [\`${HEAD_BRANCH}\`](${run_url}) (\`${head_sha_short}\`).
EOF
)"
