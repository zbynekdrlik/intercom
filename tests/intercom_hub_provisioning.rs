//! Static-anchor wiring tests for the strih-lx intercom hub provisioning (issue 1345 M1).
//!
//! The hub's systemd unit + routing TOML are installed by `setup-strih.sh` ENABLE-ONLY (never
//! started while the Windows VB-Matrix is still the live intercom — M4 is the cut-over), reported
//! REPORT-ONLY by `verify-strih.sh`, and built/tested/uploaded by the `intercom-hub` CI job. These
//! tests pin that wiring textually (same `read + .contains` convention as
//! `tests/strih_provision_pure_functions.rs`), so a silent removal or a stray `systemctl start`
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
    // ENABLE-ONLY: the M4 cut-over starts it, never this provisioning step.
    assert!(
        !s.contains("systemctl start intercom-hub"),
        "setup-strih must NOT start intercom-hub (enable-only until the M4 cut-over)"
    );
    assert!(
        !s.contains("systemctl restart intercom-hub"),
        "setup-strih must NOT restart intercom-hub (enable-only until the M4 cut-over)"
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
    // ENABLE-ONLY: the M4 cut-over starts it, never this provisioning step.
    assert!(
        !s.contains("systemctl start janus"),
        "setup-strih must NOT start janus (enable-only until the M4 cut-over)"
    );
    assert!(
        !s.contains("systemctl restart janus"),
        "setup-strih must NOT restart janus (enable-only until the M4 cut-over)"
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
fn verify_strih_reports_the_unit_as_a_note_only() {
    let v = read("scripts/verify-strih.sh");
    assert!(
        v.contains("intercom-hub.service"),
        "verify-strih must report the intercom-hub unit"
    );
    // It is a NOTE (report-only), never a hard gate item while running parallel with Windows strih.
    let idx = v
        .find("intercom-hub.service installed")
        .expect("verify-strih must carry the intercom-hub NOTE line");
    let window = &v[idx.saturating_sub(200)..idx];
    assert!(
        window.contains("note "),
        "the intercom-hub item must be a NOTE (report-only), not ok/bad"
    );
    assert!(
        v.contains("report-only"),
        "the intercom-hub verify item must state it is report-only"
    );
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
