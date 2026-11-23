#!/bin/bash
# Watch the cameramen's Discord thread (intercom#36, .claude/rules/discord-thread.md) for a tag of
# the bot: poll `read --mentions-only` every POLL seconds and EXIT the moment a mention arrives
# (printing it), so a background run wakes the session. Bounded by MAX_S; a Discord error exits 2.
# Run it through airuleset's `secret exec` (the token is only ever injected):
#   python3 ~/devel/airuleset/airuleset.py secret exec --file ~/.secrets/discord-shared-bot \
#       --env DISCORD_BOT_TOKEN -- bash scripts/discord_watch.sh
set -euo pipefail
cd "$(dirname "$0")/.."
POLL="${POLL:-30}"
MAX_S="${MAX_S:-36000}"
end=$((SECONDS + MAX_S))
while [ "$SECONDS" -lt "$end" ]; do
  if ! out=$(python3 scripts/discord_thread.py read --mentions-only 2>&1); then
    echo "$(date '+%F %T') discord read failed: $out"
    exit 2
  fi
  if printf '%s\n' "$out" | grep -qE '^\[[0-9]+\] '; then
    echo "$(date '+%F %T') MENTION"
    printf '%s\n' "$out"
    exit 0
  fi
  sleep "$POLL"
done
echo "$(date '+%F %T') no mention within ${MAX_S}s"
