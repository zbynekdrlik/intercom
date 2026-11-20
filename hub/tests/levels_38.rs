//! intercom#38: one slider per source plus a master. The owner (20.11.2026): "strihac potrebuje
//! lepsi mix kde vie nastavit viacej hlasitosti nie len tie zakladne co si mu tam dal".
//!
//! Pinned here: the master `[[level]]` (`master = true`, no sources) trims the whole `dst` output on
//! top of the per-source trims, 0 dB bit-identical, mute = silence on that output only; its
//! refusals; the `from` migration of a #34 state file (a new per-source slider starts at its old
//! group's value, its own stored value wins, unknown names are ignored, nothing saved is lost); the
//! additive `master` field on `/api/levels` and `/api/state`; both deployed strih TOMLs' sliders.

use intercom_hub::engine::{Engine, InputBlock};
use intercom_hub::levels_http::levels_body;
use intercom_hub::matrix::{gain_linear, Matrix};
use intercom_hub::state::HubState;

const HEAD: &str = r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 4

[[participant]]
name = "cam1"
role = "cambox"
adapter = "vban"
host = "cam1.lan"
in_stream = "cam1"
out_stream = "cam1"
in_channels = 1
out_channels = 1

[[participant]]
name = "cam2"
role = "cambox"
adapter = "vban"
host = "cam2.lan"
in_stream = "cam2"
out_stream = "cam2"
in_channels = 1
out_channels = 1

[[participant]]
name = "prog"
role = "program_ref"
adapter = "vban"
in_stream = "prog"
in_channels = 3
out_channels = 0

[[participant]]
name = "cutters"
role = "cutters"
adapter = "none"
in_channels = 1
out_channels = 2

[[point]]
src = "prog"
in_ch = 1
dst = "cutters"
out_ch = 1
gain_db = -8.0

[[point]]
src = "prog"
in_ch = 2
dst = "cutters"
out_ch = 2
gain_db = -8.0

[[point]]
src = "prog"
in_ch = 3
dst = "cutters"
out_ch = 1
gain_db = -10.0

[[point]]
src = "prog"
in_ch = 3
dst = "cutters"
out_ch = 2
gain_db = -10.0

[[point]]
src = "cam1"
in_ch = 1
dst = "cutters"
out_ch = 1

[[point]]
src = "cam1"
in_ch = 1
dst = "cutters"
out_ch = 2

[[point]]
src = "cam2"
in_ch = 1
dst = "cutters"
out_ch = 1

[[point]]
src = "cam2"
in_ch = 1
dst = "cutters"
out_ch = 2

[[point]]
src = "cutters"
in_ch = 1
dst = "cam1"
out_ch = 1

[[point]]
src = "cutters"
in_ch = 1
dst = "cam2"
out_ch = 1

[[point]]
src = "cam1"
in_ch = 1
dst = "cam2"
out_ch = 1
"#;

/// The per-source shape of the deployed strih-snv sliders in small: foh (from program), click,
/// one slider per camera (from kamery), the master last.
const SLIDERS: &str = r#"
[[level]]
name = "foh"
label = "FOH"
dst = "cutters"
sources = ["prog:1", "prog:2"]
from = "program"

[[level]]
name = "click"
label = "Click"
dst = "cutters"
sources = ["prog:3"]

[[level]]
name = "cam1"
label = "Kamera 1"
dst = "cutters"
sources = ["cam1"]
from = "kamery"

[[level]]
name = "cam2"
label = "Kamera 2"
dst = "cutters"
sources = ["cam2"]
from = "kamery"
min_db = -20.0

[[level]]
name = "master"
label = "Master"
dst = "cutters"
master = true
"#;

const FOH: usize = 0;
const CLICK: usize = 1;
const CAM1: usize = 2;
const CAM2: usize = 3;
const MASTER: usize = 4;

fn matrix() -> Matrix {
    Matrix::from_toml(&format!("{HEAD}{SLIDERS}")).unwrap()
}

fn refused(extra: &str) -> String {
    format!(
        "{:#}",
        Matrix::from_toml(&format!("{HEAD}{extra}")).unwrap_err()
    )
}

const CAM1_IN: [i16; 4] = [1000, -2000, 3000, -4000];
const CAM2_IN: [i16; 4] = [-500, 700, 2500, 9000];
const PROG_IN: [[i16; 4]; 3] = [
    [10000, 10000, -10000, 5000],
    [-7000, 7000, 0, 1],
    [20000, -20000, 123, 4567],
];

fn input(m: &Matrix) -> InputBlock {
    let mut b = InputBlock::silent(m.participants.len());
    b.set(m.id_of("cam1").unwrap(), vec![CAM1_IN.to_vec()]);
    b.set(m.id_of("cam2").unwrap(), vec![CAM2_IN.to_vec()]);
    b.set(
        m.id_of("prog").unwrap(),
        PROG_IN.iter().map(|c| c.to_vec()).collect(),
    );
    b
}

fn cans(m: &Matrix, e: &Engine) -> Vec<Vec<i16>> {
    let out = e.mix_block(&input(m), 4);
    let c = m.id_of("cutters").unwrap();
    (1..=2)
        .map(|ch| out.channel(c, ch).unwrap().to_vec())
        .collect()
}

/// What the cans should carry, summed in the engine's point order in f32, then the master.
fn expected(foh: f32, click: f32, cam1: f32, cam2: f32, master: f32) -> Vec<Vec<i16>> {
    let g8 = gain_linear(-8.0) * foh;
    let g10 = gain_linear(-10.0) * click;
    (0..2)
        .map(|side| {
            (0..4)
                .map(|i| {
                    let mut acc = 0f32;
                    acc += PROG_IN[side][i] as f32 * g8;
                    acc += PROG_IN[2][i] as f32 * g10;
                    acc += CAM1_IN[i] as f32 * cam1;
                    acc += CAM2_IN[i] as f32 * cam2;
                    (acc * master)
                        .round()
                        .clamp(i16::MIN as f32, i16::MAX as f32) as i16
                })
                .collect()
        })
        .collect()
}

fn talkback(m: &Matrix, e: &Engine) -> (Vec<i16>, Vec<i16>) {
    let out = e.mix_block(&input(m), 4);
    (
        out.channel(m.id_of("cam1").unwrap(), 1).unwrap().to_vec(),
        out.channel(m.id_of("cam2").unwrap(), 1).unwrap().to_vec(),
    )
}

// ---- resolution -----------------------------------------------------------------------------

#[test]
fn the_master_resolves_onto_the_whole_output_and_claims_no_point() {
    let m = matrix();
    let specs = m.levels.specs();
    assert_eq!(specs.len(), 5);
    let master = &specs[MASTER];
    assert!(master.master);
    assert!(master.sources.is_empty());
    assert_eq!(master.dst_name, "cutters");
    assert_eq!(master.points, 8, "every point into the cutters");
    assert!(specs[..MASTER].iter().all(|s| !s.master));
    assert_eq!(
        specs.iter().map(|s| s.points).collect::<Vec<_>>(),
        [2, 2, 2, 2, 8]
    );
    // Every cutters point is trimmed by exactly one SOURCE slider, never by the master's slot.
    let c = m.id_of("cutters").unwrap();
    for p in m.points.iter().filter(|p| p.dst == c) {
        let l = p.level.expect("a cutters point without a source slider");
        assert_ne!(l, MASTER);
    }
    // The talkback legs are no slider's.
    assert!(m
        .points
        .iter()
        .filter(|p| p.dst != c)
        .all(|p| p.level.is_none()));
}

#[test]
fn the_from_names_are_resolved() {
    let m = matrix();
    let from: Vec<Option<&str>> = m.levels.specs().iter().map(|s| s.from.as_deref()).collect();
    assert_eq!(
        from,
        [Some("program"), None, Some("kamery"), Some("kamery"), None]
    );
}

#[test]
fn a_master_with_sources_is_refused() {
    let e = refused(
        "\n[[level]]\nname = \"master\"\nlabel = \"Master\"\ndst = \"cutters\"\nmaster = true\nsources = [\"cam1\"]\n",
    );
    assert!(e.contains("a master takes no sources"), "{e}");
}

#[test]
fn a_second_master_for_one_output_is_refused() {
    let e = refused(
        "\n[[level]]\nname = \"m1\"\nlabel = \"M\"\ndst = \"cutters\"\nmaster = true\n\
         \n[[level]]\nname = \"m2\"\nlabel = \"M\"\ndst = \"cutters\"\nmaster = true\n",
    );
    assert!(e.contains("already has a master ('m1')"), "{e}");
}

#[test]
fn a_master_into_an_output_without_points_is_refused() {
    let e = refused("\n[[level]]\nname = \"m\"\nlabel = \"M\"\ndst = \"prog\"\nmaster = true\n");
    // prog has no outputs at all: the dst check says so first.
    assert!(e.contains("has no output channels"), "{e}");
}

#[test]
fn a_source_slider_without_sources_is_still_refused() {
    let e = refused("\n[[level]]\nname = \"x\"\nlabel = \"X\"\ndst = \"cutters\"\n");
    assert!(e.contains("needs at least one source"), "{e}");
}

#[test]
fn a_from_naming_a_slider_of_this_config_or_a_bad_name_is_refused() {
    let e = refused(
        "\n[[level]]\nname = \"a\"\nlabel = \"A\"\ndst = \"cutters\"\nsources = [\"cam1\"]\n\
         \n[[level]]\nname = \"b\"\nlabel = \"B\"\ndst = \"cutters\"\nsources = [\"cam2\"]\nfrom = \"a\"\n",
    );
    assert!(
        e.contains("'from' names 'a', a slider of this config"),
        "{e}"
    );
    let e = refused(
        "\n[[level]]\nname = \"b\"\nlabel = \"B\"\ndst = \"cutters\"\nsources = [\"cam2\"]\nfrom = \"Bad Name\"\n",
    );
    assert!(e.contains("'from'"), "{e}");
}

// ---- the engine -----------------------------------------------------------------------------

#[test]
fn at_0_db_with_a_master_the_mix_is_bit_identical_to_the_matrix_without_sliders() {
    let with = matrix();
    let without = Matrix::from_toml(HEAD).unwrap();
    let a = Engine::new(with.clone()).mix_block(&input(&with), 4);
    let b = Engine::new(without.clone()).mix_block(&input(&without), 4);
    for id in 0..with.participants.len() {
        assert_eq!(
            a.interleaved(id, 4),
            b.interleaved(id, 4),
            "participant {id}"
        );
    }
    assert_eq!(
        cans(&with, &Engine::new(with.clone())),
        expected(1.0, 1.0, 1.0, 1.0, 1.0)
    );
}

#[test]
fn one_camera_fader_moves_only_that_camera() {
    let m = matrix();
    let e = Engine::new(m.clone());
    let base = cans(&m, &e);
    let talk = talkback(&m, &e);
    m.levels.set(CAM2, -12.0, false).unwrap();
    let got = cans(&m, &e);
    assert_ne!(got, base);
    assert_eq!(got, expected(1.0, 1.0, 1.0, gain_linear(-12.0), 1.0));
    // cam1's share and every other output untouched.
    assert_eq!(talkback(&m, &e), talk);
    m.levels.set(CAM2, 0.0, false).unwrap();
    assert_eq!(cans(&m, &e), base);
}

#[test]
fn the_master_trims_the_whole_output_on_top_of_the_source_trims() {
    let m = matrix();
    let e = Engine::new(m.clone());
    let talk = talkback(&m, &e);
    m.levels.set(MASTER, -6.0, false).unwrap();
    assert_eq!(
        cans(&m, &e),
        expected(1.0, 1.0, 1.0, 1.0, gain_linear(-6.0))
    );
    m.levels.set(CAM1, -6.0, false).unwrap();
    m.levels.set(FOH, 3.0, false).unwrap();
    assert_eq!(
        cans(&m, &e),
        expected(
            gain_linear(3.0),
            1.0,
            gain_linear(-6.0),
            1.0,
            gain_linear(-6.0)
        )
    );
    // The cameramen's talkback is no part of the cutters' output.
    assert_eq!(talkback(&m, &e), talk);
}

#[test]
fn a_master_boost_clamps_at_full_scale() {
    let m = matrix();
    let e = Engine::new(m.clone());
    m.levels.set(MASTER, 12.0, false).unwrap();
    let got = cans(&m, &e);
    assert_eq!(got, expected(1.0, 1.0, 1.0, 1.0, gain_linear(12.0)));
    assert!(got
        .iter()
        .flatten()
        .any(|&s| s == i16::MAX || s == i16::MIN));
}

#[test]
fn a_muted_master_silences_the_output_and_keeps_its_db_and_the_others() {
    let m = matrix();
    let e = Engine::new(m.clone());
    let talk = talkback(&m, &e);
    m.levels.set(CAM1, -3.0, false).unwrap();
    m.levels.set(MASTER, -4.5, true).unwrap();
    assert_eq!(cans(&m, &e), vec![vec![0; 4], vec![0; 4]]);
    assert_eq!(talkback(&m, &e), talk);
    let st = m.levels.states();
    assert_eq!((st[MASTER].db, st[MASTER].muted), (-4.5, true));
    assert_eq!((st[CAM1].db, st[CAM1].muted), (-3.0, false));
    // Unmuted: back to the master's dB on top of cam1's trim.
    m.levels.set(MASTER, -4.5, false).unwrap();
    assert_eq!(
        cans(&m, &e),
        expected(1.0, 1.0, gain_linear(-3.0), 1.0, gain_linear(-4.5))
    );
}

// ---- the #34 state file migrates ------------------------------------------------------------

const OLD_FILE: &str =
    "# intercom-hub level sliders (intercom#34); written by the hub on every change.\n\
[levels.click]\ndb = -2.0\nmuted = false\n\n\
[levels.kamery]\ndb = -30.0\nmuted = true\n\n\
[levels.program]\ndb = -4.5\nmuted = false\n\n\
[levels.telefony]\ndb = 1.0\nmuted = false\n";

#[test]
fn a_group_value_carries_over_to_its_new_per_source_sliders() {
    let m = matrix();
    let r = m.levels.apply_stored(OLD_FILE).unwrap();
    let st = m.levels.states();
    assert_eq!(
        (st[FOH].db, st[FOH].muted),
        (-4.5, false),
        "foh from program"
    );
    assert_eq!(
        (st[CLICK].db, st[CLICK].muted),
        (-2.0, false),
        "click keeps its name"
    );
    assert_eq!(
        (st[CAM1].db, st[CAM1].muted),
        (-30.0, true),
        "cam1 from kamery"
    );
    assert_eq!(
        (st[CAM2].db, st[CAM2].muted),
        (-20.0, true),
        "cam2 from kamery, clamped into its -20 dB floor"
    );
    assert_eq!(
        (st[MASTER].db, st[MASTER].muted),
        (0.0, false),
        "the master is new: 0 dB"
    );
    assert_eq!(m.levels.linear(CAM1), 0.0);
    assert_eq!(m.levels.linear(FOH), gain_linear(-4.5));
    assert_eq!(
        r.migrated,
        vec![
            ("foh".to_string(), "program".to_string()),
            ("cam1".to_string(), "kamery".to_string()),
            ("cam2".to_string(), "kamery".to_string()),
        ]
    );
    assert_eq!(r.clamped, vec!["cam2".to_string()]);
    // telefony is not a slider of this small config and no `from` names it.
    assert_eq!(r.unknown, vec!["telefony".to_string()]);
}

#[test]
fn a_new_sliders_own_stored_value_wins_over_its_group() {
    let m = matrix();
    let r = m
        .levels
        .apply_stored(&format!(
            "{OLD_FILE}\n[levels.cam1]\ndb = -1.5\nmuted = false\n"
        ))
        .unwrap();
    let st = m.levels.states();
    assert_eq!((st[CAM1].db, st[CAM1].muted), (-1.5, false));
    assert_eq!((st[CAM2].db, st[CAM2].muted), (-20.0, true));
    assert!(!r.migrated.iter().any(|(n, _)| n == "cam1"));
}

#[test]
fn after_the_first_change_the_file_holds_the_new_names_and_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("levels.toml");
    std::fs::write(&path, OLD_FILE).unwrap();
    let m = matrix();
    m.levels.load_file(&path);
    m.levels.set(CLICK, -3.0, false).unwrap();
    m.levels.save_file(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    for name in ["foh", "click", "cam1", "cam2", "master"] {
        assert!(text.contains(&format!("[levels.{name}]")), "{name}: {text}");
    }
    for old in ["program", "kamery"] {
        assert!(!text.contains(&format!("[levels.{old}]")), "{old}: {text}");
    }
    let fresh = matrix();
    fresh.levels.load_file(&path);
    assert_eq!(fresh.levels.states(), m.levels.states());
    assert_eq!(fresh.levels.states()[FOH].db, -4.5);
    assert!(fresh.levels.states()[CAM1].muted);
}

#[test]
fn a_file_without_old_groups_or_new_names_keeps_0_db() {
    let m = matrix();
    let r = m.levels.apply_stored("[levels.weird]\ndb = 3.0\n").unwrap();
    assert!(r.migrated.is_empty());
    assert_eq!(r.unknown, vec!["weird".to_string()]);
    assert!(m.levels.states().iter().all(|s| s.db == 0.0 && !s.muted));
}

// ---- the API --------------------------------------------------------------------------------

#[test]
fn the_api_marks_the_master_additively() {
    let m = matrix();
    m.levels.set(MASTER, -2.0, false).unwrap();
    let body =
        serde_json::to_value(levels_body(&m.levels, &HubState::snapshot(&m, "t", &[]))).unwrap();
    let levels = body["levels"].as_array().unwrap();
    assert_eq!(levels.len(), 5);
    assert_eq!(levels[MASTER]["name"], "master");
    assert_eq!(levels[MASTER]["master"], true);
    assert_eq!(levels[MASTER]["db"], -2.0);
    assert_eq!(levels[MASTER]["sources"].as_array().unwrap().len(), 0);
    assert_eq!(levels[MASTER]["points"], 8);
    assert_eq!(levels[CAM1]["master"], false);
    // The #34 fields are all still there.
    for key in [
        "name", "label", "dst", "sources", "db", "muted", "min_db", "max_db", "points",
    ] {
        assert!(levels[CAM1].get(key).is_some(), "{key}");
    }
    let state = serde_json::to_value(HubState::snapshot(&m, "t", &[])).unwrap();
    assert_eq!(state["levels"][MASTER]["master"], true);
    assert_eq!(state["levels"][FOH]["master"], false);
}

// ---- the deployed TOMLs ---------------------------------------------------------------------

fn deployed(text: &str) -> Matrix {
    Matrix::from_toml(text).unwrap()
}

/// (name, label, from) of every slider, and the master's point count.
fn shape(m: &Matrix) -> Vec<(String, String, Option<String>, bool)> {
    m.levels
        .specs()
        .iter()
        .map(|s| (s.name.clone(), s.label.clone(), s.from.clone(), s.master))
        .collect()
}

fn row(name: &str, label: &str, from: Option<&str>) -> (String, String, Option<String>, bool) {
    (name.into(), label.into(), from.map(str::to_string), false)
}

fn cam_rows() -> Vec<(String, String, Option<String>, bool)> {
    (1..=7)
        .map(|i| row(&format!("cam{i}"), &format!("Kamera {i}"), Some("kamery")))
        .collect()
}

#[test]
fn strih_snv_has_a_slider_per_source_and_a_master() {
    let m = deployed(include_str!("../../intercom.strih-snv.toml"));
    let mut want = vec![
        row("foh", "FOH", Some("program")),
        row("lv1", "LV1", Some("program")),
        row("click", "Click", None),
    ];
    want.extend(cam_rows());
    want.push(row("telefony", "Telefóny", None));
    want.push(("master".into(), "Master".into(), None, true));
    assert_eq!(shape(&m), want);
    let master = m.levels.specs().last().unwrap();
    let c = m.id_of("cutters").unwrap();
    assert_eq!(
        master.points,
        m.points.iter().filter(|p| p.dst == c).count()
    );
    assert_eq!(master.points, 44);
}

#[test]
fn strih_pp_has_a_slider_per_source_and_a_master() {
    let m = deployed(include_str!("../../intercom.strih-pp.toml"));
    let mut want = vec![row("program", "Program", None)];
    want.extend(cam_rows());
    want.push(row("telefony", "Telefóny", None));
    want.push(("master".into(), "Master".into(), None, true));
    assert_eq!(shape(&m), want);
    assert_eq!(m.levels.specs().last().unwrap().points, 18);
}

#[test]
fn a_real_34_state_file_migrates_on_strih_snv() {
    let m = deployed(include_str!("../../intercom.strih-snv.toml"));
    let r = m.levels.apply_stored(OLD_FILE).unwrap();
    assert!(r.unknown.is_empty(), "{:?}", r.unknown);
    for s in m.levels.states() {
        let want = match s.name.as_str() {
            "foh" | "lv1" => (-4.5, false),
            "click" => (-2.0, false),
            "telefony" => (1.0, false),
            "master" => (0.0, false),
            n if n.starts_with("cam") => (-30.0, true),
            n => panic!("unexpected slider {n}"),
        };
        assert_eq!((s.db, s.muted), want, "{}", s.name);
    }
}

#[test]
fn a_real_34_state_file_migrates_on_strih_pp() {
    let m = deployed(include_str!("../../intercom.strih-pp.toml"));
    let r = m.levels.apply_stored(OLD_FILE).unwrap();
    // strih-pp never had a click slider: its #34 file has none, so here click is unknown.
    assert_eq!(r.unknown, vec!["click".to_string()]);
    for s in m.levels.states() {
        let want = match s.name.as_str() {
            "program" => (-4.5, false),
            "telefony" => (1.0, false),
            "master" => (0.0, false),
            n if n.starts_with("cam") => (-30.0, true),
            n => panic!("unexpected slider {n}"),
        };
        assert_eq!((s.db, s.muted), want, "{}", s.name);
    }
}
