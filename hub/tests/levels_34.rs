//! intercom#34: the cutters' level sliders — the `[[level]]` resolution and its refusals, the
//! engine trim / mute / 0 dB identity, the state file, the loopback control routes, the
//! `/api/state` facets, and both deployed strih TOMLs' sliders.

use std::sync::Arc;

use intercom_hub::dante_guard::DanteGuard;
use intercom_hub::engine::{Engine, InputBlock};
use intercom_hub::levels::{write_atomic, DEFAULT_CONTROL_BIND, DEFAULT_LEVELS_FILE};
use intercom_hub::levels_http::{
    control_router, levels_body, levels_css_asset, levels_index, levels_js_asset,
    start_level_controls, ControlState,
};
use intercom_hub::matrix::Matrix;
use intercom_hub::state::{HubState, RuntimeStats};

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
src = "cutters"
in_ch = 1
dst = "cam1"
out_ch = 1
"#;

const SLIDERS: &str = r#"
[[level]]
name = "program"
label = "Program"
dst = "cutters"
sources = ["prog:1", "prog:2"]

[[level]]
name = "click"
label = "Click"
dst = "cutters"
sources = ["prog:3"]
min_db = -40.0
max_db = 6.0

[[level]]
name = "kamery"
label = "Kamery"
dst = "cutters"
sources = ["cam1"]
"#;

fn matrix() -> Matrix {
    Matrix::from_toml(&format!("{HEAD}{SLIDERS}")).unwrap()
}

fn refused(extra: &str) -> String {
    Matrix::from_toml(&format!("{HEAD}{extra}"))
        .unwrap_err()
        .to_string()
}

fn level(name: &str, sources: &str) -> String {
    format!(
        "\n[[level]]\nname = \"{name}\"\nlabel = \"L\"\ndst = \"cutters\"\nsources = {sources}\n"
    )
}

// ---- matrix resolution ----------------------------------------------------------------------

#[test]
fn each_slider_claims_exactly_its_points() {
    let m = matrix();
    let lv = &m.levels;
    assert_eq!(lv.len(), 3);
    let by = |src: &str, in_ch: usize| -> Vec<Option<usize>> {
        let s = m.id_of(src).unwrap();
        m.points
            .iter()
            .filter(|p| p.src == s && p.in_ch == in_ch && p.dst == m.id_of("cutters").unwrap())
            .map(|p| p.level)
            .collect()
    };
    assert_eq!(by("prog", 1), vec![Some(0)]);
    assert_eq!(by("prog", 2), vec![Some(0)]);
    assert_eq!(by("prog", 3), vec![Some(1), Some(1)]);
    assert_eq!(by("cam1", 1), vec![Some(2), Some(2)]);
    // The talkback back to the camera is no slider's.
    let back = m
        .points
        .iter()
        .find(|p| p.dst == m.id_of("cam1").unwrap())
        .unwrap();
    assert_eq!(back.level, None);
    let counts: Vec<usize> = lv.specs().iter().map(|s| s.points).collect();
    assert_eq!(counts, vec![2, 2, 2]);
    assert_eq!(lv.specs()[1].min_db, -40.0);
    assert_eq!(lv.specs()[1].max_db, 6.0);
    assert_eq!(lv.specs()[0].min_db, -60.0, "default range");
    assert_eq!(lv.specs()[0].max_db, 12.0, "default range");
}

#[test]
fn a_whole_participant_source_takes_every_channel() {
    let m = Matrix::from_toml(&format!("{HEAD}{}", level("all", r#"["prog"]"#))).unwrap();
    assert_eq!(m.levels.specs()[0].points, 4);
}

#[test]
fn the_hub_defaults_name_the_loopback_listener_and_the_state_directory() {
    let m = matrix();
    assert_eq!(m.hub.control_bind, DEFAULT_CONTROL_BIND);
    assert_eq!(m.hub.control_bind, "127.0.0.1:8791");
    assert_eq!(m.hub.levels_file, DEFAULT_LEVELS_FILE);
    assert_eq!(m.hub.levels_file, "/var/lib/intercom-hub/levels.toml");
}

#[test]
fn a_matrix_without_sliders_has_none_and_no_point_is_trimmed() {
    let m = Matrix::from_toml(HEAD).unwrap();
    assert!(m.levels.is_empty());
    assert!(m.points.iter().all(|p| p.level.is_none()));
}

#[test]
fn an_unknown_source_participant_is_refused() {
    let e = refused(&level("x", r#"["ghost"]"#));
    assert!(e.contains("unknown source participant 'ghost'"), "{e}");
}

#[test]
fn an_unknown_dst_is_refused() {
    let e = refused(
        "\n[[level]]\nname = \"x\"\nlabel = \"L\"\ndst = \"ghost\"\nsources = [\"cam1\"]\n",
    );
    assert!(e.contains("unknown dst participant 'ghost'"), "{e}");
}

#[test]
fn a_dst_without_outputs_is_refused() {
    let e =
        refused("\n[[level]]\nname = \"x\"\nlabel = \"L\"\ndst = \"prog\"\nsources = [\"cam1\"]\n");
    assert!(e.contains("has no output channels"), "{e}");
}

#[test]
fn a_channel_outside_the_source_is_refused() {
    let e = refused(&level("x", r#"["prog:4"]"#));
    assert!(e.contains("channel 4 out of range 1..=3"), "{e}");
    let e = refused(&level("x", r#"["prog:0"]"#));
    assert!(e.contains("channel 0 out of range"), "{e}");
    let e = refused(&level("x", r#"["prog:one"]"#));
    assert!(e.contains("must be a number"), "{e}");
}

#[test]
fn a_source_with_no_point_into_dst_is_refused() {
    // cutters -> cam1 exists, but nothing from "cutters" goes into "cutters".
    let e = refused(&level("x", r#"["cutters"]"#));
    assert!(
        e.contains("source 'cutters' has no point into 'cutters'"),
        "{e}"
    );
}

#[test]
fn a_point_claimed_by_two_sliders_is_refused() {
    let e = refused(&format!(
        "{}{}",
        level("a", r#"["prog:1"]"#),
        level("b", r#"["prog"]"#)
    ));
    assert!(e.contains("already trimmed by level 'a'"), "{e}");
    // Inside one slider too.
    let e = refused(&level("a", r#"["prog", "prog:1"]"#));
    assert!(e.contains("already trimmed by level 'a'"), "{e}");
}

#[test]
fn a_bad_range_is_refused() {
    for (min, max) in [
        ("-10.0", "-1.0"),
        ("1.0", "10.0"),
        ("0.0", "0.0"),
        ("-200.0", "6.0"),
        ("-60.0", "40.0"),
        ("nan", "6.0"),
    ] {
        let e = refused(&format!(
            "\n[[level]]\nname = \"x\"\nlabel = \"L\"\ndst = \"cutters\"\nsources = [\"cam1\"]\nmin_db = {min}\nmax_db = {max}\n"
        ));
        assert!(e.contains("must hold 0 dB"), "{min}..{max}: {e}");
    }
}

#[test]
fn a_bad_or_repeated_name_or_an_empty_label_is_refused() {
    assert!(refused(&level("Program", r#"["cam1"]"#)).contains("the name must be"));
    assert!(refused(&level("a/b", r#"["cam1"]"#)).contains("the name must be"));
    assert!(refused(&level("", r#"["cam1"]"#)).contains("the name must be"));
    let twice = format!(
        "{}{}",
        level("a", r#"["cam1"]"#),
        level("a", r#"["prog:1"]"#)
    );
    assert!(refused(&twice).contains("declared twice"));
    let e = refused(
        "\n[[level]]\nname = \"x\"\nlabel = \" \"\ndst = \"cutters\"\nsources = [\"cam1\"]\n",
    );
    assert!(e.contains("label must not be empty"), "{e}");
    assert!(refused(&level("x", "[]")).contains("at least one source"));
    let e = refused("\n[[level]]\nname = \"x\"\nlabel = \"L\"\ndst = \"cutters\"\nsources = [\"cam1\"]\ngain = 1\n");
    assert!(e.contains("unknown field"), "{e}");
}

#[test]
fn a_control_bind_off_loopback_is_refused() {
    let off = HEAD.replace(
        "block_frames = 4\n",
        "block_frames = 4\ncontrol_bind = \"0.0.0.0:8791\"\n",
    );
    let e = Matrix::from_toml(&off).unwrap_err().to_string();
    assert!(e.contains("must be a loopback address"), "{e}");
    let bad = HEAD.replace(
        "block_frames = 4\n",
        "block_frames = 4\ncontrol_bind = \"nope\"\n",
    );
    let e = Matrix::from_toml(&bad).unwrap_err().to_string();
    assert!(e.contains("is not host:port"), "{e}");
}

// ---- the deployed TOMLs ---------------------------------------------------------------------

fn trimmed_into_cutters(m: &Matrix) -> Vec<(String, Vec<String>, usize)> {
    m.levels
        .specs()
        .iter()
        .map(|s| (s.name.clone(), s.sources.clone(), s.points))
        .collect()
}

fn cams() -> Vec<String> {
    (1..=7).map(|i| format!("cam{i}")).collect()
}

fn every_cutters_point_is_trimmed(m: &Matrix) {
    let c = m.id_of("cutters").unwrap();
    for p in m.points.iter().filter(|p| p.dst == c) {
        assert!(
            p.level.is_some(),
            "{}:{} -> cutters:{} has no slider",
            m.participants[p.src].name,
            p.in_ch,
            p.out_ch
        );
        assert!(
            !m.levels.specs()[p.level.unwrap()].master,
            "a point's slider is a source slider, never the master"
        );
    }
}

// intercom#38 replaced the four group sliders with one slider per source plus a master: the pins
// below follow the new names; what they guard is unchanged (every cutters point is trimmed by
// exactly one SOURCE slider, `every_cutters_point_is_trimmed`; the master trims the whole output
// and claims no point, pinned in `levels_38.rs`).
fn cam_sliders(points_each: usize) -> Vec<(String, Vec<String>, usize)> {
    cams()
        .into_iter()
        .map(|c| (c.clone(), vec![c], points_each))
        .collect()
}

#[test]
fn strih_snv_has_a_slider_per_source_and_a_master() {
    let m = Matrix::from_toml(include_str!("../../intercom.strih-snv.toml")).unwrap();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let mut want = vec![
        ("foh".into(), s(&["foh_dante:1", "foh_dante:2"]), 4),
        ("lv1".into(), s(&["lv1"]), 4),
        ("click".into(), s(&["foh_dante:3"]), 4),
    ];
    want.extend(cam_sliders(4));
    want.push(("telefony".into(), s(&["phones"]), 4));
    want.push(("master".into(), vec![], 44));
    assert_eq!(trimmed_into_cutters(&m), want);
    let labels: Vec<&str> = m.levels.specs().iter().map(|s| s.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "FOH",
            "LV1",
            "Click",
            "Kamera 1",
            "Kamera 2",
            "Kamera 3",
            "Kamera 4",
            "Kamera 5",
            "Kamera 6",
            "Kamera 7",
            "Telefóny",
            "Master"
        ]
    );
    every_cutters_point_is_trimmed(&m);
    assert_eq!(m.hub.control_bind, "127.0.0.1:8791");
}

#[test]
fn strih_pp_has_a_slider_per_source_and_a_master() {
    let m = Matrix::from_toml(include_str!("../../intercom.strih-pp.toml")).unwrap();
    let mut want = vec![("program".into(), vec!["dsg_dante".to_string()], 2)];
    want.extend(cam_sliders(2));
    want.push(("telefony".into(), vec!["phones".to_string()], 2));
    want.push(("master".into(), vec![], 18));
    assert_eq!(trimmed_into_cutters(&m), want);
    every_cutters_point_is_trimmed(&m);
}

#[test]
fn the_stream_boxes_have_no_sliders() {
    for t in [
        include_str!("../../intercom.stream-snv.toml"),
        include_str!("../../intercom.stream-pp.toml"),
    ] {
        assert!(Matrix::from_toml(t).unwrap().levels.is_empty());
    }
}

// ---- the engine -----------------------------------------------------------------------------

fn input(m: &Matrix) -> InputBlock {
    let mut b = InputBlock::silent(m.participants.len());
    b.set(
        m.id_of("cam1").unwrap(),
        vec![vec![1000, -2000, 3000, -4000]],
    );
    b.set(
        m.id_of("prog").unwrap(),
        vec![
            vec![10000, 10000, -10000, 5000],
            vec![-7000, 7000, 0, 1],
            vec![20000, -20000, 123, 4567],
        ],
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

#[test]
fn at_0_db_the_mix_is_bit_identical_to_the_matrix_without_sliders() {
    let with = matrix();
    let without = Matrix::from_toml(HEAD).unwrap();
    let e_with = Engine::new(with.clone());
    let e_without = Engine::new(without.clone());
    assert_eq!(cans(&with, &e_with), cans(&without, &e_without));
    // Every output, not just the cans.
    let a = e_with.mix_block(&input(&with), 4);
    let b = e_without.mix_block(&input(&without), 4);
    for id in 0..with.participants.len() {
        assert_eq!(a.interleaved(id, 4), b.interleaved(id, 4));
    }
}

#[test]
fn a_trim_scales_exactly_its_points() {
    let m = matrix();
    let e = Engine::new(m.clone());
    // Kamery alone at -6 dB: the cam1 share halves, the program shares stay.
    let base = cans(&m, &e);
    m.levels.set(2, -6.0, false).unwrap();
    let trimmed = cans(&m, &e);
    let k = 10f32.powf(-6.0 / 20.0);
    let cam = [1000f32, -2000.0, 3000.0, -4000.0];
    let g = |db: f32| 10f32.powf(db / 20.0);
    let prog: [[f32; 4]; 3] = [
        [10000.0, 10000.0, -10000.0, 5000.0],
        [-7000.0, 7000.0, 0.0, 1.0],
        [20000.0, -20000.0, 123.0, 4567.0],
    ];
    for i in 0..4 {
        let l = prog[0][i] * g(-8.0) + prog[2][i] * g(-10.0) + cam[i] * k;
        let r = prog[1][i] * g(-8.0) + prog[2][i] * g(-10.0) + cam[i] * k;
        assert_eq!(trimmed[0][i], l.round() as i16, "L[{i}]");
        assert_eq!(trimmed[1][i], r.round() as i16, "R[{i}]");
    }
    assert_ne!(base, trimmed);
    // Back to 0 dB = the base again, bit for bit.
    m.levels.set(2, 0.0, false).unwrap();
    assert_eq!(cans(&m, &e), base);
}

#[test]
fn a_boost_raises_its_points_on_top_of_the_toml_gain() {
    let m = matrix();
    let e = Engine::new(m.clone());
    m.levels.set(1, 6.0, false).unwrap(); // click: -10 dB base + 6 = -4 dB
    let only_click = |m: &Matrix| {
        let mut b = InputBlock::silent(m.participants.len());
        b.set(
            m.id_of("prog").unwrap(),
            vec![vec![0; 4], vec![0; 4], vec![10000; 4]],
        );
        b
    };
    let out = e.mix_block(&only_click(&m), 4);
    let c = m.id_of("cutters").unwrap();
    let want = (10000.0 * 10f32.powf(-4.0 / 20.0)).round() as i16;
    assert_eq!(out.channel(c, 1).unwrap(), &[want; 4]);
    assert_eq!(out.channel(c, 2).unwrap(), &[want; 4]);
}

#[test]
fn mute_silences_only_its_points_and_keeps_the_db() {
    let m = matrix();
    let e = Engine::new(m.clone());
    m.levels.set(0, -3.0, true).unwrap(); // program muted
    m.levels.set(1, -40.0, true).unwrap(); // click muted
    let got = cans(&m, &e);
    assert_eq!(
        got[0],
        vec![1000, -2000, 3000, -4000],
        "only the camera remains"
    );
    assert_eq!(got[1], vec![1000, -2000, 3000, -4000]);
    let st = m.levels.states();
    assert_eq!((st[0].db, st[0].muted), (-3.0, true));
    // The camera's talkback leg is untouched by the cans' sliders.
    let out = e.mix_block(&input(&m), 4);
    assert_eq!(
        out.channel(m.id_of("cam1").unwrap(), 1).unwrap(),
        &[0, 0, 0, 0]
    );
}

#[test]
fn a_value_outside_the_range_is_refused_and_changes_nothing() {
    let m = matrix();
    m.levels.set(1, 5.0, false).unwrap();
    for bad in [6.5, -40.5, f32::NAN, f32::INFINITY] {
        let e = m.levels.set(1, bad, false).unwrap_err().to_string();
        assert!(e.contains("outside -40 .. 6 dB"), "{bad}: {e}");
    }
    assert_eq!(m.levels.states()[1].db, 5.0);
    assert_eq!(m.levels.linear(1), 10f32.powf(5.0 / 20.0));
}

#[test]
fn set_reports_the_old_and_the_new_value() {
    let m = matrix();
    let c = m.levels.set(0, -4.5, false).unwrap();
    assert_eq!(
        (c.old_db, c.old_muted, c.db, c.muted),
        (0.0, false, -4.5, false)
    );
    let c = m.levels.set(0, -4.5, true).unwrap();
    assert_eq!((c.old_db, c.old_muted, c.muted), (-4.5, false, true));
    assert_eq!(m.levels.linear(0), 0.0);
}

#[test]
fn the_output_peak_is_the_largest_sample_of_every_channel() {
    let m = matrix();
    let e = Engine::new(m.clone());
    let out = e.mix_block(&input(&m), 4);
    let c = m.id_of("cutters").unwrap();
    let want = (1..=2)
        .flat_map(|ch| out.channel(c, ch).unwrap().to_vec())
        .map(|s| s.saturating_abs())
        .max()
        .unwrap();
    assert!(want > 0);
    assert_eq!(out.peak_abs(c), want);
    assert_eq!(out.peak_abs(m.id_of("prog").unwrap()), 0, "no outputs");
    assert_eq!(out.peak_abs(99), 0);
}

// ---- the state file -------------------------------------------------------------------------

#[test]
fn the_state_file_round_trips() {
    let m = matrix();
    m.levels.set(0, -7.5, false).unwrap();
    m.levels.set(1, 3.0, true).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("levels.toml");
    m.levels.save_file(&path).unwrap();
    let fresh = matrix();
    fresh.levels.load_file(&path);
    assert_eq!(fresh.levels.states(), m.levels.states());
    assert_eq!(fresh.levels.linear(1), 0.0, "muted survives");
    assert_eq!(fresh.levels.linear(0), 10f32.powf(-7.5 / 20.0));
}

#[test]
fn stored_values_are_clamped_and_unknown_names_ignored() {
    let m = matrix();
    let r = m
        .levels
        .apply_stored(
            "[levels.click]\ndb = 30.0\n[levels.program]\ndb = -99.0\nmuted = true\n[levels.gone]\ndb = 1.0\n",
        )
        .unwrap();
    assert_eq!(r.unknown, vec!["gone".to_string()]);
    let mut clamped = r.clamped.clone();
    clamped.sort();
    assert_eq!(clamped, vec!["click".to_string(), "program".to_string()]);
    let st = m.levels.states();
    assert_eq!((st[0].db, st[0].muted), (-60.0, true));
    assert_eq!((st[1].db, st[1].muted), (6.0, false));
    assert_eq!(
        (st[2].db, st[2].muted),
        (0.0, false),
        "not in the file: 0 dB"
    );
}

#[test]
fn a_corrupt_or_missing_state_file_keeps_0_db_and_is_never_fatal() {
    let m = matrix();
    assert!(m.levels.apply_stored("this is [not toml").is_err());
    assert!(m
        .levels
        .apply_stored("[levels.program]\ndb = \"loud\"\n")
        .is_err());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("levels.toml");
    std::fs::write(&path, "garbage = [").unwrap();
    m.levels.load_file(&path);
    m.levels.load_file(&dir.path().join("missing.toml"));
    m.levels.load_file(dir.path()); // a directory: unreadable
    assert!(m.levels.states().iter().all(|s| s.db == 0.0 && !s.muted));
    assert!((0..3).all(|i| m.levels.linear(i) == 1.0));
}

#[test]
fn the_atomic_write_replaces_the_file_whole_and_leaves_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("levels.toml");
    std::fs::write(&path, "old").unwrap();
    write_atomic(&path, "new contents").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new contents");
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["levels.toml".to_string()]);
    // A directory that does not exist fails loud, never silently.
    assert!(write_atomic(&dir.path().join("no/such/levels.toml"), "x").is_err());
}

// ---- /api/state -----------------------------------------------------------------------------

#[test]
fn api_state_carries_the_levels_and_the_outputs_meter() {
    let m = matrix();
    m.levels.set(0, -2.5, false).unwrap();
    let mut stats = vec![RuntimeStats::default(); m.participants.len()];
    for s in &mut stats {
        s.out_level_dbfs = Some(-18.0);
    }
    let v = serde_json::to_value(HubState::snapshot(&m, "t", &stats)).unwrap();
    assert_eq!(v["levels"][0]["name"], "program");
    assert_eq!(v["levels"][0]["db"], -2.5);
    assert_eq!(v["levels"][0]["dst"], "cutters");
    assert_eq!(v["levels"][1]["min_db"], -40.0);
    let p = |name: &str| {
        v["participants"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(p("cutters")["out_level_dbfs"], -18.0);
    assert_eq!(p("cam1")["out_level_dbfs"], -18.0);
    assert!(
        p("prog").get("out_level_dbfs").is_none(),
        "no outputs, no meter"
    );
    // The fields camera-box reads are still there.
    for key in [
        "name",
        "level_dbfs",
        "rx_packets",
        "tx_packets",
        "underruns",
    ] {
        assert!(p("cutters").get(key).is_some(), "{key}");
    }
    // A hub without sliders shows no levels key.
    let plain = Matrix::from_toml(HEAD).unwrap();
    let v = serde_json::to_value(HubState::snapshot(&plain, "t", &[])).unwrap();
    assert!(v.get("levels").is_none());
}

// ---- the control routes ---------------------------------------------------------------------

#[test]
fn the_page_assets_are_embedded_with_the_version() {
    let html = levels_index();
    assert!(!html.contains("{{VERSION}}"));
    assert!(html.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    assert!(html.contains("data-role=\"version\""));
    assert!(html.contains("/levels.js"));
    assert!(html.contains("lang=\"sk\""));
    assert!(levels_js_asset().contains("/api/levels"));
    assert!(
        !levels_js_asset().contains("console."),
        "the page never writes to the console"
    );
    assert!(levels_css_asset().contains(".strip"));
}

struct Control {
    base: String,
    file: std::path::PathBuf,
    levels: intercom_hub::levels::Levels,
    _dir: tempfile::TempDir,
}

async fn control(m: &Matrix) -> Control {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("levels.toml");
    let mut stats = vec![RuntimeStats::default(); m.participants.len()];
    stats[m.id_of("cutters").unwrap()].out_level_dbfs = Some(-21.5);
    let (_tx, rx) = tokio::sync::watch::channel(Arc::new(HubState::snapshot(m, "t", &stats)));
    let router = control_router(ControlState {
        levels: m.levels.clone(),
        live: rx,
        levels_file: file.clone(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(intercom_hub::http_serve::serve(
        listener,
        router,
        DanteGuard::new(None, Vec::new()),
        std::future::pending(),
    ));
    Control {
        base: format!("http://{addr}"),
        file,
        levels: m.levels.clone(),
        _dir: dir,
    }
}

async fn put(c: &Control, name: &str, body: &str) -> (u16, String) {
    let r = reqwest::Client::new()
        .put(format!("{}/api/levels/{name}", c.base))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

#[tokio::test]
async fn get_levels_lists_every_slider_and_the_cans_meter() {
    let m = matrix();
    let c = control(&m).await;
    let v: serde_json::Value = reqwest::get(format!("{}/api/levels", c.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    let names: Vec<&str> = v["levels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["program", "click", "kamery"]);
    assert_eq!(v["levels"][0]["label"], "Program");
    assert_eq!(v["levels"][0]["sources"][0], "prog:1");
    assert_eq!(v["outputs"][0]["participant"], "cutters");
    assert_eq!(v["outputs"][0]["out_level_dbfs"], -21.5);
    let body = levels_body(&m.levels, &HubState::snapshot(&m, "t", &[]));
    assert_eq!(body.outputs.len(), 1, "one meter per trimmed output");
    assert_eq!(body.outputs[0].out_level_dbfs, None);
}

#[tokio::test]
async fn the_page_and_its_assets_are_served() {
    let m = matrix();
    let c = control(&m).await;
    for (path, ctype, needle) in [
        ("/", "text/html", "data-role=\"strips\""),
        ("/levels.js", "text/javascript", "/api/levels"),
        ("/levels.css", "text/css", ".strip"),
        ("/favicon.svg", "image/svg+xml", "<svg"),
        (
            "/api/version",
            "application/json",
            env!("CARGO_PKG_VERSION"),
        ),
    ] {
        let r = reqwest::get(format!("{}{path}", c.base)).await.unwrap();
        assert_eq!(r.status().as_u16(), 200, "{path}");
        let t = r.headers()["content-type"].to_str().unwrap().to_string();
        assert!(t.starts_with(ctype), "{path}: {t}");
        assert!(r.text().await.unwrap().contains(needle), "{path}");
    }
    // The phone PWA's routes are not on this listener.
    let r = reqwest::get(format!("{}/api/state", c.base)).await.unwrap();
    assert_eq!(r.status().as_u16(), 404);
}

#[tokio::test]
async fn a_put_changes_the_mix_and_writes_the_state_file() {
    let m = matrix();
    let c = control(&m).await;
    let (code, body) = put(&c, "kamery", r#"{"db": -9.5, "muted": false}"#).await;
    assert_eq!(code, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["name"], "kamery");
    assert_eq!(v["db"], -9.5);
    assert_eq!(v["saved"], true);
    assert_eq!(
        c.levels.linear(2),
        10f32.powf(-9.5 / 20.0),
        "the engine reads it"
    );
    let fresh = matrix();
    fresh.levels.load_file(&c.file);
    assert_eq!(fresh.levels.states()[2].db, -9.5);
    let (code, _) = put(&c, "kamery", r#"{"db": -9.5, "muted": true}"#).await;
    assert_eq!(code, 200);
    assert_eq!(c.levels.linear(2), 0.0);
    let fresh = matrix();
    fresh.levels.load_file(&c.file);
    assert!(fresh.levels.states()[2].muted);
}

#[tokio::test]
async fn a_bad_put_is_refused_and_changes_nothing() {
    let m = matrix();
    let c = control(&m).await;
    for (name, body, code, needle) in [
        (
            "click",
            r#"{"db": 7.0, "muted": false}"#,
            400,
            "outside -40 .. 6 dB",
        ),
        ("click", r#"{"db": -41, "muted": false}"#, 400, "outside"),
        (
            "click",
            r#"{"db": "loud", "muted": false}"#,
            400,
            "body must be",
        ),
        ("click", r#"{"db": 1.0}"#, 400, "body must be"),
        (
            "click",
            r#"{"db": 1.0, "muted": false, "x": 1}"#,
            400,
            "body must be",
        ),
        ("click", "not json", 400, "body must be"),
        (
            "ghost",
            r#"{"db": 1.0, "muted": false}"#,
            404,
            "no level slider 'ghost'",
        ),
    ] {
        let (got, text) = put(&c, name, body).await;
        assert_eq!(got, code, "{body}: {text}");
        assert!(text.contains(needle), "{body}: {text}");
    }
    assert!(c.levels.states().iter().all(|s| s.db == 0.0 && !s.muted));
    assert!(!c.file.exists(), "nothing was saved");
}

#[tokio::test]
async fn a_failed_state_write_keeps_the_value_and_says_so() {
    let m = matrix();
    let dir = tempfile::tempdir().unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(Arc::new(HubState::snapshot(&m, "t", &[])));
    let router = control_router(ControlState {
        levels: m.levels.clone(),
        live: rx,
        levels_file: dir.path().join("no/such/dir/levels.toml"),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(intercom_hub::http_serve::serve(
        listener,
        router,
        DanteGuard::new(None, Vec::new()),
        std::future::pending(),
    ));
    let r = reqwest::Client::new()
        .put(format!("http://{addr}/api/levels/program"))
        .json(&serde_json::json!({"db": 2.0, "muted": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["saved"], false);
    assert_eq!(m.levels.states()[0].db, 2.0);
}

// ---- review round 1: DNS rebinding, the stored decimal ---------------------------------------

#[test]
fn only_a_loopback_host_name_is_accepted() {
    use intercom_hub::levels::is_loopback_host;
    for ok in [
        "127.0.0.1:8791",
        "127.0.0.1",
        "localhost:8791",
        "LocalHost",
        "[::1]:8791",
        "[::1]",
    ] {
        assert!(is_loopback_host(ok), "{ok}");
    }
    for bad in [
        "evil.example",
        "evil.example:8791",
        "127.0.0.1.evil.example:8791",
        "localhost.evil:8791",
        "[::1]x",
        "127.0.0.2:8791",
        "127.0.0.1:80a",
        "",
    ] {
        assert!(!is_loopback_host(bad), "{bad}");
    }
}

#[tokio::test]
async fn a_request_for_another_host_name_or_origin_is_refused() {
    let m = matrix();
    let c = control(&m).await;
    let client = reqwest::Client::new();
    // A page that re-bound evil.example to 127.0.0.1: its requests carry its own Host.
    let r = client
        .get(format!("{}/api/levels", c.base))
        .header("host", "evil.example:8791")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    let r = client
        .put(format!("{}/api/levels/program", c.base))
        .header("host", "evil.example")
        .json(&serde_json::json!({"db": 12.0, "muted": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    // A foreign Origin on a loopback Host.
    let r = client
        .put(format!("{}/api/levels/program", c.base))
        .header("origin", "http://evil.example")
        .json(&serde_json::json!({"db": 12.0, "muted": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    assert_eq!(c.levels.states()[0].db, 0.0, "nothing changed");
    // The page's own origin passes.
    let own = c.base.clone();
    let r = client
        .put(format!("{}/api/levels/program", c.base))
        .header("origin", own)
        .json(&serde_json::json!({"db": 1.5, "muted": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let r = client
        .get(format!("{}/", c.base))
        .header("host", "localhost:8791")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
}

#[test]
fn the_state_file_keeps_the_short_decimal() {
    let m = matrix();
    m.levels.set(0, -7.3, false).unwrap();
    let text = m.levels.render_stored().unwrap();
    assert!(text.contains("db = -7.3\n"), "{text}");
    let fresh = matrix();
    fresh.levels.apply_stored(&text).unwrap();
    assert_eq!(fresh.levels.states()[0].db, -7.3f32);
}

#[test]
fn the_snapshot_carries_the_sliders_and_reads_them_when_serialized() {
    // The hub-mix thread builds the snapshot; a change after that still shows when it is served.
    let m = matrix();
    let snap = HubState::snapshot(&m, "t", &[]);
    m.levels.set(1, -12.0, true).unwrap();
    let v = serde_json::to_value(&snap).unwrap();
    assert_eq!(v["levels"][1]["db"], -12.0);
    assert_eq!(v["levels"][1]["muted"], true);
}

/// The matrix with its control listener on `addr`.
fn matrix_on(addr: std::net::SocketAddr) -> Matrix {
    let head = HEAD.replace(
        "block_frames = 4\n",
        &format!("block_frames = 4\ncontrol_bind = \"{addr}\"\n"),
    );
    Matrix::from_toml(&format!("{head}{SLIDERS}")).unwrap()
}

fn live(m: &Matrix) -> tokio::sync::watch::Receiver<Arc<HubState>> {
    let stats = vec![RuntimeStats::default(); m.participants.len()];
    tokio::sync::watch::channel(Arc::new(HubState::snapshot(m, "t", &stats))).1
}

// Review finding (intercom#34): the sliders are a convenience, the audio is not. A taken control
// port must leave the hub running without sliders, never stop every camera, phone and cans leg.
#[tokio::test]
async fn a_taken_control_port_leaves_the_hub_running_without_sliders() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let m = matrix_on(taken.local_addr().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let up = start_level_controls(
        &m,
        &DanteGuard::new(None, Vec::new()),
        live(&m),
        dir.path().join("levels.toml"),
        std::future::pending(),
    )
    .await
    .expect("a taken port is no error that would stop the hub");
    assert_eq!(up, None, "no sliders");
}

#[tokio::test]
async fn a_free_control_port_serves_the_sliders() {
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = free.local_addr().unwrap();
    drop(free);
    let m = matrix_on(addr);
    let dir = tempfile::tempdir().unwrap();
    let up = start_level_controls(
        &m,
        &DanteGuard::new(None, Vec::new()),
        live(&m),
        dir.path().join("levels.toml"),
        std::future::pending(),
    )
    .await
    .unwrap();
    assert_eq!(up, Some(addr));
    let v: serde_json::Value = reqwest::get(format!("http://{addr}/api/levels"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!v["levels"].as_array().unwrap().is_empty(), "{v}");
}

#[tokio::test]
async fn a_control_bind_the_dante_guard_refuses_still_stops_the_hub() {
    // A config error, not a busy port: with a Dante leg the hub binds IPv4 only (intercom#25).
    let m = matrix_on("[::1]:8791".parse().unwrap());
    let guard = DanteGuard::new(
        None,
        vec![intercom_hub::dante_guard::Ipv4Net::new("10.77.7.0".parse().unwrap(), 24).unwrap()],
    );
    let dir = tempfile::tempdir().unwrap();
    let e = start_level_controls(
        &m,
        &guard,
        live(&m),
        dir.path().join("l.toml"),
        std::future::pending(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("control_bind [::1]:8791 is IPv6"), "{e}");
}
