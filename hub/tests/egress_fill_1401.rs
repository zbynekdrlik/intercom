//! Issue 1401, live 4.10.2026 (design comment 5979128757): the two hub EGRESS faults behind the
//! "extreme" choppy audio after the production.
//!
//! - **The pw-cat playback pipes ran empty.** pw-cat `fread`s a whole 1024-frame graph quantum from
//!   its stdin pipe in every process callback. The hub wrote exactly one block per tick, at exactly
//!   the consumer's rate, so the pipe only ever held its start-up backlog, and every block the hub
//!   did not write drained it for good. Below one quantum every cycle blocked and xrunned (~50/s on
//!   the strih program sink and on the cutters' MiniFuse cans) until a hub restart. Each pipe is
//!   now measured before every write (`FIONREAD` on the write end) and kept at a fill target.
//! - **One blocking UDP socket sent every cambox's VBAN.** Packets to a cambox that is off wait in
//!   its unresolved ARP neighbour queue, charged to the socket; once the buffer was full, `send_to`
//!   blocked the block loop (16929 missed ticks = blocks lost on every output). Every destination
//!   now gets its own non-blocking socket: a full one drops and counts that one packet.

use std::io::{Read, Write};
use std::path::PathBuf;

use intercom_hub::local_audio::{
    pipe_fill_bytes, pipe_fill_plan, LocalAudioStats, PipeFillPlan, PipeFillWriter,
    PipeWriteReport, PIPE_HIGH_FRAMES, PIPE_TARGET_FRAMES, PW_GRAPH_BURST_FRAMES,
};
use intercom_hub::matrix::Matrix;
use intercom_hub::state::{HubState, RuntimeStats};
use intercom_hub::vban_io::{classify_send, SendOutcome};

/// The hub block (`[hub].block_frames` on strih-lx).
const BLOCK: usize = 256;

fn top_up(silence_frames: usize) -> PipeFillPlan {
    PipeFillPlan::TopUp { silence_frames }
}

#[test]
fn the_pipe_target_is_two_graph_quanta_and_the_trim_mark_two_more() {
    assert_eq!(PIPE_TARGET_FRAMES, 2 * PW_GRAPH_BURST_FRAMES);
    assert_eq!(PIPE_TARGET_FRAMES, 2048, "42.7 ms at 48 kHz");
    assert_eq!(
        PIPE_HIGH_FRAMES,
        PIPE_TARGET_FRAMES + 2 * PW_GRAPH_BURST_FRAMES
    );
}

#[test]
fn the_pipe_fill_plan_tops_up_a_drained_pipe_writes_a_healthy_one_and_drops_above_the_mark() {
    // Empty (a fresh pw-cat, or a pipe a hub stall drained): silence up to the target, then the block.
    assert_eq!(pipe_fill_plan(0, BLOCK), top_up(2048));
    // Below one hub block: the same top-up, by exactly the missing frames.
    assert_eq!(pipe_fill_plan(1, BLOCK), top_up(2047));
    assert_eq!(pipe_fill_plan(BLOCK - 1, BLOCK), top_up(2048 - 255));
    // One block and above is enough for the next graph cycle: write the block as it is.
    assert_eq!(pipe_fill_plan(BLOCK, BLOCK), PipeFillPlan::Write);
    // The healthy steady state rides 1024..2048 and never triggers either guard.
    for fill in (1024..=2048).step_by(BLOCK) {
        assert_eq!(pipe_fill_plan(fill, BLOCK), PipeFillPlan::Write, "{fill}");
    }
    // Up to and including the trim mark the block is still written.
    assert_eq!(pipe_fill_plan(3000, BLOCK), PipeFillPlan::Write);
    assert_eq!(pipe_fill_plan(PIPE_HIGH_FRAMES, BLOCK), PipeFillPlan::Write);
    // Above the target + 2 quanta: drop the block.
    assert_eq!(
        pipe_fill_plan(PIPE_HIGH_FRAMES + 1, BLOCK),
        PipeFillPlan::Drop
    );
    assert_eq!(pipe_fill_plan(16_384, BLOCK), PipeFillPlan::Drop);
    // "One hub block" is the block being written, not a fixed 256.
    assert_eq!(pipe_fill_plan(500, 512), top_up(2048 - 500));
    assert_eq!(pipe_fill_plan(512, 512), PipeFillPlan::Write);
}

#[test]
fn fionread_reports_exactly_the_bytes_in_the_pipe_from_either_end() {
    let (mut reader, mut writer) = std::io::pipe().expect("pipe");
    assert_eq!(pipe_fill_bytes(&writer).unwrap(), 0);
    writer.write_all(&[7u8; 1000]).unwrap();
    assert_eq!(pipe_fill_bytes(&writer).unwrap(), 1000, "the write end");
    assert_eq!(pipe_fill_bytes(&reader).unwrap(), 1000, "the read end");
    let mut taken = [0u8; 400];
    reader.read_exact(&mut taken).unwrap();
    assert_eq!(pipe_fill_bytes(&writer).unwrap(), 600);
}

/// A stereo block whose samples are all non-zero, so the silence in front of it is visible.
fn stereo_block() -> Vec<i16> {
    (0..BLOCK * 2).map(|i| i as i16 + 1).collect()
}

fn le_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

#[test]
fn a_sink_writer_tops_an_empty_pipe_up_to_the_target_and_then_writes_the_block() {
    let (mut reader, writer) = std::io::pipe().expect("pipe");
    let probe = writer.try_clone().expect("clone the write end");
    let mut sink = PipeFillWriter::new(writer, 2);
    let block = stereo_block();

    // The first write after a spawn primes the pipe to the target, then writes the block.
    let report = sink.write_block(&block).unwrap();
    assert_eq!(
        report,
        PipeWriteReport {
            fill_frames: 0,
            plan: top_up(PIPE_TARGET_FRAMES),
            first: true,
        }
    );
    // Bytes per frame = channels x 2 (s16).
    assert_eq!(
        pipe_fill_bytes(&probe).unwrap(),
        (PIPE_TARGET_FRAMES + BLOCK) * 2 * 2
    );
    let mut silence = vec![0xffu8; PIPE_TARGET_FRAMES * 4];
    reader.read_exact(&mut silence).unwrap();
    assert!(silence.iter().all(|&b| b == 0), "the top-up is silence");
    let mut audio = vec![0u8; BLOCK * 4];
    reader.read_exact(&mut audio).unwrap();
    assert_eq!(audio, le_bytes(&block), "the block follows the silence");

    // pw-cat took everything: the next write is a REFILL (not the first write any more).
    let report = sink.write_block(&block).unwrap();
    assert_eq!(report.fill_frames, 0);
    assert_eq!(report.plan, top_up(PIPE_TARGET_FRAMES));
    assert!(!report.first);

    // A healthy pipe takes the block as it is.
    let report = sink.write_block(&block).unwrap();
    assert_eq!(report.fill_frames, PIPE_TARGET_FRAMES + BLOCK);
    assert_eq!(report.plan, PipeFillPlan::Write);
    assert_eq!(
        pipe_fill_bytes(&probe).unwrap(),
        (PIPE_TARGET_FRAMES + 2 * BLOCK) * 4
    );

    // Above the trim mark the block is dropped and nothing is written.
    let extra = PIPE_HIGH_FRAMES * 4;
    (&probe).write_all(&vec![0u8; extra]).unwrap();
    let before = pipe_fill_bytes(&probe).unwrap();
    let report = sink.write_block(&block).unwrap();
    assert_eq!(report.fill_frames, before / 4);
    assert_eq!(report.plan, PipeFillPlan::Drop);
    assert_eq!(pipe_fill_bytes(&probe).unwrap(), before, "a dropped block");
}

#[test]
fn a_four_channel_sink_measures_its_fill_in_four_channel_frames() {
    // The cutters' MiniFuse cans: 4 channels, 8 bytes per frame.
    let (_reader, writer) = std::io::pipe().expect("pipe");
    let probe = writer.try_clone().expect("clone the write end");
    let mut sink = PipeFillWriter::new(writer, 4);
    let block: Vec<i16> = vec![1; BLOCK * 4];
    let report = sink.write_block(&block).unwrap();
    assert_eq!(report.plan, top_up(PIPE_TARGET_FRAMES));
    assert_eq!(
        pipe_fill_bytes(&probe).unwrap(),
        (PIPE_TARGET_FRAMES + BLOCK) * 8
    );
    let report = sink.write_block(&block).unwrap();
    assert_eq!(report.fill_frames, PIPE_TARGET_FRAMES + BLOCK);
    assert_eq!(report.plan, PipeFillPlan::Write);
}

#[test]
fn the_local_audio_facet_counts_refills_trims_and_the_last_fill_but_not_the_prime() {
    let stats = LocalAudioStats::default();
    // The prime after a spawn is not a refill: `pipe_refills` stays 0 in a healthy run.
    stats.record_write(&PipeWriteReport {
        fill_frames: 0,
        plan: top_up(2048),
        first: true,
    });
    let f = stats.snapshot();
    assert_eq!(
        (f.tx_blocks, f.pipe_refills, f.pipe_refill_frames),
        (1, 0, 0)
    );
    // A drained pipe under a running pw-cat is a refill.
    stats.record_write(&PipeWriteReport {
        fill_frames: 100,
        plan: top_up(1948),
        first: false,
    });
    stats.record_write(&PipeWriteReport {
        fill_frames: 1500,
        plan: PipeFillPlan::Write,
        first: false,
    });
    stats.record_write(&PipeWriteReport {
        fill_frames: 5000,
        plan: PipeFillPlan::Drop,
        first: false,
    });
    let f = stats.snapshot();
    assert_eq!(f.tx_blocks, 3, "a dropped block is not written");
    assert_eq!(f.pipe_refills, 1);
    assert_eq!(f.pipe_refill_frames, 1948);
    assert_eq!(f.pipe_trims, 1);
    assert_eq!(f.pipe_fill_frames, 5000, "the last measured fill");
    let v = serde_json::to_value(f).unwrap();
    for key in [
        "pipe_refills",
        "pipe_refill_frames",
        "pipe_trims",
        "pipe_fill_frames",
    ] {
        assert!(v.get(key).is_some(), "{key} in {v}");
    }
}

#[test]
fn a_full_send_buffer_is_a_dropped_packet_and_any_other_error_stays_an_error() {
    assert_eq!(classify_send(Ok(1072)).unwrap(), SendOutcome::Sent);
    let full = std::io::Error::from(std::io::ErrorKind::WouldBlock);
    assert_eq!(classify_send(Err(full)).unwrap(), SendOutcome::Dropped);
    let other = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    let err = classify_send(Err(other)).expect_err("not a drop");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
}

fn two_camboxes() -> Matrix {
    Matrix::from_toml(
        r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[[participant]]
name = "cam1"
role = "cambox"
adapter = "vban"
host = "cam1.lan"
in_stream = "cam1"
out_stream = "cam1"
in_channels = 2
out_channels = 2

[[participant]]
name = "cam2"
role = "cambox"
adapter = "vban"
host = "cam2.lan"
in_stream = "cam2"
out_stream = "cam2"
in_channels = 2
out_channels = 2
"#,
    )
    .unwrap()
}

#[test]
fn dropped_vban_packets_show_per_participant_and_on_the_status_line_only_when_there_are_any() {
    let quiet = HubState::snapshot(
        &two_camboxes(),
        "v",
        &[RuntimeStats::default(), RuntimeStats::default()],
    );
    let line = quiet.status_line();
    assert!(!line.contains("tx_dropped"), "got: {line}");
    assert!(
        line.starts_with("intercom-hub: status participants=2 underruns=0 overruns=0"),
        "the leading shape a parser reads: {line}"
    );

    // cam2 is off: its own socket fills and its packets are dropped; cam1 sends as before.
    let stats = vec![
        RuntimeStats {
            tx_packets: 187,
            ..Default::default()
        },
        RuntimeStats {
            tx_dropped: 160,
            tx_packets: 27,
            ..Default::default()
        },
    ];
    let hs = HubState::snapshot(&two_camboxes(), "v", &stats);
    let line = hs.status_line();
    assert!(line.contains("tx_dropped=160(cam2)"), "got: {line}");
    assert!(
        line.starts_with("intercom-hub: status participants=2 underruns=0 overruns=0"),
        "got: {line}"
    );
    let v = serde_json::to_value(&hs).unwrap();
    assert_eq!(v["participants"][0]["tx_dropped"], 0);
    assert_eq!(v["participants"][1]["tx_dropped"], 160);
    assert_eq!(v["participants"][1]["tx_packets"], 27);
}

#[test]
fn the_block_loop_sends_every_output_through_its_own_sender_and_counts_the_drops() {
    // The daemon's tokio loop is not unit-testable; anchor the wiring of the pieces tested above.
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let src = std::fs::read_to_string(&p).expect("read main.rs");
    assert_eq!(
        src.matches("VbanSender::bind_ephemeral()").count(),
        1,
        "one bind site"
    );
    let senders = src
        .find("let senders: Vec<VbanSender> = outputs")
        .expect("one sender per output slot");
    let bind = src.find("VbanSender::bind_ephemeral()").unwrap();
    assert!(
        bind > senders && bind - senders < 200,
        "the bind is inside the per-output map"
    );
    assert!(
        src.contains("outputs.iter().zip(&senders).enumerate()"),
        "each slot sends through its own socket"
    );
    assert!(src.contains("sender.send_block(addr, &block)"));
    assert!(src.contains("Ok(SendOutcome::Dropped) => tx_dropped[*id]"));
    assert!(
        src.contains("s.tx_dropped = tx_dropped[id]"),
        "the drops reach /api/state"
    );
}
