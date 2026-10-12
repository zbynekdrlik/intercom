//! Static-anchor wiring tests for the strih-lx intercom hub provisioning (issue 1345 M1).
//!
//! The hub's systemd unit + routing TOML are installed by `setup-strih.sh`, which also STARTS a
//! stopped hub on the strih role (issue 1418: the interkom runs on it since the M4 cut-over) and never
//! restarts a running one. `verify-strih.sh` FAILs an inactive hub on the strih role and only reports
//! it on the stream role, and the `intercom-hub` CI job builds/tests/uploads it. These
//! tests pin that wiring textually (same `read + .contains` convention as
//! `tests/strih_provision_pure_functions.rs`), so a silent removal, a restart or an ungated start
//! surfaces on CI. Std-only — runs in the appliance `test` job (Tier-0 #557: CI is the first run).

use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let p = root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

#[test]
fn setup_strih_installs_the_unit_and_toml_enable_only() {
    let s = read("scripts/setup-strih.sh");
    assert!(
        s.contains("/etc/systemd/system/intercom-hub.service"),
        "setup-strih must install the intercom-hub systemd unit"
    );
    assert!(
        s.contains("/etc/intercom-hub/intercom.toml"),
        "setup-strih must install the generated routing TOML"
    );
    assert!(
        s.contains("systemctl enable intercom-hub"),
        "setup-strih must ENABLE the intercom-hub unit"
    );
    // issue 1418: a STOPPED hub is started on the strih role only (behind the 13-hub-live gate and an
    // is-active check); a running hub is never restarted by provisioning.
    let gate = s
        .find("if WHY=\"$(obs_box_step_applies 13-hub-live)\"")
        .expect("the hub start is gated on the strih role");
    let active = s
        .find("systemctl is-active --quiet intercom-hub")
        .expect("the hub start checks is-active first");
    let start = s
        .find("systemctl start intercom-hub")
        .expect("setup-strih starts a stopped hub on the strih role");
    assert!(
        gate < active && active < start,
        "gate -> is-active -> start, in that order"
    );
    // The unit's LoadCredential needs the room secret step 14 writes: start only after it.
    let step14 = s.find("step 14 ").expect("step 14");
    assert!(
        step14 < start,
        "the hub starts after step 14 wrote its room secret"
    );
    assert!(
        !s.contains("systemctl restart intercom-hub"),
        "setup-strih must NOT restart a running intercom-hub"
    );
    assert!(
        s.contains("TOTAL_STEPS=17"),
        "setup-strih TOTAL_STEPS must be bumped for the intercom + janus + issue-1317 perf/companion steps"
    );
}

#[test]
fn setup_strih_installs_janus_enable_only() {
    let s = read("scripts/setup-strih.sh");
    // apt install janus + write both jcfg files via the pure renderers.
    assert!(
        s.contains("apt-get install -y janus"),
        "setup-strih must apt-get install janus (M3a)"
    );
    assert!(
        s.contains("strih_janus_audiobridge_jcfg_text")
            && s.contains("/etc/janus/janus.plugin.audiobridge.jcfg"),
        "setup-strih must render + write the audiobridge room jcfg"
    );
    assert!(
        s.contains("strih_janus_ws_jcfg_text")
            && s.contains("/etc/janus/janus.transport.websockets.jcfg"),
        "setup-strih must render + write the WebSocket transport jcfg"
    );
    // The room secret is generated to a 0600 file and NEVER started/echoed.
    assert!(
        s.contains("/etc/intercom-hub/janus-room.secret") && s.contains("openssl rand -hex 16"),
        "setup-strih must generate the 0600 room secret with openssl"
    );
    assert!(
        s.contains("systemctl enable janus"),
        "setup-strih must ENABLE janus"
    );
    // ENABLE-ONLY: never an UNCONDITIONAL start (the M4 cut-over starts it with the hub).
    assert!(
        !s.contains("systemctl start janus"),
        "setup-strih must NOT start janus (enable-only until the M4 cut-over)"
    );
    // A restart is allowed ONLY guarded by `is-active` — Ubuntu's janus package auto-STARTS the
    // service on install (before the jcfg exists), so if it is already running we restart it to load
    // the freshly written room/ws/http jcfg; a not-running janus stays enable-only (issue 1345 M3
    // follow-up c).
    if s.contains("systemctl restart janus") {
        let restart = s.find("systemctl restart janus").unwrap();
        let guard = s.find("systemctl is-active --quiet janus");
        assert!(
            guard.is_some_and(|g| g < restart && restart - g < 300),
            "any `systemctl restart janus` must be guarded by a nearby `systemctl is-active --quiet janus`"
        );
    }
}

#[test]
fn systemd_unit_loads_the_room_secret_credential_and_keeps_dynamicuser() {
    let unit = read("systemd/intercom-hub.service");
    // The production unit runs DynamicUser=yes (unchanged) and reads the root-owned 0600 room secret
    // through systemd's credential store — LoadCredential drops it 0400 owned by the dynamic user, so
    // the hub can read it without dropping the sandbox (issue 1345 M3 follow-up b).
    assert!(
        unit.contains("LoadCredential=janus-room.secret:/etc/intercom-hub/janus-room.secret"),
        "the unit must LoadCredential the room secret from its 0600 file"
    );
    assert!(
        unit.contains("DynamicUser=yes"),
        "the unit keeps DynamicUser=yes (the credential store makes the 0600 secret readable)"
    );
}

/// Issue 1401 (design 5980775411): the hub's block loop runs on one thread, "hub-mix", at
/// SCHED_FIFO 10. The unit that strih-lx runs (the base unit plus the step-12 local-audio drop-in)
/// must let it: `LimitRTPRIO=10` in the base unit's `[Service]`, and the drop-in neither lowers nor
/// forbids it.
#[test]
fn the_real_hub_unit_lets_the_hub_mix_thread_run_sched_fifo_10() {
    let unit = read("systemd/intercom-hub.service");
    let service = unit
        .split("[Service]")
        .nth(1)
        .and_then(|s| s.split("[Install]").next())
        .expect("the unit has a [Service] section");
    assert!(
        service.lines().any(|l| l.trim() == "LimitRTPRIO=10"),
        "the base unit's [Service] must grant LimitRTPRIO=10"
    );
    assert!(!unit.contains("RestrictRealtime=yes"));
    let lib = read("scripts/lib/strih-provision.sh");
    let start = lib
        .find("strih_intercom_audio_dropin() {")
        .expect("the step-12 drop-in renderer");
    let body = &lib[start..];
    let dropin = &body[..body.find("\nDROPIN\n").expect("its heredoc end")];
    assert!(
        !dropin.contains("LimitRTPRIO") && !dropin.contains("RestrictRealtime"),
        "the local-audio drop-in must leave the realtime grant alone"
    );
}

#[test]
fn verify_strih_reports_janus_as_a_note_only() {
    let v = read("scripts/verify-strih.sh");
    // The janus items are NOTEs (report-only), never hard gate items while running parallel.
    assert!(
        v.contains("janus.service (enabled="),
        "verify-strih must report the janus unit state"
    );
    assert!(
        v.contains("strih_janus_room_jcfg_ok"),
        "verify-strih must report the audiobridge room jcfg via the pure predicate"
    );
    let idx = v
        .find("janus.service (enabled=")
        .expect("verify-strih must carry the janus NOTE line");
    let window = &v[idx.saturating_sub(400)..idx];
    assert!(
        window.contains("note "),
        "the janus item must be a NOTE (report-only), not ok/bad"
    );
    // No hard FAIL on janus while parallel with the Windows strih.
    assert!(
        !v.contains("bad \"janus"),
        "the janus items must never hard-FAIL (report-only until the M4 cut-over)"
    );
}

#[test]
fn verify_strih_grades_the_unit_by_role() {
    let v = read("scripts/verify-strih.sh");
    let a = v
        .find("# 16) intercom hub unit")
        .expect("verify-strih item 16");
    let b = v[a..].find("# 17)").map(|i| a + i).expect("item 17");
    let item = &v[a..b];
    // issue 1418: the strih role FAILs an inactive / disabled / missing hub (the interkom runs on it) ...
    assert!(
        item.contains("obs_box_step_applies 13-hub-live"),
        "item 16 grades the hub by the 13-hub-live role gate: {item}"
    );
    assert!(
        item.contains("bad \"intercom-hub.service enabled=")
            && item.contains("bad \"intercom-hub.service not installed"),
        "an inactive or missing hub is a FAIL on the strih role: {item}"
    );
    // ... and the stream role keeps the report-only NOTE.
    let idx = item
        .find("intercom-hub.service installed")
        .expect("the stream-role NOTE line");
    assert!(
        item[idx.saturating_sub(200)..idx].contains("note "),
        "the stream role's hub line is a NOTE: {item}"
    );
    assert!(!item.contains("M4"), "no stale M4 wording: {item}");
}

#[test]
fn ci_has_the_intercom_hub_job_uploading_the_artifact() {
    let ci = read(".github/workflows/ci.yml");
    assert!(
        ci.contains("intercom-hub:"),
        "ci.yml must define the intercom-hub job"
    );
    assert!(
        ci.contains("cargo test -p intercom-vban -p intercom-hub"),
        "the intercom-hub job must test both members"
    );
    assert!(
        ci.contains("cargo clippy -p intercom-vban -p intercom-hub --all-targets -- -D warnings"),
        "the intercom-hub job must clippy -D warnings both members"
    );
    assert!(
        ci.contains("intercom-hub-linux-amd64"),
        "the intercom-hub job must upload the deployable binary artifact"
    );
    // The job must be in the notify-on-failure needs list so a red hub build pings.
    assert!(
        ci.contains("intercom-hub, build"),
        "intercom-hub must be in the notify-on-failure needs list"
    );
}

#[test]
fn systemd_unit_runs_the_binary_against_the_installed_toml() {
    let unit = read("systemd/intercom-hub.service");
    assert!(
        unit.contains(
            "ExecStart=/usr/local/bin/intercom-hub --config /etc/intercom-hub/intercom.toml"
        ),
        "the unit must run the hub binary against the installed matrix"
    );
    assert!(
        unit.contains("Restart=on-failure"),
        "the unit must restart on failure"
    );
    assert!(unit.contains("Type=simple"), "the unit must be Type=simple");
}
