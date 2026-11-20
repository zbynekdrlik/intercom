//! The in-hub WebRTC phone leg end to end over localhost (intercom#42): a str0m CLIENT (a second,
//! independent ICE + DTLS + SRTP stack in this test) posts its offer to the REAL signalling routes,
//! connects to the REAL network task and phones-room thread, and a tone goes both ways: the phones
//! bus (fed here the way the hub-mix thread feeds it) reaches the client, and the client's Opus
//! reaches the hub's `phones` input. The signalling limits are checked on the same server: the
//! slot cap (503), leave (204, the slot is free again), an unknown id (DELETE 204, rename 404), a rename on the facet.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use intercom_hub::dante_guard::DanteGuard;
use intercom_hub::janus_pacing::{PacedRing, FRAME_48K, RING_CAP_FRAMES, RING_TARGET_FRAMES};
use intercom_hub::vban_io::JitterBuffer;
use intercom_hub::webrtc_http::{phone_router, PhoneApi};
use intercom_hub::webrtc_leg::{start_webrtc, WebrtcLegConfig, WebrtcShared};
use intercom_hub::webrtc_room::{spawn_phones_room, PhonesRoom, RoomIo, RoomSlots};
use intercom_hub::webrtc_signal::RateLimiter;
use str0m::change::SdpAnswer;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};

const RATE: f64 = 48_000.0;
const BUS_HZ: f64 = 997.0;
const CLIENT_HZ: f64 = 1559.0;

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
}

/// The leg as `main.rs` starts it, minus the block loop: a thread pushes a bus tone into the ring
/// every 20 ms in its place.
async fn start_hub(max_phones: usize) -> Hub {
    start_hub_rated(max_phones, None).await
}

/// [`start_hub`] with the routes' own rate limiter (`None` = the production default).
async fn start_hub_rated(max_phones: usize, rate: Option<RateLimiter>) -> Hub {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    let shared = Arc::new(WebrtcShared::new(
        format!("127.0.0.1:{port}"),
        vec![format!("127.0.0.1:{port}")],
        max_phones,
    ));
    let slots = Arc::new(RoomSlots::new(max_phones));
    let ring = Arc::new(Mutex::new(PacedRing::new(
        RING_TARGET_FRAMES,
        RING_CAP_FRAMES,
    )));
    let jitter = Arc::new(Mutex::new(vec![
        JitterBuffer::new(4096),
        JitterBuffer::janus_capture(8192, 2048, 48_000).with_min_channels(2),
    ]));
    let (handle, packets) = start_webrtc(WebrtcLegConfig {
        socket,
        host_ips: vec!["127.0.0.1".parse().unwrap()],
        max_phones,
        slots: slots.clone(),
        shared: shared.clone(),
        dante_guard: DanteGuard::new(None, vec![]),
    })
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
    tokio::spawn(async move {
        let api = match rate {
            Some(r) => PhoneApi::with_rate(Some(handle), r),
            None => PhoneApi::new(Some(handle)),
        };
        axum::serve(listener, phone_router(api)).await.unwrap();
    });
    Hub {
        base: format!("http://{addr}"),
        jitter,
        shared,
    }
}

struct ClientRun {
    connected: bool,
    received: Vec<i16>,
}

/// A str0m phone: offer (sendrecv audio), POST it, apply the answer, then send a tone and collect
/// what comes back for `run_for`.
fn run_client(
    base: String,
    display: &str,
    run_for: Duration,
    stop: Arc<AtomicBool>,
) -> (String, ClientRun) {
    run_client_with(ClientOpts {
        base,
        display: display.to_string(),
        replaces: None,
        extra_candidates: Vec::new(),
        run_for,
        stop,
        id_out: Arc::new(Mutex::new(None)),
    })
}

/// How a test phone offers.
struct ClientOpts {
    base: String,
    display: String,
    /// The id of this phone's previous session (a reconnect).
    replaces: Option<String>,
    /// Extra `a=candidate:` lines added to the offer (addresses that are not the phone's).
    extra_candidates: Vec<String>,
    run_for: Duration,
    stop: Arc<AtomicBool>,
    /// The phone's id, as soon as the hub answered.
    id_out: Arc<Mutex<Option<String>>>,
}

fn run_client_with(o: ClientOpts) -> (String, ClientRun) {
    let ClientOpts {
        base,
        display,
        replaces,
        extra_candidates,
        run_for,
        stop,
        id_out,
    } = o;
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let local = socket.local_addr().unwrap();
    let mut rtc: Rtc = RtcConfig::new()
        .clear_codecs()
        .enable_opus(true, false)
        .build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let mut api = rtc.sdp_api();
    let mid = api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, pending) = api.apply().unwrap();
    let mut sdp = offer.to_sdp_string();
    if !extra_candidates.is_empty() {
        let at = sdp.find("a=ice-pwd:").expect("an ice-pwd line");
        let eol = at + sdp[at..].find("\r\n").expect("CRLF") + 2;
        let lines: String = extra_candidates
            .iter()
            .map(|c| format!("{c}\r\n"))
            .collect();
        sdp.insert_str(eol, &lines);
    }
    let mut body = serde_json::json!({ "sdp": sdp, "display": display });
    if let Some(r) = replaces {
        body["replaces"] = serde_json::json!(r);
    }
    let resp = reqwest::blocking_post(&format!("{base}/api/phone/offer"), &body);
    assert_eq!(resp.0, 200, "offer answered: {}", resp.1);
    let v: serde_json::Value = serde_json::from_str(&resp.1).unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    *id_out.lock().unwrap() = Some(id.clone());
    let answer = SdpAnswer::from_sdp_string(v["sdp"].as_str().unwrap()).unwrap();
    rtc.sdp_api().accept_answer(pending, answer).unwrap();

    let mut enc =
        opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
    let mut dec = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
    let mut run = ClientRun {
        connected: false,
        received: Vec::new(),
    };
    let mut buf = vec![0u8; 2048];
    let mut frame = vec![0i16; 5760];
    let t0 = Instant::now();
    let mut sent_frames = 0usize;
    let mut send_start: Option<Instant> = None;
    while t0.elapsed() < run_for && !stop.load(Ordering::Relaxed) {
        // Send a 20 ms tone frame whenever one is due, once connected.
        if let Some(start) = send_start {
            while start + Duration::from_millis(20 * sent_frames as u64) <= Instant::now() {
                let pcm = tone(CLIENT_HZ, 8000.0, sent_frames * FRAME_48K, FRAME_48K);
                let mut pkt = vec![0u8; 1275];
                let n = enc.encode(&pcm, &mut pkt).unwrap();
                pkt.truncate(n);
                let writer = rtc.writer(mid).unwrap();
                let pt = writer.payload_params().next().map(|p| p.pt()).unwrap();
                let t =
                    MediaTime::new((sent_frames * FRAME_48K) as u64, Frequency::FORTY_EIGHT_KHZ);
                writer.write(pt, Instant::now(), t, pkt).unwrap();
                sent_frames += 1;
            }
        }
        let timeout = loop {
            match rtc.poll_output().unwrap() {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    socket.send_to(&t.contents, t.destination).unwrap();
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

/// A tiny blocking HTTP client over reqwest's async one (the hub's own dependency).
mod reqwest {
    pub fn blocking_post(url: &str, body: &serde_json::Value) -> (u16, String) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let r = ::reqwest::Client::new()
                .post(url)
                .json(body)
                .send()
                .await
                .unwrap();
            let s = r.status().as_u16();
            (s, r.text().await.unwrap())
        })
    }
}

async fn post(url: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let r = ::reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .unwrap();
    let s = r.status().as_u16();
    let text = r.text().await.unwrap();
    (
        s,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_str0m_phone_hears_the_bus_and_the_hub_hears_the_phone() {
    let hub = start_hub(2).await;
    let base = hub.base.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let client = tokio::task::spawn_blocking({
        let stop = stop.clone();
        move || run_client(base, "Kamera 1", Duration::from_secs(4), stop)
    });
    // While the client runs: the hub's phones input fills with the client's tone.
    tokio::time::sleep(Duration::from_millis(3000)).await;
    let facet = hub.shared.snapshot();
    let (id, run) = client.await.unwrap();
    assert!(run.connected, "ICE + DTLS came up");
    assert_eq!(facet.phones.len(), 1, "{facet:?}");
    let p = &facet.phones[0];
    assert_eq!(p.display, "Kamera 1");
    assert!(p.connected);
    assert_eq!(p.id, id[..8], "only the id's first 8 characters are shown");
    assert!(p.rx_packets > 50, "{p:?}");
    assert!(p.tx_packets > 50, "{p:?}");
    assert!(p.mic_active, "{p:?}");
    assert!(matches!(p.ice_state, "connected" | "completed"), "{p:?}");

    // The bus tone reached the phone (grade the last second, after priming).
    assert!(
        run.received.len() > 2 * 48_000,
        "received {} samples",
        run.received.len()
    );
    let tail = &run.received[run.received.len() - 48_000..];
    assert!(goertzel_db(tail, BUS_HZ) > -30.0, "the phone hears the bus");
    assert!(
        goertzel_db(tail, CLIENT_HZ) < -50.0,
        "the phone never hears itself"
    );

    // The phone's tone reached the hub's phones participant.
    let mut jb = hub.jitter.lock().unwrap();
    assert!(jb[1].rx_packets > 50);
    let mut heard = Vec::new();
    for _ in 0..7 {
        heard.extend_from_slice(&jb[1].pop_block(256)[0]);
    }
    assert!(
        goertzel_db(&heard, CLIENT_HZ) > -30.0,
        "the hub hears the phone"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_signalling_routes_keep_their_limits() {
    let hub = start_hub(1).await;
    let base = hub.base.clone();
    let r = ::reqwest::get(format!("{base}/api/phone")).await.unwrap();
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["transport"],
        "webrtc"
    );

    // Phone A takes the only slot (it keeps running in the background).
    let stop = Arc::new(AtomicBool::new(false));
    let client = tokio::task::spawn_blocking({
        let base = base.clone();
        let stop = stop.clone();
        move || run_client(base, "A", Duration::from_secs(20), stop)
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while hub.shared.snapshot().phones.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let short = hub.shared.snapshot().phones[0].id.clone();

    // A second phone: every slot is taken -> 503 (str0m would parse the offer; the cap comes first
    // for a parseable one, so send a real-looking offer).
    let mut probe = Rtc::new(Instant::now());
    let mut api = probe.sdp_api();
    api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, _pending) = api.apply().unwrap();
    let (s, v) = post(
        &format!("{base}/api/phone/offer"),
        serde_json::json!({"sdp": offer.to_sdp_string(), "display": "B"}),
    )
    .await;
    assert_eq!(s, 503, "{v}");
    assert!(v["error"].as_str().unwrap().contains("slot"));
    // Too large: 413 before anything else.
    let (s, _) = post(
        &format!("{base}/api/phone/offer"),
        serde_json::json!({"sdp": format!("v=0{}", "x".repeat(16 * 1024))}),
    )
    .await;
    assert_eq!(s, 413);
    // Not an SDP, while every slot is taken: the capacity is checked before the SDP is parsed
    // (the parse and the certificate are the expensive part), so 503 (400 with a free slot:
    // offers_are_json_only_and_rate_limited_per_client).
    let (s, _) = post(
        &format!("{base}/api/phone/offer"),
        serde_json::json!({"sdp": "v=0 nonsense"}),
    )
    .await;
    assert_eq!(s, 503);
    assert!(hub.shared.snapshot().offers_refused >= 3);

    // Leaving with an id the hub does not have (any more) is not an error: 204, so a page never
    // logs a 404 for a phone the hub already dropped; a rename of one is 404.
    let c = ::reqwest::Client::new();
    let r = c
        .delete(format!("{base}/api/phone/{}", "ab".repeat(16)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 204);
    let r = c
        .delete(format!("{base}/api/phone/not-an-id"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 204);

    // Stop the client and read its full id from the run, then rename and leave with it.
    stop.store(true, Ordering::Relaxed);
    let (id, _) = client.await.unwrap();
    assert!(id.starts_with(&short));
    let (s, _) = post(
        &format!("{base}/api/phone/{id}/display"),
        serde_json::json!({"display": "  Réžia  "}),
    )
    .await;
    assert_eq!(s, 204);
    assert_eq!(hub.shared.snapshot().phones[0].display, "Réžia");
    let r = c
        .delete(format!("{base}/api/phone/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 204);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !hub.shared.snapshot().phones.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let f = hub.shared.snapshot();
    assert!(f.phones.is_empty(), "the slot is freed on leave: {f:?}");
    assert_eq!(f.sessions_started, 1);
    assert_eq!(f.sessions_ended, 1);
    // ... so the next phone gets it.
    let mut again = Rtc::new(Instant::now());
    let mut api = again.sdp_api();
    api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, _p) = api.apply().unwrap();
    let (s, v) = post(
        &format!("{base}/api/phone/offer"),
        serde_json::json!({"sdp": offer.to_sdp_string()}),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["id"].as_str().unwrap().len(), 32);
    assert!(v["sdp"].as_str().unwrap().contains("a=candidate"));
}

/// A parseable offer from a phone that will never connect (it never sends a STUN check).
fn silent_offer() -> String {
    let mut probe = Rtc::new(Instant::now());
    let mut api = probe.sdp_api();
    api.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
    let (offer, _pending) = api.apply().unwrap();
    offer.to_sdp_string()
}

async fn post_as(base: &str, client_ip: &str, body: serde_json::Value) -> u16 {
    ::reqwest::Client::new()
        .post(format!("{base}/api/phone/offer"))
        .header("cf-connecting-ip", client_ip)
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// intercom#42 review 🔴1: the public offer route, reachable from the internet, must not let
// anonymous offers lock the real phones out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_offer_flood_never_locks_out_a_reconnecting_or_a_new_phone() {
    let hub = start_hub(4).await; // pending cap 2
    let base = hub.base.clone();

    // Phone A connects and stays.
    let stop_a = Arc::new(AtomicBool::new(false));
    let a_id = Arc::new(Mutex::new(None));
    let a = tokio::task::spawn_blocking({
        let (base, stop, id_out) = (base.clone(), stop_a.clone(), a_id.clone());
        move || {
            run_client_with(ClientOpts {
                base,
                display: "A".into(),
                replaces: None,
                extra_candidates: Vec::new(),
                run_for: Duration::from_secs(30),
                stop,
                id_out,
            })
        }
    });
    wait_for("A connected", 5, || {
        hub.shared.snapshot().phones.iter().any(|p| p.connected)
    })
    .await;
    let a_old = a_id.lock().unwrap().clone().unwrap();

    // The flood: offers that never connect, each from its own address (the rate limit is per
    // client). Only the pending cap (half the slots) is ever held by them.
    let mut codes = Vec::new();
    for i in 0..10 {
        let ip = format!("198.51.100.{i}");
        codes.push(post_as(&base, &ip, serde_json::json!({"sdp": silent_offer()})).await);
    }
    assert_eq!(codes.iter().filter(|c| **c == 200).count(), 2, "{codes:?}");
    assert_eq!(codes.iter().filter(|c| **c == 503).count(), 8, "{codes:?}");
    assert_eq!(hub.shared.snapshot().phones_pending, 2);

    // A reconnects during the flood, naming its old id: admitted at once, its old session goes.
    stop_a.store(true, Ordering::Relaxed);
    a.await.unwrap();
    let stop_a2 = Arc::new(AtomicBool::new(false));
    let a2 = tokio::task::spawn_blocking({
        let (base, stop) = (base.clone(), stop_a2.clone());
        let old = a_old.clone();
        move || {
            run_client_with(ClientOpts {
                base,
                display: "A".into(),
                replaces: Some(old),
                extra_candidates: Vec::new(),
                run_for: Duration::from_secs(30),
                stop,
                id_out: Arc::new(Mutex::new(None)),
            })
        }
    });
    wait_for("A reconnected", 5, || {
        let f = hub.shared.snapshot();
        f.phones
            .iter()
            .filter(|p| p.display == "A" && p.connected)
            .count()
            == 1
            && f.phones.iter().filter(|p| p.display == "A").count() == 1
    })
    .await;

    // A brand-new phone B is admitted as soon as the flood's slots expire (no STUN check within
    // 3 s), never 20 s later.
    let t0 = Instant::now();
    wait_for("the flood's pending slots expire", 5, || {
        hub.shared.snapshot().phones_pending == 0
    })
    .await;
    assert!(
        t0.elapsed() < Duration::from_millis(3500),
        "{:?}",
        t0.elapsed()
    );
    let (b_id, b) = tokio::task::spawn_blocking({
        let base = base.clone();
        move || {
            run_client(
                base,
                "B",
                Duration::from_secs(3),
                Arc::new(AtomicBool::new(false)),
            )
        }
    })
    .await
    .unwrap();
    assert!(b.connected, "B connected after the flood");
    assert_eq!(b_id.len(), 32);
    stop_a2.store(true, Ordering::Relaxed);
    a2.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offers_are_json_only_and_rate_limited_per_client() {
    let hub = start_hub_rated(4, Some(RateLimiter::new(2.0, 0.001))).await;
    let base = hub.base.clone();
    // A "simple request" (text/plain) is refused before anything else: 415.
    let r = ::reqwest::Client::new()
        .post(format!("{base}/api/phone/offer"))
        .header("content-type", "text/plain")
        .body(serde_json::json!({"sdp": silent_offer()}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 415);
    // Two offers per client, then 429; another client is not affected.
    let bad = serde_json::json!({"sdp": "v=0 nonsense"});
    assert_eq!(post_as(&base, "203.0.113.1", bad.clone()).await, 400);
    assert_eq!(post_as(&base, "203.0.113.1", bad.clone()).await, 400);
    assert_eq!(post_as(&base, "203.0.113.1", bad.clone()).await, 429);
    assert_eq!(post_as(&base, "203.0.113.2", bad.clone()).await, 400);
    let f = hub.shared.snapshot();
    assert_eq!(f.offers_rate_limited, 1);
    assert_eq!(f.offers_refused, 5, "every refusal is counted: {f:?}");
}

// intercom#42 review 🔴2: an anonymous offer must not turn the hub into a STUN reflector. The
// hub is ICE-lite: it answers the phone's checks and never sends one to an offer's candidate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_offer_naming_a_foreign_address_gets_no_datagram_from_the_hub() {
    let hub = start_hub(2).await;
    let victim = UdpSocket::bind("127.0.0.1:0").unwrap();
    let vport = victim.local_addr().unwrap().port();
    let foreign = vec![
        format!("a=candidate:9 1 udp 2130706431 127.0.0.1 {vport} typ host"),
        format!(
            "a=candidate:10 1 udp 1694498815 127.0.0.1 {vport} typ srflx raddr 0.0.0.0 rport 9"
        ),
    ];
    let (_, run) = tokio::task::spawn_blocking({
        let base = hub.base.clone();
        move || {
            run_client_with(ClientOpts {
                base,
                display: "C".into(),
                replaces: None,
                extra_candidates: foreign,
                run_for: Duration::from_secs(3),
                stop: Arc::new(AtomicBool::new(false)),
                id_out: Arc::new(Mutex::new(None)),
            })
        }
    })
    .await
    .unwrap();
    assert!(
        run.connected,
        "the phone itself still connects (its checks, answered)"
    );
    victim
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut buf = [0u8; 2048];
    let got = victim.recv_from(&mut buf);
    assert!(
        got.is_err(),
        "the hub sent {got:?} to an address the offer only named"
    );
}

#[test]
fn a_phones_rtp_time_follows_the_room_tick_even_across_dropped_packets() {
    use intercom_hub::webrtc_leg::rtp_time_for;
    assert_eq!(rtp_time_for(100, 100).numer(), 0);
    assert_eq!(rtp_time_for(100, 101).numer(), 960);
    // Ticks 102..104 never reached the phone: the next packet is 4 ticks on.
    assert_eq!(rtp_time_for(100, 105).numer(), 5 * 960);
    assert_eq!(rtp_time_for(100, 105).denom(), 48_000);
}

#[test]
fn stun_messages_are_told_apart_from_dtls_and_rtp() {
    use intercom_hub::webrtc_leg::is_stun;
    let mut stun = vec![0x00, 0x01, 0x00, 0x00, 0x21, 0x12, 0xA4, 0x42];
    stun.extend_from_slice(&[0u8; 12]);
    assert!(is_stun(&stun));
    assert!(!is_stun(&stun[..19]), "a STUN header is 20 bytes");
    let mut dtls = stun.clone();
    dtls[0] = 22; // a DTLS handshake record
    assert!(!is_stun(&dtls));
    let mut rtp = stun.clone();
    rtp[0] = 0x80; // RTP version 2
    assert!(!is_stun(&rtp));
    let mut no_cookie = stun.clone();
    no_cookie[4] = 0;
    assert!(!is_stun(&no_cookie));
}
