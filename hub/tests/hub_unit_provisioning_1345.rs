//! Static-anchor tests for the hub's systemd unit and the CI release job (issue 1345 M1). Moved
//! from camera-box's `tests/intercom_hub_provisioning.rs` by intercom#1. The anchors on the
//! provisioning that installs the unit (`setup-strih.sh`, `verify-strih.sh`, the
//! `scripts/lib/strih-*.sh` drop-in renderers) stay in camera-box, next to those files. Std-only.

use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn read(rel: &str) -> String {
    let p = root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
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
/// SCHED_FIFO 10. The unit that strih-snv runs (the base unit plus the step-12 local-audio drop-in)
/// must let it: `LimitRTPRIO=10` in the base unit's `[Service]`. That the step-12 drop-in leaves it
/// alone is pinned in camera-box, which renders that drop-in.
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
}

/// Issue 1401, design 6265012279: the hub locks its memory and is never the OOM victim. The base
/// unit grants both (`LimitMEMLOCK=infinity`, `OOMScoreAdjust=-900`). camera-box pins that no
/// drop-in its provisioning writes (the step-12 local audio, the issue-1412 credential reset, the NDI
/// discovery, the public tunnel) lowers or resets either.
#[test]
fn the_real_hub_unit_grants_the_memory_lock_and_no_dropin_takes_it_back() {
    let unit = read("systemd/intercom-hub.service");
    let service = unit
        .split("[Service]")
        .nth(1)
        .and_then(|s| s.split("[Install]").next())
        .expect("the unit has a [Service] section");
    for line in ["LimitMEMLOCK=infinity", "OOMScoreAdjust=-900"] {
        assert!(
            service.lines().any(|l| l.trim() == line),
            "the base unit's [Service] must carry {line}"
        );
    }
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

#[test]
fn ci_tests_both_members_and_the_release_ships_the_pinned_tarball() {
    let ci = read(".github/workflows/ci.yml");
    for needle in [
        "cargo test -p intercom-vban -p intercom-hub --locked",
        "cargo clippy -p intercom-vban -p intercom-hub --all-targets --locked -- -D warnings",
        "cargo test -p intercom-hub --no-default-features --locked",
        "--fail-under-lines",
        "intercom-hub-linux-amd64",
    ] {
        assert!(ci.contains(needle), "ci.yml must carry `{needle}`");
    }
    let rel = read(".github/workflows/release.yml");
    // camera-box fetches the hub by exact version with a sha256 (issue 1467: never a newest build).
    for needle in [
        "intercom-hub-linux-amd64.tar.gz",
        "intercom-hub-linux-amd64.tar.gz.sha256",
        "systemd/intercom-hub.service",
        "intercom.strih-snv.toml",
        "intercom.strih-pp.toml",
        "intercom.stream-snv.toml",
        "intercom.stream-pp.toml",
        "--version",
    ] {
        assert!(rel.contains(needle), "release.yml must carry `{needle}`");
    }
}
