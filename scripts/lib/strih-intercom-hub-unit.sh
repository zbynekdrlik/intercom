#!/usr/bin/env bash
# airuleset:script-ok source-only lib (defines functions + constants only, no top-level statements) --
# the scripts/lib/*.sh convention: sourcing runs in the CALLER's shell, so strict mode here would leak.
#
# scripts/lib/strih-intercom-hub-unit.sh -- issue 1412: the intercom-hub unit per box ROLE, and verify
# item 16's crash-loop grade; issue 1424: verify item 9's grade of the hub's HDMI audio legs; issue
# 1401: verify item 16's grade of the hub's memory lock; issue 1467: verify item 16's hub version vs the
# checkout + the decode_rejects note.
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
        print("FAIL\t%s on %s resolved to %s but aplay is not playing (a busy PCM, or it never read) -- journalctl -u intercom-hub (issue 1424)" % (name, conn, h.get("hw")))
    elif not la.get("tx_blocks"):
        print("FAIL\t%s on %s (%s): aplay plays but no audio block was written yet" % (name, conn, h.get("hw")))
    else:
        print("OK\t%s on %s plays on %s (pipe %s frames, xruns %s)" % (name, conn, h.get("hw"), la.get("pipe_depth_frames", 0), h.get("xruns", 0)))
' 2>&1 || true)"
  # The leg runs aplay + amixer (alsa-utils, setup-strih step 4b): a box without them never plays.
  if [ -n "$rows" ] && ! { command -v aplay && command -v amixer; } >/dev/null 2>&1; then
    bad "(hdmi-leg) aplay/amixer missing -- apt-get install alsa-utils (setup-strih.sh step 4b, issue 1424)"
  fi
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

# strih_intercom_hub_memlock_grade STATE_URL -> verify item 16's memory-lock row (issue 1401, design
# 6265012279) through the caller's ok / bad / note. On 14.10.2026 stream-pp ran out of memory and the
# unlocked hub's SCHED_FIFO hub-mix thread stalled in page faults (711 lost ticks in 6 s). The hub locks
# its memory before it starts hub-mix and publishes the outcome as `memory_locked` on /api/state: a JSON
# true PASSes; false FAILs (the running hub's unit has no LimitMEMLOCK=infinity, or the hub was not
# restarted since setup-strih step 13 installed it); a hub that does not report it FAILs (a binary from
# before the lock); an unreadable hub is a NOTE (the item's unit rows grade a dead hub).
# STRIH_HUB_MEMLOCK_STATE_FILE reads the JSON from a file instead (the test seam). Always rc 0.
strih_intercom_hub_memlock_grade() {
  local url="${1:?hub state url required}" json row
  if [ -n "${STRIH_HUB_MEMLOCK_STATE_FILE:-}" ]; then
    json="$(cat -- "$STRIH_HUB_MEMLOCK_STATE_FILE" 2>/dev/null || true)"
  else
    json="$(curl -fsS --max-time 3 "$url" 2>/dev/null || true)"
  fi
  if [ -z "$json" ]; then
    note "(intercom-hub) the hub state at ${url} is not readable -- its memory lock is graded when the hub answers"
    return 0
  fi
  row="$(printf '%s' "$json" | python3 -c '
import json, sys
try:
    state = json.load(sys.stdin)
except Exception as e:
    print("FAIL\tthe intercom hub state is not JSON (%s)" % e)
    sys.exit(0)
if not isinstance(state, dict):
    print("FAIL\tthe intercom hub state is not a JSON object")
elif "memory_locked" not in state:
    print("FAIL\tthe running hub does not report memory_locked (a binary from before issue 1401) -- deploy the current intercom-hub and restart it")
elif state["memory_locked"] is True:
    print("OK\tmemory locked (mlockall): the hub-mix thread never waits on a page fault")
else:
    print("FAIL\tmemory NOT locked (memory_locked=%s): under memory pressure hub-mix stalls in page faults (issue 1401) -- the unit needs LimitMEMLOCK=infinity, then systemctl restart intercom-hub; journalctl -u intercom-hub names the errno" % json.dumps(state["memory_locked"]))
' 2>&1 || true)"
  case "$row" in
    OK$'\t'*) ok "(intercom-hub) ${row#*$'\t'}" ;;
    FAIL$'\t'*) bad "(intercom-hub) ${row#*$'\t'}" ;;
    *) bad "(intercom-hub) grading the hub memory lock failed: ${row:-no output}" ;;
  esac
  return 0
}

# strih_intercom_hub_version_grade BASE_URL BOX [ROOT] [BIN] -> verify item 16's version + decode rows
# (issue 1467) through the caller's ok / note; never a FAIL, always rc 0. 23.10.2026 the FOH console's
# 24-bit VBAN stream was dropped silently by a hub binary built before the 24-bit decode, and a fresh
# strih installed main's hub, older than its checkout. So:
#   * the RUNNING hub's version (BASE_URL/api/version), else the version setup-strih recorded next to
#     the binary (BIN.version, default /usr/local/bin/intercom-hub), against the checkout's workspace
#     version (strih_checkout_version ROOT, default the checkout this lib lives in): OLDER = a NOTE with the reinstall line (the old binary is
#     moved aside, never deleted: setup never replaces an installed hub); the same or newer = a PASS;
#     nothing readable = a NOTE;
#   * a VBAN participant with decode_rejects > 0 on /api/state (a known stream the hub cannot decode)
#     = a NOTE naming the legs; an unreadable state adds nothing (the memory-lock row reports it).
# STRIH_HUB_VERSION_JSON_FILE / STRIH_HUB_DECODE_STATE_FILE read the JSON from files (the test seams).
strih_intercom_hub_version_grade() {
  local base="${1:?hub base url required}" box="${2:-<box>}"
  local root="${3:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
  local bin="${4:-/usr/local/bin/intercom-hub}" want vjson recorded="" sjson rows row tmp
  want="$(strih_checkout_version "$root" || true)"
  if [ -n "${STRIH_HUB_VERSION_JSON_FILE:-}" ]; then
    vjson="$(cat -- "$STRIH_HUB_VERSION_JSON_FILE" 2>/dev/null || true)"
  else
    vjson="$(curl -fsS --max-time 3 "${base}/api/version" 2>/dev/null || true)"
  fi
  [ -f "${bin}.version" ] && recorded="$(sed -n 's/^version=//p' "${bin}.version" 2>/dev/null | head -n 1 || true)"
  if [ -n "${STRIH_HUB_DECODE_STATE_FILE:-}" ]; then
    sjson="$(cat -- "$STRIH_HUB_DECODE_STATE_FILE" 2>/dev/null || true)"
  else
    sjson="$(curl -fsS --max-time 3 "${base}/api/state" 2>/dev/null || true)"
  fi
  # The two JSON answers go to python as FILES (an environment string is capped at 128 KB, issue 1467
  # review); python reads its program from the heredoc.
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/strih-hub-version.XXXXXX" 2>/dev/null)" || {
    note "(intercom-hub) the hub version is not graded: no temp dir"
    return 0
  }
  printf '%s' "$vjson" > "$tmp/version.json" 2>/dev/null || true
  printf '%s' "$sjson" > "$tmp/state.json" 2>/dev/null || true
  rows="$(python3 - "$want" "$recorded" "$bin" "$root" "$box" "$tmp/version.json" "$tmp/state.json" <<'PYHUBV' 2>&1 || true
import json, re, sys
want, recorded, binp, root, box, vfile, sfile = sys.argv[1:8]

def load(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return None

def key(v):
    m = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?", v or "")
    if not m:
        return None
    pre = m.group(4)
    ids = []
    for part in (pre.split(".") if pre else []):
        ids.append((0, int(part), "") if part.isdigit() else (1, 0, part))
    # a release sorts after every pre-release of the same triple (semver)
    return (int(m.group(1)), int(m.group(2)), int(m.group(3)), 0 if pre else 1, ids)

running = ""
try:
    running = str(load(vfile)["version"])
except Exception:
    running = ""
have, source = (running, "runs") if running else (recorded if recorded not in ("", "unknown") else "", "recorded")
if not have:
    print("NOTE\tthe hub version is not readable (no /api/version answer, no recorded %s.version) -- not graded against this checkout's %s" % (binp, want or "unknown version"))
elif key(want) is None or key(have) is None:
    print("NOTE\tthe hub %s %s, this checkout is %s -- the versions cannot be compared" % (source, have, want or "unknown"))
elif key(have) < key(want):
    verb = "runs" if source == "runs" else "is installed (recorded version, the hub does not answer)"
    print("NOTE\tthe hub %s %s, OLDER than this checkout's %s -- a VBAN format it cannot decode reads as a silent leg (issue 1467); reinstall: sudo mv %s %s.pre-%s && sudo GH_TOKEN=<token> %s/scripts/setup-strih.sh --box %s (step 13 installs the checkout's hub), then sudo systemctl restart intercom-hub"
          % (verb, have, want, binp, binp, have, root, box))
else:
    print("OK\thub %s %s (this checkout %s)" % ("runs" if source == "runs" else "recorded", have, want))

try:
    state = load(sfile)
    legs = [(p.get("name", "?"), int(p.get("decode_rejects") or 0)) for p in state.get("participants", [])
            if isinstance(p, dict)]
except Exception:
    legs = []
bad = ["%s=%d" % (n, c) for n, c in legs if c > 0]
if bad:
    print("NOTE\tdecode_rejects %s -- a known VBAN stream the hub cannot decode (its packets are dropped; journalctl -u intercom-hub names the stream, sender and reason, issue 1467)" % " ".join(bad))
PYHUBV
)"
  rm -rf -- "$tmp" 2>/dev/null || true
  while IFS= read -r row; do
    case "$row" in
      OK$'\t'*) ok "(intercom-hub) ${row#*$'\t'}" ;;
      NOTE$'\t'*) note "(intercom-hub) ${row#*$'\t'}" ;;
      '') ;;
      *) note "(intercom-hub) grading the hub version failed: ${row}" ;;
    esac
  done <<<"$rows"
  return 0
}

# --- issue 1439: the stream role runs no intercom hub ------------------------------------------------
# Design revision 6338127636 (owner 22.10.2026: "naco tam je dalsia vrstva?"): on the stream role the
# console's VBAN program goes straight into the OBS obs-vban source, so setup-strih step 13 installs no
# hub there and moves a hub an earlier run left behind aside (never deleted: the files stay under
# STRIH_HUB_ASIDE_DIR/<UTC stamp>/<their absolute path>), and verify item 16 grades "no hub". The paths
# are seams for the tests (STRIH_HUB_UNIT_FILE / _DROPIN_DIR / _BIN / _CONFIG_DIR / _ASIDE_DIR).

# strih_intercom_hub_leftovers -> the hub files present on this box, one path per line (nothing = none).
strih_intercom_hub_leftovers() {
  local p
  for p in "${STRIH_HUB_UNIT_FILE:-/etc/systemd/system/intercom-hub.service}" \
           "${STRIH_HUB_DROPIN_DIR:-/etc/systemd/system/intercom-hub.service.d}" \
           "${STRIH_HUB_BIN:-/usr/local/bin/intercom-hub}" "${STRIH_HUB_CONFIG_DIR:-/etc/intercom-hub}"; do
    if [ -e "$p" ] || [ -L "$p" ]; then printf '%s\n' "$p"; fi
  done
}

# strih_intercom_hub_remove -> setup-strih step 13 on a box that runs no hub: stop + disable a running /
# enabled hub (BEFORE its unit file moves, so the WantedBy link goes with it), then move every hub file
# aside, then daemon-reload. Idempotent: a box without a hub prints one line and touches nothing. rc 1 +
# a stderr line when a stop / disable / move fails (nothing after it is done).
strih_intercom_hub_remove() {
  local aside="${STRIH_HUB_ASIDE_DIR:-/var/lib/camera-box/intercom-hub-removed}" stamp p dest moved=0 state
  # Every state with a job or a process is stopped, not only "active" (`is-active --quiet`): a restart
  # loop reads activating, and a hub still shutting down can hold the console's UDP port.
  state="$(systemctl is-active intercom-hub 2>/dev/null || true)"
  case "$state" in
    active|activating|reloading|deactivating)
      systemctl stop intercom-hub || { echo "strih-intercom-hub-unit: could not stop intercom-hub.service" >&2; return 1; }
      echo "  stopped intercom-hub.service (${state}; the stream role runs no hub, issue 1439)" ;;
  esac
  if systemctl is-enabled --quiet intercom-hub 2>/dev/null; then
    systemctl disable intercom-hub || { echo "strih-intercom-hub-unit: could not disable intercom-hub.service" >&2; return 1; }
    echo "  disabled intercom-hub.service"
  fi
  stamp="$(date -u +%Y%m%dT%H%M%SZ)"
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    dest="${aside%/}/${stamp}${p}"
    if ! { mkdir -p "$(dirname "$dest")" && mv -T -- "$p" "$dest"; }; then
      echo "strih-intercom-hub-unit: could not move ${p} aside to ${dest}" >&2; return 1
    fi
    echo "  moved ${p} aside to ${dest}"
    moved=1
  done < <(strih_intercom_hub_leftovers)
  if [ "$moved" = 1 ]; then
    systemctl daemon-reload || { echo "strih-intercom-hub-unit: daemon-reload failed after moving the hub aside" >&2; return 1; }
  else
    echo "  no intercom hub on this box -- nothing to remove"
  fi
  return 0
}

# strih_intercom_hub_absent_grade WHY -> verify item 16 on a box that runs no hub (WHY = the step gate's
# reason) through the caller's ok / bad: PASS when no hub file is left and the unit is not running, FAIL
# naming what is left (a running hub keeps the console's UDP port from the OBS vban source). Always rc 0.
strih_intercom_hub_absent_grade() {
  local why="${1-}" left state
  left="$(strih_intercom_hub_leftovers | tr '\n' ' ')"
  state="$(systemctl is-active intercom-hub 2>/dev/null || true)"
  case "$state" in active|activating|reloading|deactivating) left="${left}(intercom-hub.service ${state}) " ;; esac
  if [ -z "$left" ]; then
    ok "(intercom-hub) no intercom hub on this box (${why})"
  else
    bad "(intercom-hub) this box runs no hub (${why}), but ${left% } is still there -- re-run setup-strih.sh step 13, then restart OBS (issue 1439)"
  fi
  return 0
}
