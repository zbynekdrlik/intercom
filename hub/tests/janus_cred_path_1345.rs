//! `resolve_secret_path` — WHERE the hub reads the Janus room secret from (issue 1345 M3 follow-up).
//!
//! The production `intercom-hub.service` runs `DynamicUser=yes` and cannot read the root-owned 0600
//! `[janus].room_secret_file`. systemd's credential mechanism (`LoadCredential=janus-room.secret:…`)
//! drops the secret into `$CREDENTIALS_DIRECTORY` readable by the dynamic user. This pure function
//! decides the path: prefer the credential store, else the configured file. It NEVER reads or
//! returns the secret VALUE — only the path to read. Std-only; runs in the intercom-hub CI job.

use intercom_hub::matrix::{resolve_secret_path, JANUS_ROOM_SECRET_CRED};
use std::path::{Path, PathBuf};

#[test]
fn credentials_dir_wins_and_appends_the_credential_name() {
    let got = resolve_secret_path(
        Some("/run/credentials/intercom-hub.service"),
        Some(Path::new("/etc/intercom-hub/janus-room.secret")),
    );
    assert_eq!(
        got,
        Some(PathBuf::from(
            "/run/credentials/intercom-hub.service/janus-room.secret"
        )),
        "with $CREDENTIALS_DIRECTORY set, read the credential (dir + credential name)"
    );
}

#[test]
fn credential_name_is_the_loadcredential_name() {
    assert_eq!(
        JANUS_ROOM_SECRET_CRED, "janus-room.secret",
        "the credential name must match the unit's LoadCredential name"
    );
}

#[test]
fn falls_back_to_the_configured_file_when_no_credentials_dir() {
    let got = resolve_secret_path(None, Some(Path::new("/etc/intercom-hub/janus-room.secret")));
    assert_eq!(
        got,
        Some(PathBuf::from("/etc/intercom-hub/janus-room.secret")),
        "no credential store => the configured room_secret_file"
    );
}

#[test]
fn empty_credentials_dir_is_treated_as_unset() {
    // A stray empty/whitespace $CREDENTIALS_DIRECTORY must NOT resolve to a bare `/janus-room.secret`.
    for dir in ["", "   "] {
        let got = resolve_secret_path(
            Some(dir),
            Some(Path::new("/etc/intercom-hub/janus-room.secret")),
        );
        assert_eq!(
            got,
            Some(PathBuf::from("/etc/intercom-hub/janus-room.secret")),
            "an empty credentials dir falls back to the configured file"
        );
    }
}

#[test]
fn none_when_neither_source_is_available() {
    assert_eq!(
        resolve_secret_path(None, None),
        None,
        "no credential store and no configured file => no secret path (join without a secret)"
    );
    // An empty credentials dir with no configured file is still None (not a bare credential name).
    assert_eq!(resolve_secret_path(Some(""), None), None);
}
