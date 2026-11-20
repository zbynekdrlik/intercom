//! The hub-side TURN relay against a REAL TURN server (intercom#42 phase 3): coturn on loopback
//! (`coturn` module), the REAL network task + phones-room, and a str0m phone that may reach the
//! hub ONLY through the hub's relayed candidate (the answer's host candidates are removed before
//! the phone sees them, and coturn refuses 127.0.0.1 as a peer). The tone goes both ways, the
//! `/api/state` pair reads `relay`, every datagram the phone got came from the TURN server, and
//! the allocation refreshes and rotates its credential against coturn.

mod coturn;

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coturn::Coturn;
use intercom_hub::dante_guard::DanteGuard;
use intercom_hub::janus_pacing::{PacedRing, FRAME_48K, RING_CAP_FRAMES, RING_TARGET_FRAMES};
use intercom_hub::vban_io::JitterBuffer;
use intercom_hub::webrtc_http::{phone_router, PhoneApi};
use intercom_hub::webrtc_leg::{
    start_webrtc_turn, TurnFeed, WebrtcHandle, WebrtcLegConfig, WebrtcShared,
};
use intercom_hub::webrtc_room::{spawn_phones_room, PhonesRoom, RoomIo, RoomSlots};
use intercom_hub::webrtc_turn::{TurnCreds, TurnOptions};
use str0m::change::SdpAnswer;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};

const RATE: f64 = 48_000.0;
const BUS_HZ: f64 = 997.0;
const PHONE_HZ: f64 = 1559.0;

fn goertzel_db(x: &[i16], freq: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq / RATE;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    let len = x.len() as f64;
    for (i, &v) in x.iter().enumerate() {
        let hann = 1.0 - (2.0 * std::f64::consts::PI * i as f64 / len).cos();
        let s0 = f64::from(v) * hann + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    let amp = 2.0 * power.max(0.0).sqrt() / len;
    20.0 * (amp / 32768.0).max(1e-12).log10()
}

fn tone(freq: f64, amp: f64, start: usize, n: usize) -> Vec<i16> {
    (start..start + n)
        .map(|i| (amp * (2.0 * std::f64::consts::PI * freq * i as f64 / RATE).sin()).round() as i16)
        .collect()
}

struct Hub {
    base: String,
    jitter: Arc<Mutex<Vec<JitterBuffer>>>,
    shared: Arc<WebrtcShared>,
    handle: WebrtcHandle,
    host: SocketAddr,
}

async fn start_hub(opts: TurnOptions) -> Hub {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let host = socket.local_addr().unwrap();
    let shared = Arc::new(WebrtcShared::new(
        host.to_string(),
        vec![host.to_string()],
        4,
    ));
    let slots = Arc::new(RoomSlots::new(4));
    let ring = Arc::new(Mutex::new(PacedRing::new(
        RING_TARGET_FRAMES,
        RING_CAP_FRAMES,
    )));
    let jitter = Arc::new(Mutex::new(vec![
        JitterBuffer::new(4096),
        JitterBuffer::janus_capture(8192, 2048, 48_000).with_min_channels(2),
    ]));
    let (handle, packets) = start_webrtc_turn(
        WebrtcLegConfig {
            socket,
            host_ips: vec!["127.0.0.1".parse().unwrap()],
            max_phones: 4,
            slots: slots.clone(),
            shared: shared.clone(),
            dante_guard: DanteGuard::new(None, vec![]),
        },
        TurnFeed::Manual,
        opts,
    )
    .unwrap();
    let room = PhonesRoom::new(RoomIo {
        bus: ring.clone(),
        slots,
        jitter: jitter.clone(),
        phones_id: 1,
        sink: packets,
        stats: shared.clone(),
    })
    .unwrap();
    spawn_phones_room(room).unwrap();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        for k in 0..usize::MAX {
            let due = t0 + Duration::from_millis(20 * k as u64);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            let block = tone(BUS_HZ, 8000.0, k * FRAME_48K, FRAME_48K);
            ring.lock().unwrap().push_at(&block, Instant::now());
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let api_handle = handle.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            phone_router(PhoneApi::new(Some(api_handle)))
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Hub {
        base: format!("http://{addr}"),
        jitter,
        shared,
        handle,
        host,
    }
}

/// The production bounds with the test-only permission for loopback peers (coturn and its peers
/// live on 127.0.0.x here; a production hub never asks a permission for a non-global address).
fn loopback_opts() -> TurnOptions {
    TurnOptions {
        allow_private_peers: true,
        ..TurnOptions::for_phones(4)
    }
}

fn creds(turn: &Coturn, user: &str, pass: &str) -> TurnCreds {
    TurnCreds {
        server: turn.addr,
        username: user.into(),
        password: pass.into(),
        expires_at: Instant::now() + Duration::from_secs(86_400),
    }
}

/// Wait (up to 10 s) until the facet's TURN state reads `allocated` with `allocations` >= `n`.
async fn wait_allocated(hub: &Hub, n: u64) -> intercom_hub::webrtc_turn::TurnFacet {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let f = hub.shared.snapshot().turn.expect("the turn facet");
        if f.state == "allocated" && f.allocations >= n && f.relayed.is_some() {
            return f;
        }
        assert!(Instant::now() < deadline, "never allocated: {f:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct PhoneRun {
    connected: bool,
    received: Vec<i16>,
    sources: Vec<SocketAddr>,
    answer: String,
}

/// A str0m phone that sees ONLY the hub's relayed candidate: tone out, tone in. It sits on
/// coturn's own IP (127.0.0.2), as a phone's Cloudflare relay shares the anycast IP of the hub's:
/// an offer address with the relay's IP is still a peer that needs its permission.
fn run_phone(base: String, run_for: Duration) -> (String, PhoneRun) {
    let socket = UdpSocket::bind(format!("{}:0", coturn::LISTEN_IP)).unwrap();
    let local = socket.local_addr().unwrap();
    let mut rtc: Rtc = RtcConfig::new()
        .clear_codecs()
        .enable_opus(true, false)
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let mut api = rtc.sdp_api();
    let mid = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, pending) = api.apply().unwrap();
    let body = serde_json::json!({ "sdp": offer.to_sdp_string(), "display": "Relay" });
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (status, text) = rt.block_on(async {
        let r = reqwest::Client::new()
            .post(format!("{base}/api/phone/offer"))
            .json(&body)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    });
    assert_eq!(status, 200, "offer answered: {text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    let full = v["sdp"].as_str().unwrap().to_string();
    // The phone never learns the hub's host candidate: the relay is its only way in.
    let relay_only: String = full
        .split_inclusive('\n')
        .filter(|l| !(l.starts_with("a=candidate:") && l.contains(" typ host")))
        .collect();
    let answer = SdpAnswer::from_sdp_string(&relay_only).unwrap();
    rtc.sdp_api().accept_answer(pending, answer).unwrap();

    let mut enc =
        opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
    let mut dec = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
    let mut run = PhoneRun {
        connected: false,
        received: Vec::new(),
        sources: Vec::new(),
        answer: full,
    };
    let mut buf = vec![0u8; 2048];
    let mut frame = vec![0i16; 5760];
    let t0 = Instant::now();
    let mut sent = 0usize;
    let mut send_start: Option<Instant> = None;
    while t0.elapsed() < run_for {
        if let Some(start) = send_start {
            while start + Duration::from_millis(20 * sent as u64) <= Instant::now() {
                let pcm = tone(PHONE_HZ, 8000.0, sent * FRAME_48K, FRAME_48K);
                let mut pkt = vec![0u8; 1275];
                let n = enc.encode(&pcm, &mut pkt).unwrap();
                pkt.truncate(n);
                let writer = rtc.writer(mid).unwrap();
                let pt = writer.payload_params().next().map(|p| p.pt()).unwrap();
                let t = MediaTime::new((sent * FRAME_48K) as u64, Frequency::FORTY_EIGHT_KHZ);
                writer.write(pt, Instant::now(), t, pkt).unwrap();
                sent += 1;
            }
        }
        let timeout = loop {
            match rtc.poll_output().unwrap() {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Output::Event(Event::Connected) => {
                    run.connected = true;
                    send_start = Some(Instant::now());
                }
                Output::Event(Event::MediaData(d)) => {
                    let n = dec.decode(&d.data, &mut frame, false).unwrap();
                    run.received.extend_from_slice(&frame[..n]);
                }
                Output::Event(_) => {}
            }
        };
        let wait = timeout
            .saturating_duration_since(Instant::now())
            .clamp(Duration::from_millis(1), Duration::from_millis(5));
        socket.set_read_timeout(Some(wait)).unwrap();
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                if !run.sources.contains(&from) {
                    run.sources.push(from);
                }
                let input = Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source: from,
                        destination: local,
                        contents: buf[..n].try_into().unwrap(),
                    },
                );
                rtc.handle_input(input).unwrap();
            }
            Err(_) => rtc.handle_input(Input::Timeout(Instant::now())).unwrap(),
        }
    }
    (id, run)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_phone_reaches_the_hub_only_through_the_hubs_relay_and_the_tone_round_trips() {
    let turn = Coturn::start(&[("hub", "hub-pw")]);
    let hub = start_hub(loopback_opts()).await;
    hub.handle
        .set_turn_credentials(creds(&turn, "hub", "hub-pw"))
        .await
        .unwrap();
    let f = wait_allocated(&hub, 1).await;
    let relayed: SocketAddr = f.relayed.unwrap().parse().unwrap();
    assert_eq!(relayed.ip().to_string(), coturn::LISTEN_IP);
    assert_eq!(f.server.as_deref(), Some(turn.addr.to_string().as_str()));

    let base = hub.base.clone();
    let (id, run) = tokio::task::spawn_blocking(move || run_phone(base, Duration::from_secs(6)))
        .await
        .unwrap();
    // The answer carries the relayed candidate next to the host one.
    assert!(
        run.answer
            .contains(&format!("{} {} typ relay", relayed.ip(), relayed.port())),
        "{}",
        run.answer
    );
    assert!(run.answer.contains(" typ host"), "{}", run.answer);
    assert!(run.connected, "ICE + DTLS through the relay");
    // Every datagram the phone got came from the hub's relayed address, never the hub itself.
    assert_eq!(run.sources, vec![relayed], "sources: {:?}", run.sources);
    assert!(!run.sources.contains(&hub.host));

    // The bus tone reached the phone through the relay.
    assert!(
        run.received.len() > 48_000,
        "{} samples",
        run.received.len()
    );
    let tail = &run.received[run.received.len() - 24_000..];
    assert!(goertzel_db(tail, BUS_HZ) > -30.0, "bus tone at the phone");
    assert!(
        goertzel_db(tail, PHONE_HZ) < -50.0,
        "the phone never hears itself"
    );
    // The phone's tone reached the hub's phones input.
    let heard = {
        let mut jb = hub.jitter.lock().unwrap();
        assert!(jb[1].rx_packets > 50);
        let mut heard = Vec::new();
        for _ in 0..7 {
            heard.extend_from_slice(&jb[1].pop_block(256)[0]);
        }
        heard
    };
    assert!(
        goertzel_db(&heard, PHONE_HZ) > -30.0,
        "the hub hears the phone"
    );
    let state = hub.shared.snapshot();
    let phone = state
        .phones
        .iter()
        .find(|p| id.starts_with(&p.id))
        .expect("the phone on /api/state");
    assert_eq!(phone.pair, Some("relay"), "{phone:?}");
    assert!(phone.rx_packets > 50 && phone.tx_packets > 50, "{phone:?}");
    let t = state.turn.unwrap();
    assert!(t.permissions >= 1, "{t:?}");
    assert!(
        t.channels >= 1,
        "the established phone got a channel: {t:?}"
    );
    assert!(t.rx_packets > 50 && t.tx_packets > 50, "{t:?}");
}

/// Only the network task, on `udp_bind`, with `creds` handed over; returns its facet source.
async fn start_leg_on(bind: SocketAddr, creds: TurnCreds) -> Arc<WebrtcShared> {
    let socket = UdpSocket::bind(bind).unwrap();
    let shared = Arc::new(WebrtcShared::new(
        bind.to_string(),
        vec![bind.to_string()],
        4,
    ));
    let (handle, _packets) = start_webrtc_turn(
        WebrtcLegConfig {
            socket,
            host_ips: vec![bind.ip()],
            max_phones: 4,
            slots: Arc::new(RoomSlots::new(4)),
            shared: shared.clone(),
            dante_guard: DanteGuard::new(None, vec![]),
        },
        TurnFeed::Manual,
        loopback_opts(),
    )
    .unwrap();
    handle.set_turn_credentials(creds).await.unwrap();
    shared
}

/// Poll `shared` until allocated (true) or `within` passed (false).
async fn allocated_within(shared: &WebrtcShared, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(f) = shared.snapshot().turn {
            if f.state == "allocated" && f.relayed.is_some() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// A hub restart (a crash, a deploy: the old process never released its allocation, which coturn
/// keeps for its whole lifetime) on the SAME `[webrtc].udp_bind`: the new hub's relay must be up
/// within seconds. The TURN traffic leaves from its own ephemeral socket, so every process is a new
/// 5-tuple; on the phones' fixed socket the new Allocate got 437 until the old one expired (~10 min).
#[test]
fn a_restarted_hub_allocates_at_once_while_the_old_allocation_still_lives() {
    let turn = Coturn::start(&[("hub", "hub-pw")]);
    let bind: SocketAddr = format!("127.0.0.1:{}", {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    })
    .parse()
    .unwrap();
    // The first hub allocates, then dies without a word (its runtime and socket are dropped).
    let first = tokio::runtime::Runtime::new().unwrap();
    first.block_on(async {
        let shared = start_leg_on(bind, creds(&turn, "hub", "hub-pw")).await;
        assert!(
            allocated_within(&shared, Duration::from_secs(10)).await,
            "the first hub allocated"
        );
    });
    drop(first);
    // The second hub, same udp_bind, same credential: allocated within a few seconds.
    let second = tokio::runtime::Runtime::new().unwrap();
    second.block_on(async {
        let shared = start_leg_on(bind, creds(&turn, "hub", "hub-pw")).await;
        let up = allocated_within(&shared, Duration::from_secs(4)).await;
        let f = shared.snapshot().turn.unwrap();
        assert!(up, "the restarted hub's relay is up at once: {f:?}");
        assert_eq!(f.failures, 0, "no 437 from the old allocation: {f:?}");
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_allocation_refreshes_and_rotates_its_credential_against_coturn() {
    let turn = Coturn::start(&[("hub-a", "pw-a"), ("hub-b", "pw-b")]);
    let mut opts = loopback_opts();
    opts.refresh_cap = Some(Duration::from_secs(1));
    let hub = start_hub(opts).await;
    hub.handle
        .set_turn_credentials(creds(&turn, "hub-a", "pw-a"))
        .await
        .unwrap();
    wait_allocated(&hub, 1).await;
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let f = hub.shared.snapshot().turn.unwrap();
    assert!(f.refreshes >= 2, "coturn accepted the refreshes: {f:?}");
    assert_eq!(f.failures, 0, "{f:?}");
    // A new credential with no phone on the relay: released and allocated again under hub-b.
    hub.handle
        .set_turn_credentials(creds(&turn, "hub-b", "pw-b"))
        .await
        .unwrap();
    let f = wait_allocated(&hub, 2).await;
    assert_eq!(
        f.failures, 0,
        "the release came before the new Allocate (no 437): {f:?}"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let g = hub.shared.snapshot().turn.unwrap();
    assert_eq!(g.state, "allocated");
    assert!(
        g.refreshes > f.refreshes,
        "hub-b's refreshes are accepted: {g:?}"
    );
    assert_eq!(g.failures, 0, "{g:?}");
}
