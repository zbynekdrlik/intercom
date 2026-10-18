#!/usr/bin/env bash
# airuleset:script-ok source-only lib (defines the issue-1453 constants, the setup-strih step 16i install
# and the verify-strih item 45 rows; no top-level statements besides constants and the lib sources) --
# the sibling scripts/lib/*.sh convention of NOT setting `set -euo pipefail` here: sourcing runs in the
# CALLER's shell, so strict mode would leak into it. Every function checks its own return codes.
#
# scripts/lib/strih-interkom-public.sh -- issue 1453 (owner 18.10.2026: the web intercom must work for a
# cameraman on MOBILE DATA, served from the strih box itself, never through dev1), design
# issuecomment-6250479059 (Approach 1). The venue uplink is mobile data behind CGNAT, so nothing at the
# venue can accept an inbound connection:
#
#   * the page + the Janus WebSocket: an OUTBOUND Cloudflare Tunnel. `cloudflared-interkom.service` runs
#     the official `cloudflared` package (Cloudflare's apt repo, its signing key pinned by sha256) with
#     the tunnel's connector token from /etc/cloudflared/interkom-tunnel.token (0600 root, handed to the
#     service as a systemd credential, never in argv, never printed, never committed). The tunnel's
#     public hostnames (the intercom names -> https://127.0.0.1:443, originServerName = the first name)
#     are configured on the Cloudflare side by the supervisor, so the box needs only the token. Public
#     DNS for the names becomes the tunnel's CNAME; the venue routers keep answering the private
#     address (split horizon), so a phone on the venue WiFi stays on the LAN.
#   * the media: Cloudflare Realtime TURN. The intercom hub mints short-lived credentials from the key
#     file /etc/intercom-hub/turn.toml (0600 root) on `/api/ice` (the PWA) and `/api/turn-rest` (Janus's
#     `nat.turn_rest_api`). The hub runs as the operator, so the key file reaches it as the systemd
#     credential `turn.toml` (the intercom-hub drop-in below), and janus.jcfg's nat section gets the
#     managed TURN REST block (scripts/janus_turn_rest_cfg.py).
#
#   * strih_interkom_public_install -- setup-strih step 16i (as root), after strih_https_install, gated on
#     the box fact STRIH_HTTPS_PUBLIC (`no` = the box publishes nothing: a turn drop-in / Janus block left
#     from earlier is removed, a leftover tunnel unit is named). On `yes`: cloudflared from the official
#     repo (only when missing; the key's sha256 checked before it is trusted); the token written from
#     CLOUDFLARE_TUNNEL_TOKEN when the caller exports it (never argv); the unit written only on a
#     difference, enabled, started / restarted only with a token present (a missing token = ONE WARN
#     naming the supervisor step, never a failure of the rest of setup); the hub drop-in + the Janus
#     block when the TURN key file exists (a missing key = ONE WARN). A running hub is never restarted
#     (provisioning rule): a changed drop-in prints the restart the supervisor owes. A running Janus is
#     restarted when its jcfg changed (step 14 restarts it on every run anyway).
#   * strih_interkom_public_token_store -- the token on STDIN -> the token file (0600 root, compare-then-
#     write). The supervisor's runbook pipes it from the secret store over ssh stdin.
#   * strih_interkom_public_grade_rows / _report -- verify-strih item 45, two rows:
#       (https-tunnel)  the token file (owner 600), the unit = its render, enabled + active, and
#                       cloudflared's own metrics /ready = 200 with at least one connection;
#       (https-turn)    the key file (owner 600), the hub drop-in = its render, janus.jcfg's managed
#                       block, and the hub's /api/turn-rest = 200 with a turn: uri;
#     both NOTE on a box whose fact is `no`.
#
# Test seams (tests/python/test_strih_interkom_public_1453.py): STRIH_PUBLIC_TOKEN_FILE, _UNIT_DIR,
# _KEYRING_DIR, _APT_DIR, _TURN_FILE, _HUB_DROPIN_DIR, _JANUS_JCFG, _OWNER, _KEY_SHA256; apt-get,
# dpkg-query, systemctl, curl, python3 and sha256sum are looked up on PATH.

if [ -z "${STRIH_HTTPS_HUB_UPSTREAM:-}" ]; then
  # shellcheck source=scripts/lib/strih-https.sh
  . "$(dirname "${BASH_SOURCE[0]}")/strih-https.sh"
fi

STRIH_PUBLIC_UNIT="cloudflared-interkom.service"
STRIH_PUBLIC_METRICS="127.0.0.1:20241"
STRIH_PUBLIC_KEY_URL="https://pkg.cloudflare.com/cloudflare-public-v2.gpg"
# sha256 of the signing key read 18.10.2026 (primary CC94B39C77AE7342A68B89628A682D308D4E5E73,
# "CloudFlare Software Packaging 2025"); a rotated key fails loud here until the pin is moved on purpose.
STRIH_PUBLIC_KEY_PIN="1bd95f4082b320d541bee351560fc2765aa9f9cd8efa4c9e32135e63f252721d"
STRIH_PUBLIC_REPO="https://pkg.cloudflare.com/cloudflared any main"
STRIH_PUBLIC_TURN_CRED="turn.toml"
STRIH_PUBLIC_DROPIN_NAME="30-turn-credential.conf"

strih_public_token_file() { printf '%s' "${STRIH_PUBLIC_TOKEN_FILE:-/etc/cloudflared/interkom-tunnel.token}"; }
strih_public_unit_path() { printf '%s/%s' "${STRIH_PUBLIC_UNIT_DIR:-/etc/systemd/system}" "$STRIH_PUBLIC_UNIT"; }
strih_public_keyring_path() { printf '%s/cloudflare-public-v2.gpg' "${STRIH_PUBLIC_KEYRING_DIR:-/usr/share/keyrings}"; }
strih_public_apt_list_path() { printf '%s/cloudflared.list' "${STRIH_PUBLIC_APT_DIR:-/etc/apt/sources.list.d}"; }
strih_public_turn_file() { printf '%s' "${STRIH_PUBLIC_TURN_FILE:-/etc/intercom-hub/turn.toml}"; }
strih_public_dropin_path() {
  printf '%s/%s' "${STRIH_PUBLIC_HUB_DROPIN_DIR:-/etc/systemd/system/intercom-hub.service.d}" "$STRIH_PUBLIC_DROPIN_NAME"
}
strih_public_janus_jcfg() { printf '%s' "${STRIH_PUBLIC_JANUS_JCFG:-/etc/janus/janus.jcfg}"; }
strih_public_turn_rest_url() { printf '%s/api/turn-rest' "$STRIH_HTTPS_HUB_UPSTREAM"; }
strih_public_janus_tool() { printf '%s/../janus_turn_rest_cfg.py' "$(dirname "${BASH_SOURCE[0]}")"; }

# strih_public_unit_text -> the cloudflared-interkom unit. The token reaches the process as a systemd
# credential (%d = its credentials directory), so it is never in argv or in this file; the condition
# keeps the unit from failing on a box whose token is not placed yet. Type=simple, never notify: a
# notify unit is "starting" until cloudflared reaches Cloudflare, so with the venue uplink down a
# blocking start (and multi-user.target, which the kiosk session waits for) would never finish.
strih_public_unit_text() {
  local tok
  tok="$(strih_public_token_file)"
  cat <<UNIT
# ${STRIH_PUBLIC_UNIT} -- written by setup-strih.sh step 16i (scripts/lib/strih-interkom-public.sh,
# camera-box issue 1453). Do NOT edit by hand.
# Publishes this strih box's intercom page (nginx :443) to phones on mobile data through an OUTBOUND
# Cloudflare Tunnel. The public hostnames and the origin are configured on the Cloudflare side; the box
# holds only the connector token (0600 root, loaded as a credential, never in argv).
[Unit]
Description=Cloudflare Tunnel: the intercom page for phones on mobile data (camera-box issue 1453)
After=network-online.target nginx.service
Wants=network-online.target
ConditionPathExists=${tok}
StartLimitIntervalSec=0

[Service]
Type=simple
DynamicUser=yes
LoadCredential=tunnel.token:${tok}
ExecStart=/usr/bin/cloudflared tunnel --no-autoupdate --metrics ${STRIH_PUBLIC_METRICS} run --token-file %d/tunnel.token
Restart=on-failure
RestartSec=5
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
UNIT
}

# strih_public_apt_list_text -> the official cloudflared apt source, signed by the pinned key.
strih_public_apt_list_text() {
  printf 'deb [signed-by=%s] %s\n' "$(strih_public_keyring_path)" "$STRIH_PUBLIC_REPO"
}

# strih_public_dropin_text -> the intercom-hub drop-in that hands the TURN key file to the hub.
strih_public_dropin_text() {
  cat <<DROPIN
# intercom-hub TURN key (camera-box issue 1453) -- installed by setup-strih step 16i. Do NOT edit by hand.
# The hub runs as the operator and cannot read the root 0600 key file, so systemd hands it over as the
# credential ${STRIH_PUBLIC_TURN_CRED}; the hub prefers that copy (intercom/hub/src/turn.rs). Present only
# while the key file exists: a LoadCredential= of a missing file fails the hub start.
[Service]
LoadCredential=${STRIH_PUBLIC_TURN_CRED}:$(strih_public_turn_file)
DROPIN
}

# strih_public_token_ok TOKEN -> rc 0 iff TOKEN looks like a tunnel token (base64url / base64 characters,
# 20 or more). The sets are spelled out: a bracket range follows the locale (ci-testing-gotchas.md).
strih_public_token_ok() {
  local LC_ALL=C t="${1-}" up='ABCDEFGHIJKLMNOPQRSTUVWXYZ' low='abcdefghijklmnopqrstuvwxyz' num='0123456789'
  [ "${#t}" -ge 20 ] && [[ $t =~ ^[${up}${low}${num}+/=_-]+$ ]]
}

# _strih_public_owner_mode FILE -> `<owner> <mode>` (stat), empty when unreadable. Always rc 0.
_strih_public_owner_mode() { stat -c '%U %a' "${1-}" 2>/dev/null || true; }

# _strih_public_lock_down FILE -> mode 600 and, as root, owner root:root. rc 1 + a stderr line on failure.
_strih_public_lock_down() {
  local f="$1" mode
  mode="$(stat -c %a "$f" 2>/dev/null || true)"
  if [ "$mode" != 600 ]; then
    chmod 600 "$f" || { echo "strih-interkom-public: cannot chmod 600 ${f}" >&2; return 1; }
    echo "  ${f} mode ${mode:-?} -> 600"
  fi
  if [ "$(id -u)" = 0 ] && [ "$(stat -c %U:%G "$f" 2>/dev/null || true)" != root:root ]; then
    chown root:root "$f" || { echo "strih-interkom-public: cannot chown root:root ${f}" >&2; return 1; }
    echo "  ${f} owner -> root:root"
  fi
  return 0
}

# strih_interkom_public_token_store -> the tunnel token on STDIN (one line) -> the token file, 0600 root,
# written only when it differs. Prints `written` | `unchanged`; never prints the token. rc 1 on an empty
# or malformed token or a failed write (the file then untouched).
strih_interkom_public_token_store() {
  local f tok d tmp
  f="$(strih_public_token_file)"
  IFS= read -r tok || true
  tok="${tok%$'\r'}"
  if ! strih_public_token_ok "$tok"; then
    echo "strih-interkom-public: the tunnel token on stdin is empty or malformed -- nothing written" >&2
    return 1
  fi
  d="$(dirname "$f")"
  install -d -m 0755 "$d" || { echo "strih-interkom-public: cannot create ${d}" >&2; return 1; }
  if [ -f "$f" ] && [ "$(cat "$f" 2>/dev/null || true)" = "$tok" ]; then
    _strih_public_lock_down "$f" >/dev/null || return 1
    printf 'unchanged'
    return 0
  fi
  tmp="$(umask 077 && mktemp "${d}/.interkom-tunnel.token.XXXXXX")" || { echo "strih-interkom-public: cannot write into ${d}" >&2; return 1; }
  if ! { printf '%s\n' "$tok" > "$tmp" && chmod 600 "$tmp" && mv -f "$tmp" "$f"; }; then
    rm -f "$tmp"
    echo "strih-interkom-public: cannot write ${f}" >&2
    return 1
  fi
  _strih_public_lock_down "$f" >/dev/null || return 1
  printf 'written'
}

# _strih_public_install_cloudflared -> the cloudflared package from Cloudflare's apt repo, only when it
# is missing. rc 1 + a stderr line on a key that does not match the pin or a failed apt run.
_strih_public_install_cloudflared() {
  local tmp sum w pin
  if [ "$(dpkg-query -W -f='${Status}' cloudflared 2>/dev/null || true)" = "install ok installed" ]; then
    echo "  cloudflared present"
    return 0
  fi
  pin="${STRIH_PUBLIC_KEY_SHA256:-$STRIH_PUBLIC_KEY_PIN}"
  tmp="$(mktemp -d)" || { echo "strih-interkom-public: mktemp failed" >&2; return 1; }
  if ! curl -fsSL --max-time 60 -o "${tmp}/key" "$STRIH_PUBLIC_KEY_URL"; then
    rm -rf "$tmp"; echo "strih-interkom-public: cannot download ${STRIH_PUBLIC_KEY_URL}" >&2; return 1
  fi
  sum="$(sha256sum "${tmp}/key" | cut -d' ' -f1)"
  if [ "$sum" != "$pin" ]; then
    rm -rf "$tmp"
    echo "strih-interkom-public: ${STRIH_PUBLIC_KEY_URL} sha256 ${sum} is not the pinned ${pin} -- refusing the key (a rotation moves the pin on purpose)" >&2
    return 1
  fi
  install -d -m 0755 "$(dirname "$(strih_public_keyring_path)")" "$(dirname "$(strih_public_apt_list_path)")" || {
    rm -rf "$tmp"; echo "strih-interkom-public: cannot create the keyring / apt source dirs" >&2; return 1; }
  w="$(strih_session_apps_put "${tmp}/key" "$(strih_public_keyring_path)" 0644)" || { rm -rf "$tmp"; return 1; }
  echo "  $(strih_public_keyring_path) ${w} (sha256 = the pin)"
  strih_public_apt_list_text > "${tmp}/list" || { rm -rf "$tmp"; return 1; }
  w="$(strih_session_apps_put "${tmp}/list" "$(strih_public_apt_list_path)" 0644)" || { rm -rf "$tmp"; return 1; }
  rm -rf "$tmp"
  echo "  $(strih_public_apt_list_path) ${w}"
  echo "  installing cloudflared (--no-upgrade: nothing OBS uses is upgraded)"
  DEBIAN_FRONTEND=noninteractive apt-get update || { echo "strih-interkom-public: apt-get update failed" >&2; return 1; }
  DEBIAN_FRONTEND=noninteractive apt-get install -y --no-upgrade cloudflared || {
    echo "strih-interkom-public: apt-get install cloudflared failed" >&2; return 1; }
}

# _strih_public_janus MODE [URL] -> run the jcfg tool (--set URL | --clear) on janus.jcfg when it exists.
# Prints changed | unchanged | absent; rc 1 on a tool failure (a conflict included).
_strih_public_janus() {
  local jcfg out
  jcfg="$(strih_public_janus_jcfg)"
  if [ ! -f "$jcfg" ]; then printf 'absent'; return 0; fi
  out="$(python3 "$(strih_public_janus_tool)" --file "$jcfg" "$@")" || return 1
  printf '%s' "$out"
}

# _strih_public_turn_off -> remove the hub drop-in and the Janus block (a box without a TURN key, or a
# box that publishes nothing). Prints one line per change; rc 1 on a failure.
_strih_public_turn_off() {
  local f j
  f="$(strih_public_dropin_path)"
  if [ -e "$f" ] || [ -L "$f" ]; then
    rm -f "$f" || { echo "strih-interkom-public: cannot remove ${f}" >&2; return 1; }
    systemctl daemon-reload || { echo "strih-interkom-public: systemctl daemon-reload failed" >&2; return 1; }
    echo "  removed ${f} (no TURN key): the hub drops TURN at its next restart"
  fi
  j="$(_strih_public_janus --clear)" || { echo "strih-interkom-public: could not clear the Janus TURN REST block" >&2; return 1; }
  [ "$j" = changed ] && echo "  janus.jcfg: the TURN REST block removed"
  _strih_public_janus_reload "$j"
}

# _strih_public_janus_reload JANUS_STATE -> restart a running Janus when its jcfg changed.
_strih_public_janus_reload() {
  [ "${1-}" = changed ] || return 0
  if systemctl is-active --quiet janus; then
    systemctl restart janus || { echo "strih-interkom-public: systemctl restart janus failed" >&2; return 1; }
    echo "  janus restarted to load the changed nat section"
  fi
  return 0
}

# strih_interkom_public_install -> see the header. rc 1 + a stderr line on a failure; a missing token /
# TURN key is a WARN.
strih_interkom_public_install() {
  local fact tok unit turn w_tok="" w_unit w_drop tmp j f act
  fact="$(strih_lx_https_public)" || return 1
  tok="$(strih_public_token_file)"
  unit="$(strih_public_unit_path)"
  turn="$(strih_public_turn_file)"
  if [ "$fact" != yes ]; then
    echo "  STRIH_HTTPS_PUBLIC=${fact}: this box publishes nothing off the LAN (no tunnel, no TURN)"
    [ -e "$unit" ] && echo "  WARN: ${unit} is present on a box that publishes nothing -- left alone; disable it by hand if it runs" >&2
    _strih_public_turn_off
    return
  fi

  _strih_public_install_cloudflared || return 1
  if [ -n "${CLOUDFLARE_TUNNEL_TOKEN:-}" ]; then
    w_tok="$(printf '%s\n' "$CLOUDFLARE_TUNNEL_TOKEN" | strih_interkom_public_token_store)" || return 1
    echo "  ${tok} ${w_tok} (from CLOUDFLARE_TUNNEL_TOKEN; never printed)"
  elif [ -f "$tok" ]; then
    _strih_public_lock_down "$tok" || return 1
  fi

  tmp="$(mktemp -d)" || { echo "strih-interkom-public: mktemp failed" >&2; return 1; }
  strih_public_unit_text > "${tmp}/unit" || { rm -rf "$tmp"; return 1; }
  install -d -m 0755 "$(dirname "$unit")" || { rm -rf "$tmp"; echo "strih-interkom-public: cannot create $(dirname "$unit")" >&2; return 1; }
  w_unit="$(strih_session_apps_put "${tmp}/unit" "$unit" 0644)" || { rm -rf "$tmp"; return 1; }
  echo "  ${unit} ${w_unit}"
  if [ "$w_unit" = written ]; then
    systemctl daemon-reload || { rm -rf "$tmp"; echo "strih-interkom-public: systemctl daemon-reload failed" >&2; return 1; }
  fi
  if [ "$(systemctl is-enabled "$STRIH_PUBLIC_UNIT" 2>/dev/null || true)" != enabled ]; then
    systemctl enable "$STRIH_PUBLIC_UNIT" || { rm -rf "$tmp"; echo "strih-interkom-public: systemctl enable ${STRIH_PUBLIC_UNIT} failed" >&2; return 1; }
  fi
  if [ -f "$tok" ]; then
    act="$(systemctl is-active "$STRIH_PUBLIC_UNIT" 2>/dev/null || true)"
    # --no-block: the queued job never holds setup up (an uplink-down venue); item 45 grades /ready.
    if [ "$act" != active ]; then
      systemctl --no-block start "$STRIH_PUBLIC_UNIT" || { rm -rf "$tmp"; echo "strih-interkom-public: systemctl start ${STRIH_PUBLIC_UNIT} failed" >&2; return 1; }
      echo "  started ${STRIH_PUBLIC_UNIT}"
    elif [ "$w_unit" = written ] || [ "$w_tok" = written ]; then
      systemctl --no-block restart "$STRIH_PUBLIC_UNIT" || { rm -rf "$tmp"; echo "strih-interkom-public: systemctl restart ${STRIH_PUBLIC_UNIT} failed" >&2; return 1; }
      echo "  restarted ${STRIH_PUBLIC_UNIT} (its unit or token changed)"
    else
      echo "  ${STRIH_PUBLIC_UNIT} running"
    fi
  else
    echo "  WARN: ${tok} missing -- the intercom is not published to mobile data yet; supervisor step (the issue 1453 runbook): pipe the tunnel token from the secret store over ssh stdin into strih_interkom_public_token_store (or export CLOUDFLARE_TUNNEL_TOKEN for this run), then re-run setup-strih.sh step 16i" >&2
  fi

  if [ ! -f "$turn" ]; then
    rm -rf "$tmp"
    echo "  WARN: ${turn} missing -- phones on mobile data load the page but get no media relay; supervisor step (the issue 1453 runbook): place the Cloudflare TURN key (key_id + key_api_token, 0600 root), then re-run setup-strih.sh step 16i" >&2
    _strih_public_turn_off
    return
  fi
  _strih_public_lock_down "$turn" || { rm -rf "$tmp"; return 1; }
  f="$(strih_public_dropin_path)"
  install -d -m 0755 "$(dirname "$f")" || { rm -rf "$tmp"; echo "strih-interkom-public: cannot create $(dirname "$f")" >&2; return 1; }
  strih_public_dropin_text > "${tmp}/dropin" || { rm -rf "$tmp"; return 1; }
  w_drop="$(strih_session_apps_put "${tmp}/dropin" "$f" 0644)" || { rm -rf "$tmp"; return 1; }
  rm -rf "$tmp"
  echo "  ${f} ${w_drop}"
  if [ "$w_drop" = written ]; then
    systemctl daemon-reload || { echo "strih-interkom-public: systemctl daemon-reload failed" >&2; return 1; }
    if systemctl is-active --quiet intercom-hub; then
      echo "  NOTE: the running intercom hub loads the TURN key at its next restart (provisioning never restarts a running hub): supervisor, systemctl restart intercom-hub"
    fi
  fi
  j="$(_strih_public_janus --set "$(strih_public_turn_rest_url)")" || {
    echo "strih-interkom-public: could not write Janus's TURN REST setting (above)" >&2; return 1; }
  echo "  janus.jcfg TURN REST block: ${j}"
  _strih_public_janus_reload "$j"
}

# --- verify-strih item 45 -------------------------------------------------------------------------------

# strih_interkom_public_grade_rows -> two verify-strih rows `OK|text`, `NOTE|text` or `FAIL|text`.
# Always rc 0.
strih_interkom_public_grade_rows() {
  local fact owner tok unit turn tmp st problems ready conns code body f j
  fact="$(strih_lx_https_public 2>/dev/null)" || { printf 'FAIL|(https-tunnel) the box facts are unreadable\nFAIL|(https-turn) the box facts are unreadable\n'; return 0; }
  if [ "$fact" != yes ]; then
    st="$(systemctl is-enabled "$STRIH_PUBLIC_UNIT" 2>/dev/null | head -n 1 || true)/$(systemctl is-active "$STRIH_PUBLIC_UNIT" 2>/dev/null | head -n 1 || true)"
    case "$st" in
      enabled/* | */active | */activating)
        printf 'FAIL|(https-tunnel) STRIH_HTTPS_PUBLIC=%s but %s is %s -- a leftover tunnel still publishes this box; systemctl disable --now %s\n' \
          "$fact" "$STRIH_PUBLIC_UNIT" "$st" "$STRIH_PUBLIC_UNIT" ;;
      *) printf 'NOTE|(https-tunnel) STRIH_HTTPS_PUBLIC=%s: this box publishes nothing off the LAN -- not graded\n' "$fact" ;;
    esac
    printf 'NOTE|(https-turn) STRIH_HTTPS_PUBLIC=%s: no TURN on this box -- not graded\n' "$fact"
    return 0
  fi
  owner="${STRIH_PUBLIC_OWNER:-root}"
  tok="$(strih_public_token_file)"
  unit="$(strih_public_unit_path)"
  turn="$(strih_public_turn_file)"
  tmp="$(mktemp -d 2>/dev/null)" || tmp=""

  problems=""
  if [ ! -f "$tok" ]; then
    problems="${problems} no-${tok}(the supervisor places the tunnel token, issue 1453 runbook)"
  else
    st="$(_strih_public_owner_mode "$tok")"
    [ "$st" = "${owner} 600" ] || problems="${problems} ${tok}=${st:-unreadable}(want ${owner} 600)"
  fi
  if [ -n "$tmp" ] && strih_public_unit_text > "${tmp}/unit" 2>/dev/null; then
    st="$(strih_session_app_file_state "$unit" "${tmp}/unit")"
  else
    st=render-failed
  fi
  [ "$st" = ok ] || problems="${problems} unit=${st}"
  st="$(systemctl is-enabled "$STRIH_PUBLIC_UNIT" 2>/dev/null | head -n 1 || true)"
  [ "$st" = enabled ] || problems="${problems} ${STRIH_PUBLIC_UNIT}=${st:-unreadable}"
  st="$(systemctl is-active "$STRIH_PUBLIC_UNIT" 2>/dev/null | head -n 1 || true)"
  [ "$st" = active ] || problems="${problems} ${STRIH_PUBLIC_UNIT}=${st:-unreadable}"
  body="$(curl -s --max-time 5 -w '\n%{http_code}' "http://${STRIH_PUBLIC_METRICS}/ready" 2>/dev/null || true)"
  ready="${body##*$'\n'}"
  conns="$(sed -n 's/.*"readyConnections"[[:space:]]*:[[:space:]]*\([0123456789][0123456789]*\).*/\1/p' <<<"$body" | head -n 1 || true)"
  if [ "$ready" != 200 ] || [ "${conns:-0}" -lt 1 ]; then
    problems="${problems} metrics-/ready=${ready:-000}(readyConnections ${conns:-?}, want 200 + at least 1)"
  fi
  if [ -z "$problems" ]; then
    printf 'OK|(https-tunnel) %s active with %s connection(s) to Cloudflare (metrics /ready), the token file present (%s 600) -- phones on mobile data reach this box\n' \
      "$STRIH_PUBLIC_UNIT" "$conns" "$owner"
  else
    printf 'FAIL|(https-tunnel)%s -- re-run setup-strih.sh step 16i (issue 1453); journalctl -u %s\n' "$problems" "$STRIH_PUBLIC_UNIT"
  fi

  problems=""
  if [ ! -f "$turn" ]; then
    problems="${problems} no-${turn}(the supervisor places the Cloudflare TURN key, issue 1453 runbook)"
  else
    st="$(_strih_public_owner_mode "$turn")"
    [ "$st" = "${owner} 600" ] || problems="${problems} ${turn}=${st:-unreadable}(want ${owner} 600)"
  fi
  f="$(strih_public_dropin_path)"
  if [ -n "$tmp" ] && strih_public_dropin_text > "${tmp}/dropin" 2>/dev/null; then
    st="$(strih_session_app_file_state "$f" "${tmp}/dropin")"
  else
    st=render-failed
  fi
  [ "$st" = ok ] || problems="${problems} hub-dropin=${st}"
  j="$(python3 "$(strih_public_janus_tool)" --file "$(strih_public_janus_jcfg)" --check "$(strih_public_turn_rest_url)" 2>/dev/null || true)"
  [ "$j" = ok ] || problems="${problems} janus.jcfg-turn-rest=${j:-unreadable}"
  body="$(curl -s --max-time 10 -w '\n%{http_code}' "$(strih_public_turn_rest_url)" 2>/dev/null || true)"
  code="${body##*$'\n'}"
  if [ "$code" != 200 ] || ! grep -q '"turns\{0,1\}:' <<<"$body"; then
    problems="${problems} hub-/api/turn-rest=${code:-000}($(head -n 1 <<<"${body%$'\n'*}" | cut -c1-120))"
  fi
  [ -z "$tmp" ] || rm -rf "$tmp"
  if [ -z "$problems" ]; then
    printf 'OK|(https-turn) the TURN key present (%s 600) and handed to the hub, Janus asks the hub for TURN, the hub mints Cloudflare TURN credentials (/api/turn-rest 200)\n' "$owner"
  else
    printf 'FAIL|(https-turn)%s -- re-run setup-strih.sh step 16i, then systemctl restart intercom-hub (issue 1453)\n' "$problems"
  fi
  return 0
}

# strih_interkom_public_grade_report -> print the rows through the CALLER's ok / note / bad functions
# (verify-strih). rc 1 unless the grader printed both rows.
strih_interkom_public_grade_report() {
  local st d rows=0
  while IFS='|' read -r st d; do
    [ -n "$st" ] || continue
    rows=$((rows + 1))
    case "$st" in
      OK) ok "$d" ;;
      NOTE) note "$d" ;;
      *) bad "$d" ;;
    esac
  done < <(strih_interkom_public_grade_rows)
  [ "$rows" = 2 ]
}
