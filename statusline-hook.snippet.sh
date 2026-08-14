# --- claude-usage-tray hook: tees rate_limits to the tray's cache file ---
# Paste after `input=$(cat)` in your statusline script. Requires jq. Data only
# refreshes while Claude Code is running a session; safe no-op if jq is
# missing or rate_limits is absent (writes nothing rather than garbage).
if command -v jq >/dev/null 2>&1; then
  cache_dir="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
  cache_file="$cache_dir/usage-tray-cache.json"
  rate_limits=$(printf '%s' "$input" | jq -c '.rate_limits // empty' 2>/dev/null)
  if [ -n "$rate_limits" ]; then
    mkdir -p "$cache_dir" 2>/dev/null
    tmp_file=$(mktemp "$cache_dir/.usage-tray-cache.XXXXXX" 2>/dev/null) && {
      jq -n --argjson rl "$rate_limits" '{written_at: now | floor, rate_limits: $rl}' \
        > "$tmp_file" 2>/dev/null && mv -f "$tmp_file" "$cache_file" || rm -f "$tmp_file"
    } || :
  fi
fi
: # ensure the snippet always exits 0, even when jq is missing
# --- end claude-usage-tray hook ---
