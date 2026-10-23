//! issue 1438 (review round 1): `intercom-hub --check-config` loads the routing TOML and exits
//! without starting anything, so setup-strih installs a new routing file only when the installed hub
//! runs it. Runs the real binary (CI: `CARGO_BIN_EXE_intercom-hub`).

use std::path::Path;
use std::process::Command;

fn check(config: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_intercom-hub"))
        .arg("--check-config")
        .arg("--config")
        .arg(config)
        .output()
        .expect("run intercom-hub")
}

#[test]
fn both_deployed_routing_files_load() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for toml in ["intercom.strih-snv.toml", "intercom.strih-pp.toml"] {
        let out = check(&root.join(toml));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{toml}: {stdout} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("loads"), "{toml}: {stdout}");
    }
}

#[test]
fn a_routing_file_the_hub_refuses_exits_non_zero_naming_why() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.toml");
    std::fs::write(
        &bad,
        "[hub]\nbind = \"0.0.0.0:8790\"\nvban_bind = \"0.0.0.0:6980\"\nsample_rate = 48000\nblock_frames = 256\n\n[[participant]]\nname = \"cutters\"\nrole = \"cutters\"\nadapter = \"teleport\"\nin_channels = 2\nout_channels = 2\n",
    )
    .unwrap();
    let out = check(&bad);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown adapter 'teleport'"), "{stderr}");

    let missing = check(&dir.path().join("absent.toml"));
    assert!(!missing.status.success());
}
