#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

bad_files=()
while IFS= read -r -d '' file; do
  bad_files+=("$file")
done < <(
  find . \
    \( -path './.git' -o -path './cli-proxy-api/.git' -o -path './kiro-rs/.git' \) -prune -o \
    -type f \
    \( \
      -name 'config.json' -o \
      -name 'credentials.json' -o \
      -name 'credentials.*.json' -o \
      -name 'config.yaml' -o \
      -name 'secrets.env' -o \
      -name '.env' -o \
      -name 'kiro_balance_cache.json' -o \
      -name 'kiro_stats.json' \
    \) -print0
)

if ((${#bad_files[@]} > 0)); then
  echo "Refusing to publish: runtime secret/config files are present:" >&2
  printf '  %s\n' "${bad_files[@]}" >&2
  exit 1
fi

if rg --pcre2 -n \
  -e 'sk-kiro-rs-[A-Za-z0-9_-]{12,}' \
  -e 'sk-(cpa-pro|cpa-admin|kiro-admin)-[0-9a-fA-F]{32,}' \
  -e 'GO''CSPX-[A-Za-z0-9_-]+' \
  -e '[0-9]+-[A-Za-z0-9_-]+\.apps\.googleusercontent\.com' \
  -e 'localhost:3128/oauth/callback\?[^[:space:]]*code=[0-9a-fA-F-]{36}' \
  -e 'workflowStateHandle=[0-9a-fA-F-]{36}' \
  -e '"(access_token|refresh_token|id_token|accessToken|refreshToken)"[[:space:]]*:[[:space:]]*"[A-Za-z0-9._~+/=-]{20,}"' \
  -g '!go.sum' -g '!Cargo.lock' -g '!scripts/sanitize-secrets.sh' \
  -g '!**/node_modules/**' -g '!**/target/**' .; then
  echo "Refusing to publish: known local secret material found." >&2
  exit 1
fi

echo "Secret scan passed."
