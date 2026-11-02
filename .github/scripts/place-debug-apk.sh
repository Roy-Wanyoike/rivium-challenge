#!/usr/bin/env bash
# Ensure ${APP_NAME}/debug.apk exists after build.sh, then write apk-meta.env
# for the follow-up comment workflow.
set -euo pipefail

: "${APP_NAME:?}"

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

target="${APP_NAME}/debug.apk"

if [[ ! -f "$target" ]]; then
  first_apk=""
  while IFS= read -r apk; do
    [[ -n "$apk" ]] || continue
    first_apk="$apk"
    break
  done < <(find "$APP_NAME" -type f -name '*.apk' ! -path '*/.*' | sort)

  if [[ -z "$first_apk" ]]; then
    echo "::error::${APP_NAME}/build.sh did not produce an APK. Write one to ${target}."
    exit 1
  fi
  echo "Using ${first_apk} as ${target}"
  cp "$first_apk" "$target"
fi

test -s "$target"

cat > apk-meta.env <<EOF
APP_NAME=${APP_NAME}
PR_NUMBER=${PR_NUMBER:-}
EOF

echo "APK ready: ${target} ($(wc -c < "$target") bytes)"
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  echo "path=${target}" >> "$GITHUB_OUTPUT"
fi
