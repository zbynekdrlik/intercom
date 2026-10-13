#!/usr/bin/env bash
# airuleset:script-ok source-only lib (defines functions + constants only, no top-level statements) --
# the scripts/lib/*.sh convention: sourcing runs in the CALLER's shell, so strict mode here would leak.
#
# scripts/lib/strih-intercom-hub-unit.sh -- issue 1412: the intercom-hub unit per box ROLE, and verify
# item 16's crash-loop grade; issue 1424: verify item 9's grade of the hub's HDMI audio legs.
#
# The base systemd/intercom-hub.service loads the Janus room secret with an unconditional
#   LoadCredential=janus-room.secret:/etc/intercom-hub/janus-room.secret
# (issue 1345: DynamicUser cannot read the root 0600 file directly). setup-strih step 14 (Janus) creates
# that file, but the stream role skips step 14 (obs_box_step_applies), so on stream-pp (11.10.2026) the
# file never existed: `Failed to set up credentials` -> status 243/CREDENTIALS, 425 restarts, and the
# resolume-pp VBAN -> strih-program sink -> OBS program audio path was dead. The hub of such a box has
# no Janus participant and never reads the secret, so its role drop-in RESETS the credential list (an
# empty LoadCredential= clears every earlier one; the live workaround that brought the hub up, NRestarts 0).
# A box that runs step 14 keeps the base unit's credential and has the drop-in removed.

STRIH_HUB_CRED_DROPIN_NAME="20-no-janus-credential.conf"

# strih_intercom_hub_cred_dropin_text -> the role drop-in for a box without the Janus phones leg.
strih_intercom_hub_cred_dropin_text() {
  cat <<'DROPIN'
# intercom-hub on a box without the Janus phones leg (issue 1412) -- installed by setup-strih step 13.
# This box runs no setup-strih step 14, so /etc/intercom-hub/janus-room.secret does not exist and the
# base unit's LoadCredential= fails the start (243/CREDENTIALS). Its hub matrix has no Janus participant
# and never reads the secret. The empty assignment resets the credential list. Do NOT edit by hand.
[Service]
LoadCredential=
DROPIN
}

# strih_intercom_hub_cred_dropin_apply DROPIN_DIR -> write the drop-in on a box that skips step 14 (only
# when its bytes differ, through a temp file + rename), remove a leftover on a box that runs it. Prints
# one line. rc 1 + a stderr line on a failed write.
strih_intercom_hub_cred_dropin_apply() {
  local dir="${1:?drop-in dir required}" f tmp why
  f="${dir%/}/${STRIH_HUB_CRED_DROPIN_NAME}"
  if why="$(obs_box_step_applies 14)"; then
    if [ -e "$f" ] || [ -L "$f" ]; then
      rm -f "$f" || { echo "strih-intercom-hub-unit: cannot remove ${f}" >&2; return 1; }
      echo "  removed ${f}: this box runs the Janus leg, the hub loads its room credential"
    fi
    return 0
  fi
  mkdir -p "$dir" || { echo "strih-intercom-hub-unit: cannot create ${dir}" >&2; return 1; }
  if [ -f "$f" ] && [ ! -L "$f" ] && cmp -s "$f" <(strih_intercom_hub_cred_dropin_text); then
    echo "  ${f} unchanged (${why})"
    return 0
  fi
  tmp="$(mktemp "${dir%/}/.${STRIH_HUB_CRED_DROPIN_NAME}.XXXXXX")" || { echo "strih-intercom-hub-unit: cannot write into ${dir}" >&2; return 1; }
  if ! { strih_intercom_hub_cred_dropin_text > "$tmp" && chmod 0644 "$tmp" && mv -f "$tmp" "$f"; }; then
    rm -f "$tmp"
    echo "strih-intercom-hub-unit: cannot write ${f}" >&2
    return 1
  fi
  echo "  wrote ${f}: the hub runs without the Janus room credential (${why})"
}

# strih_intercom_hub_cred_grade DROPIN_DIR -> verify item 16's role row through the caller's ok / bad /
# note: a box that skips step 14 must carry the drop-in byte-identical (else the hub cannot start); a box
# that runs it must not carry it and must have the room secret file. Always rc 0.
strih_intercom_hub_cred_grade() {
  local dir="${1:?drop-in dir required}" f why
  f="${dir%/}/${STRIH_HUB_CRED_DROPIN_NAME}"
  if why="$(obs_box_step_applies 14)"; then
    if [ -e "$f" ]; then
      bad "(intercom-hub) ${f} resets the Janus credential on a box that runs the Janus leg -- re-run setup-strih.sh step 13"
    elif [ -f "${STRIH_HUB_JANUS_SECRET:-/etc/intercom-hub/janus-room.secret}" ]; then
      ok "(intercom-hub) the Janus room credential the unit loads is present"
    else
      bad "(intercom-hub) ${STRIH_HUB_JANUS_SECRET:-/etc/intercom-hub/janus-room.secret} missing -- the unit's LoadCredential= fails the hub (243/CREDENTIALS); re-run setup-strih.sh step 14"
    fi
  elif [ -f "$f" ] && cmp -s "$f" <(strih_intercom_hub_cred_dropin_text); then
    ok "(intercom-hub) ${f} resets the Janus credential (${why})"
  else
    bad "(intercom-hub) ${f} missing or drifted (${why}) -- the base unit's LoadCredential= fails the hub (243/CREDENTIALS); re-run setup-strih.sh step 13"
  fi
  return 0
}

# _strih_hub_show_field TEXT KEY -> the value of KEY= in a `systemctl show` text ('' when absent).
_strih_hub_show_field() {
  sed -n "/^${2}=/{s/^${2}=//p;q}" <<<"$1"
}

# strih_intercom_hub_loop_verdict SAMPLE1 SAMPLE2 -> rc 0 + the reason on stdout when the hub is
# crash-looping between the two `systemctl show -p ActiveState,SubState,NRestarts,Result` samples:
# either sample `failed`, either in `auto-restart`, or NRestarts climbing. rc 1 (nothing printed) for a
# steady, an inactive (enable-only) or an unreadable unit. Pure.
strih_intercom_hub_loop_verdict() {
  local s1="${1-}" s2="${2-}" s a sub n r n1 n2
  for s in "$s1" "$s2"; do
    a="$(_strih_hub_show_field "$s" ActiveState)"
    sub="$(_strih_hub_show_field "$s" SubState)"
    n="$(_strih_hub_show_field "$s" NRestarts)"
    r="$(_strih_hub_show_field "$s" Result)"
    if [ "$a" = failed ]; then
      printf 'failed (Result=%s, NRestarts=%s)' "${r:-?}" "${n:-?}"
      return 0
    fi
    if [ "$sub" = auto-restart ]; then
      printf 'crash-looping (%s auto-restart, Result=%s, NRestarts=%s)' "${a:-?}" "${r:-?}" "${n:-?}"
      return 0
    fi
  done
  n1="$(_strih_hub_show_field "$s1" NRestarts)"
  n2="$(_strih_hub_show_field "$s2" NRestarts)"
  if [[ "$n1" =~ ^[0123456789]+$ ]] && [[ "$n2" =~ ^[0123456789]+$ ]] && [ "$n2" -gt "$n1" ]; then
    printf 'restarting (NRestarts %s -> %s within %ss)' "$n1" "$n2" "${STRIH_HUB_LOOP_WINDOW_S:-6}"
    return 0
  fi
  return 1
}

# strih_intercom_hub_crash_loop -> verify item 16's live read: sample the unit, and when it is not plainly
# inactive sample it again after STRIH_HUB_LOOP_WINDOW_S (default 6 s: longer than the unit's
# RestartSec=5, so one restart lands inside it). rc 0 + the reason when it crash-loops, rc 1 otherwise.
strih_intercom_hub_crash_loop() {
  local q=(show -p ActiveState -p SubState -p NRestarts -p Result intercom-hub) s1 s2
  s1="$(systemctl "${q[@]}" 2>/dev/null || true)"
  s2="$s1"
  case "$(_strih_hub_show_field "$s1" ActiveState)" in
    active|activating|failed|deactivating)
      sleep "${STRIH_HUB_LOOP_WINDOW_S:-6}"
      s2="$(systemctl "${q[@]}" 2>/dev/null || true)" ;;
  esac
  strih_intercom_hub_loop_verdict "$s1" "$s2"
}

# strih_intercom_hub_hdmi_leg_grade STATE_URL -> verify item 9's rows for the hub's HDMI audio legs
# (issue 1424 part 3, the strih-pp vestibule on HDMI-1) through the caller's ok / bad / note: one row per
# `alsa_hdmi` participant on the hub's /api/state. PASS = resolved to its hw PCM, aplay running, blocks
# written; FAIL names why the leg is silent (the hub's own reason). A hub without such a participant
# prints nothing; an unreadable hub is a NOTE (item 16 FAILs a dead hub). STRIH_HUB_HDMI_STATE_FILE reads
# the JSON from a file instead (the test seam). Always rc 0.
strih_intercom_hub_hdmi_leg_grade() {
  local url="${1:?hub state url required}" json rows row
  if [ -n "${STRIH_HUB_HDMI_STATE_FILE:-}" ]; then
    json="$(cat -- "$STRIH_HUB_HDMI_STATE_FILE" 2>/dev/null || true)"
  else
    json="$(curl -fsS --max-time 3 "$url" 2>/dev/null || true)"
  fi
  if [ -z "$json" ]; then
    note "(hdmi-leg) the intercom hub state at ${url} is not readable -- the HDMI audio legs are graded when the hub answers"
    return 0
  fi
  rows="$(printf '%s' "$json" | python3 -c '
import json, sys
try:
    ps = json.load(sys.stdin).get("participants", [])
except Exception as e:
    print("FAIL\tthe intercom hub state is not JSON (%s)" % e)
    sys.exit(0)
for p in ps:
    if p.get("adapter") != "alsa_hdmi":
        continue
    h = p.get("hdmi") or {}
    la = p.get("local_audio") or {}
    name, conn = p.get("name", "?"), h.get("connector", "?")
    if not h.get("resolved"):
        print("FAIL\t%s on %s is silent: %s -- journalctl -u intercom-hub names it (issue 1424)" % (name, conn, h.get("reason", "no hdmi facet")))
    elif not h.get("running"):
        print("FAIL\t%s on %s resolved to %s but aplay is not running (a busy PCM?) -- journalctl -u intercom-hub (issue 1424)" % (name, conn, h.get("hw")))
    elif not la.get("tx_blocks"):
        print("FAIL\t%s on %s (%s): aplay runs but no audio block was written yet" % (name, conn, h.get("hw")))
    else:
        print("OK\t%s on %s plays on %s (pipe %s frames, xruns %s)" % (name, conn, h.get("hw"), la.get("pipe_depth_frames", 0), h.get("xruns", 0)))
' 2>&1 || true)"
  while IFS= read -r row; do
    case "$row" in
      OK$'\t'*) ok "(hdmi-leg) ${row#*$'\t'}" ;;
      FAIL$'\t'*) bad "(hdmi-leg) ${row#*$'\t'}" ;;
      '') ;;
      *) bad "(hdmi-leg) grading the hub state failed: ${row}" ;;
    esac
  done <<<"$rows"
  return 0
}
