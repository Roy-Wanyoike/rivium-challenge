#!/usr/bin/env bash
# Resolve the submission folder from a workflow_dispatch input or a PR title
# of the form "[app-name]: ...". Non-submission PRs (no title prefix) skip.
set -euo pipefail

app=""
skip=false

if [[ -n "${DISPATCH_APP:-}" ]]; then
  app="$DISPATCH_APP"
elif [[ "${PR_TITLE:-}" =~ ^\[([^]]+)\] ]]; then
  app="${BASH_REMATCH[1]}"
else
  echo "Pull request title has no [app-name] prefix. Skipping APK build."
  skip=true
fi

if [[ "$skip" != "true" ]]; then
  # Folder name only: no slashes, traversal, or whitespace.
  if [[ ! "$app" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
    echo "::error::Invalid submission folder name: ${app}"
    exit 1
  fi

  if [[ ! -d "$app" ]]; then
    echo "::error::Submission folder '${app}' does not exist."
    exit 1
  fi

  if [[ ! -f "$app/build.sh" ]]; then
    echo "::error::Missing ${app}/build.sh. Each submission must provide its own build script."
    exit 1
  fi

  echo "Submission: ${app}"
fi

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  echo "skip=${skip}" >> "$GITHUB_OUTPUT"
  echo "name=${app}" >> "$GITHUB_OUTPUT"
fi
