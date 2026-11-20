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
        // With the peer address, as the hub's own accept loop hands it to the routes.
        axum::serve(
            listener,
            phone_router(api).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
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
        ..Default::default()
    })
}

/// How a test phone offers.
#[derive(Default)]
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
    /// Arrive as a phone through cloudflared -> the box's nginx with this CF-Connecting-IP.
    client_ip: Option<String>,
    /// Retry a 429 / 503 every 300 ms until this long has passed (the page's backoff); none =
    /// the first answer must be 200.
    retry_for: Option<Duration>,
    /// Retry like the page (1, 2, 4, 4 ... s, no jitter: the worst case) instead of every 300 ms.
    pwa_backoff: bool,
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
        client_ip,
        retry_for,
        pwa_backoff,
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
    let url = format!("{base}/api/phone/offer");
    let deadline = Instant::now() + retry_for.unwrap_or_default();
    let mut attempts = 0u32;
    let resp = loop {
        let r = reqwest::blocking_post(&url, &body, client_ip.as_deref());
        if r.0 == 200 || Instant::now() >= deadline || !matches!(r.0, 429 | 503) {
            break r;
        }
        // The page's own backoff (1 -> 2 -> 4 s cap) or a quick 300 ms one.
        std::thread::sleep(if pwa_backoff {
            let d = Duration::from_secs(1u64 << attempts.min(2));
            attempts += 1;
            d
        } else {
            Duration::from_millis(300)
        });
    };
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
    pub fn blocking_post(url: &str, body: &serde_json::Value, cf: Option<&str>) -> (u16, String) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut req = ::reqwest::Client::new().post(url).json(body);
            if let Some(cf) = cf {
                // As cloudflared -> the box's nginx -> the hub delivers a phone.
                req = req
                    .header("x-forwarded-for", "127.0.0.1")
                    .header("cf-connecting-ip", cf);
            }
            let r = req.send().await.unwrap();
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
        // As cloudflared -> the box's nginx -> the hub delivers a phone on mobile data.
        .header("x-forwarded-for", "127.0.0.1")
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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

/// A running test phone: its stop flag, its id once answered, its task.
type RunningPhone = (
    Arc<AtomicBool>,
    Arc<Mutex<Option<String>>>,
    tokio::task::JoinHandle<(String, ClientRun)>,
);

/// Run a phone that connects and stays until `stop`; its id once answered.
fn spawn_phone(base: &str, display: &str, replaces: Option<String>) -> RunningPhone {
    let stop = Arc::new(AtomicBool::new(false));
    let id_out = Arc::new(Mutex::new(None));
    let h = tokio::task::spawn_blocking({
        let (base, display, stop, id_out) = (
            base.to_string(),
            display.to_string(),
            stop.clone(),
            id_out.clone(),
        );
        move || {
            run_client_with(ClientOpts {
                base,
                display,
                replaces,
                extra_candidates: Vec::new(),
                run_for: Duration::from_secs(30),
                stop,
                id_out,
                ..Default::default()
            })
        }
    });
    (stop, id_out, h)
}

async fn fill_pending(hub: &Hub, from: u8) {
    // Two never-connecting offers from two Cloudflare clients: the pending cap of 4 slots.
    for i in 0..2 {
        let ip = format!("198.51.100.{}", from + i);
        assert_eq!(
            post_as(&hub.base, &ip, serde_json::json!({"sdp": silent_offer()})).await,
            200
        );
    }
    assert_eq!(hub.shared.snapshot().phones_pending, 2);
}

// Review round 2 🟡2: a reconnect claim is spent by its use, whether it named a live phone or a
// recently connected one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reconnect_claim_is_spent_once() {
    let hub = start_hub(4).await; // pending cap 2
    let base = hub.base.clone();

    // A recent claim: phone A connects and leaves.
    let (stop, id, h) = spawn_phone(&base, "A", None);
    wait_for("A connected", 5, || {
        hub.shared.snapshot().phones_connected == 1
    })
    .await;
    let a_id = id.lock().unwrap().clone().unwrap();
    stop.store(true, Ordering::Relaxed);
    h.await.unwrap();
    let c = ::reqwest::Client::new();
    let r = c
        .delete(format!("{base}/api/phone/{a_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 204);
    wait_for("A gone", 3, || hub.shared.snapshot().phones.is_empty()).await;

    fill_pending(&hub, 10).await;
    let claim = serde_json::json!({"sdp": silent_offer(), "replaces": a_id});
    assert_eq!(
        post_as(&base, "198.51.100.40", claim.clone()).await,
        200,
        "the claim admits once"
    );
    assert_eq!(
        post_as(&base, "198.51.100.41", claim).await,
        503,
        "the same claim again is an ordinary offer, held to the pending cap"
    );

    // A live claim: phone B connects; its id replaces it once, and the replaced id is no claim.
    wait_for("the pending slots expire", 5, || {
        hub.shared.snapshot().phones_pending == 0
    })
    .await;
    let (stop_b, id_b, hb) = spawn_phone(&base, "B", None);
    wait_for("B connected", 5, || {
        hub.shared.snapshot().phones_connected == 1
    })
    .await;
    let b_id = id_b.lock().unwrap().clone().unwrap();
    fill_pending(&hub, 20).await;
    let claim = serde_json::json!({"sdp": silent_offer(), "replaces": b_id});
    assert_eq!(post_as(&base, "198.51.100.50", claim.clone()).await, 200);
    assert_eq!(hub.shared.snapshot().phones_connected, 0, "B was replaced");
    assert_eq!(post_as(&base, "198.51.100.51", claim).await, 503);
    stop_b.store(true, Ordering::Relaxed);
    hb.await.unwrap();
}

// Review round 2 🟡3: one client (one key) holds at most 3 live or pending sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_client_holds_at_most_its_share_of_the_pending_offers() {
    use intercom_hub::webrtc_leg::pending_cap_per_client;
    let hub = start_hub(16).await; // pending cap 8, 4 per client (review round 4)
    let base = hub.base.clone();
    let n = pending_cap_per_client(16);
    let mut expect = vec![200; n];
    expect.push(429);
    let mut codes = Vec::new();
    for _ in 0..=n {
        codes.push(
            post_as(
                &base,
                "203.0.113.77",
                serde_json::json!({"sdp": silent_offer()}),
            )
            .await,
        );
    }
    assert_eq!(codes, expect);
    // A LAN client that skips nginx cannot pose as someone else: its forged headers are ignored,
    // so its offers all count on its own address (here the test's loopback peer, as nginx would be
    // with no X-Forwarded-For of its own).
    let c = ::reqwest::Client::new();
    let mut forged = Vec::new();
    for i in 0..=n {
        let r = c
            .post(format!("{base}/api/phone/offer"))
            .header("x-forwarded-for", format!("10.0.0.{i}, junk"))
            .json(&serde_json::json!({"sdp": silent_offer()}))
            .send()
            .await
            .unwrap();
        forged.push(r.status().as_u16());
    }
    assert_eq!(
        forged, expect,
        "its own share, not a fresh key per forged header"
    );
    // Once the never-connecting offers expire, the client may offer again.
    wait_for("the pending slots expire", 5, || {
        hub.shared.snapshot().phones_pending == 0
    })
    .await;
    let f = hub.shared.snapshot();
    assert_eq!(f.sessions_never_connected, 2 * n as u64, "{f:?}");
    assert_eq!(
        post_as(
            &base,
            "203.0.113.77",
            serde_json::json!({"sdp": silent_offer()})
        )
        .await,
        200
    );
}

#[test]
fn a_failing_send_warns_once_per_run_and_notes_the_recovery() {
    use intercom_hub::webrtc_leg::SendFailures;
    let mut f = SendFailures::default();
    assert_eq!(f.ok(), None, "nothing to recover from");
    assert_eq!(f.fail(), Some(1), "the first failure warns");
    assert_eq!(f.fail(), None);
    assert_eq!(f.fail(), None);
    assert_eq!(f.ok(), Some(3), "the recovery names the count");
    assert_eq!(f.ok(), None);
    assert_eq!(f.fail(), Some(4), "a new failing run warns again");
}

/// The venue's Wi-Fi: every phone reaches the hub through cloudflared with ONE public address.
const VENUE_IP: &str = "203.0.113.200";

fn spawn_venue_phone(
    base: &str,
    display: String,
    replaces: Option<String>,
    retry_for: Option<Duration>,
) -> RunningPhone {
    let stop = Arc::new(AtomicBool::new(false));
    let id_out = Arc::new(Mutex::new(None));
    let h = tokio::task::spawn_blocking({
        let (base, stop, id_out) = (base.to_string(), stop.clone(), id_out.clone());
        move || {
            run_client_with(ClientOpts {
                base,
                display,
                replaces,
                run_for: Duration::from_secs(60),
                stop,
                id_out,
                client_ip: Some(VENUE_IP.to_string()),
                retry_for,
                ..Default::default()
            })
        }
    });
    (stop, id_out, h)
}

// Review round 3 🔴: a whole crew behind the venue's one address all connect, all reconnect at
// once, and a flood of fresh offers from that same address neither exceeds the pending cap nor
// knocks a connected phone off.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_whole_crew_behind_one_address_connects_and_reconnects_at_once() {
    let hub = start_hub(16).await; // the production limiter: burst 16, 1/s; pending cap 8
    let base = hub.base.clone();

    // 16 phones open the page at once; the per-client pending cap (3) and the hub-wide one (8)
    // pace them, the page's retry (here every 300 ms) brings every one in.
    let mut phones: Vec<RunningPhone> = (0..16)
        .map(|i| spawn_venue_phone(&base, format!("K{i}"), None, Some(Duration::from_secs(20))))
        .collect();
    wait_for("16 phones connected", 30, || {
        hub.shared.snapshot().phones_connected == 16
    })
    .await;
    let ids: Vec<String> = phones
        .iter()
        .map(|(_, id, _)| id.lock().unwrap().clone().unwrap())
        .collect();

    // A network blip: every page drops its session and comes back AT ONCE naming its old id. No
    // retry: each reconnect must be admitted on its first try (no token, no pending cap).
    for (stop, _, _) in &phones {
        stop.store(true, Ordering::Relaxed);
    }
    for (_, _, h) in phones.drain(..) {
        h.await.unwrap();
    }
    let refused_before = hub.shared.snapshot().offers_refused;
    let phones: Vec<RunningPhone> = ids
        .iter()
        .enumerate()
        .map(|(i, old)| spawn_venue_phone(&base, format!("K{i}"), Some(old.clone()), None))
        .collect();
    wait_for("16 phones reconnected", 30, || {
        let f = hub.shared.snapshot();
        f.phones_connected == 16 && f.phones.len() == 16
    })
    .await;
    assert_eq!(
        hub.shared.snapshot().offers_refused,
        refused_before,
        "no reconnect was refused"
    );

    // A flood of fresh offers from the same address: the table is full of connected phones, and
    // nothing of the flood ever holds a slot or knocks one of them off.
    let mut codes = Vec::new();
    for _ in 0..20 {
        codes.push(post_as(&base, VENUE_IP, serde_json::json!({"sdp": silent_offer()})).await);
    }
    assert!(codes.iter().all(|c| *c == 503 || *c == 429), "{codes:?}");
    let f = hub.shared.snapshot();
    assert_eq!(f.phones_connected, 16);
    assert!(f.phones_pending <= 8, "{f:?}");
    for (stop, _, _) in &phones {
        stop.store(true, Ordering::Relaxed);
    }
    for (_, _, h) in phones {
        h.await.unwrap();
    }
}

// The same address with free slots: fresh offers are still bounded by the per-client pending
// cap and the hub-wide one, and a connected phone of that address is never refused a reconnect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flood_from_the_venues_address_stays_inside_the_pending_caps() {
    let hub = start_hub(16).await;
    let base = hub.base.clone();
    let (stop_a, id_a, ha) = spawn_venue_phone(&base, "A".into(), None, None);
    wait_for("A connected", 5, || {
        hub.shared.snapshot().phones_connected == 1
    })
    .await;
    let a_old = id_a.lock().unwrap().clone().unwrap();
    let mut codes = Vec::new();
    for _ in 0..20 {
        codes.push(post_as(&base, VENUE_IP, serde_json::json!({"sdp": silent_offer()})).await);
    }
    let n = intercom_hub::webrtc_leg::pending_cap_per_client(16);
    assert_eq!(codes.iter().filter(|c| **c == 200).count(), n, "{codes:?}");
    assert!(codes.iter().all(|c| *c == 200 || *c == 429), "{codes:?}");
    assert_eq!(hub.shared.snapshot().phones_pending, n as u64);
    // A reconnects during the flood: admitted at once.
    stop_a.store(true, Ordering::Relaxed);
    ha.await.unwrap();
    let (stop_a2, _, ha2) = spawn_venue_phone(&base, "A".into(), Some(a_old), None);
    wait_for("A back", 5, || {
        hub.shared
            .snapshot()
            .phones
            .iter()
            .any(|p| p.display == "A" && p.connected)
    })
    .await;
    stop_a2.store(true, Ordering::Relaxed);
    ha2.await.unwrap();
}

// Review round 3 🟡: a reconnect claim spent on an offer that never connects (or never gets an
// answer) comes back, so the flaky phone's next try is still a reconnect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_spent_on_an_offer_that_never_connects_comes_back() {
    let hub = start_hub(4).await; // pending cap 2
    let base = hub.base.clone();
    let (stop, id, h) = spawn_phone(&base, "A", None);
    wait_for("A connected", 5, || {
        hub.shared.snapshot().phones_connected == 1
    })
    .await;
    let a_id = id.lock().unwrap().clone().unwrap();
    stop.store(true, Ordering::Relaxed);
    h.await.unwrap();

    // The reconnect's offer is answered but its phone never connects (3 s STUN deadline).
    let claim = serde_json::json!({"sdp": silent_offer(), "replaces": a_id});
    assert_eq!(post_as(&base, "198.51.100.60", claim.clone()).await, 200);
    wait_for("the silent reconnect ends", 5, || {
        hub.shared.snapshot().phones.is_empty()
    })
    .await;
    fill_pending(&hub, 70).await;
    assert_eq!(
        post_as(&base, "198.51.100.61", claim.clone()).await,
        200,
        "the claim came back: still a reconnect, past the full pending cap"
    );

    // A reconnect whose SDP fails to build (400) gives its claim back too.
    wait_for("the slots expire", 5, || {
        hub.shared.snapshot().phones.is_empty()
    })
    .await;
    fill_pending(&hub, 80).await;
    let broken = serde_json::json!({"sdp": "v=0 nonsense", "replaces": a_id});
    assert_eq!(post_as(&base, "198.51.100.62", broken).await, 400);
    assert_eq!(post_as(&base, "198.51.100.63", claim).await, 200);
}

// Review round 3 🟡: one phone whose sends fail never makes the healthy ones log.
#[test]
fn send_failures_are_per_phone() {
    use intercom_hub::webrtc_leg::SendFailures;
    let (mut bad, mut good) = (SendFailures::default(), SendFailures::default());
    let mut lines = 0;
    for _ in 0..50 {
        lines += usize::from(bad.fail().is_some());
        lines += usize::from(good.ok().is_some());
    }
    assert_eq!(
        lines, 1,
        "one warning for the failing phone, nothing per packet"
    );
}

// Review round 4 🟡: after a hub RESTART no claim exists, so every phone is a fresh offer under the
// venue's one key. 10 phones retrying with the page's own backoff (1, 2, 4 s; the jitter left out,
// the worst case: all in step) are all connected within ~6 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crew_of_ten_behind_one_address_is_back_within_six_seconds_after_a_restart() {
    let hub = start_hub(16).await; // a fresh hub: `recent` is empty, as after a restart
    let base = hub.base.clone();
    let t0 = Instant::now();
    let phones: Vec<RunningPhone> = (0..10)
        .map(|i| {
            let stop = Arc::new(AtomicBool::new(false));
            let id_out = Arc::new(Mutex::new(None));
            let h = tokio::task::spawn_blocking({
                let (base, stop, id_out) = (base.clone(), stop.clone(), id_out.clone());
                move || {
                    run_client_with(ClientOpts {
                        base,
                        display: format!("R{i}"),
                        run_for: Duration::from_secs(30),
                        stop,
                        id_out,
                        client_ip: Some(VENUE_IP.to_string()),
                        retry_for: Some(Duration::from_secs(20)),
                        pwa_backoff: true,
                        ..Default::default()
                    })
                }
            });
            (stop, id_out, h)
        })
        .collect();
    wait_for("10 phones connected", 20, || {
        hub.shared.snapshot().phones_connected == 10
    })
    .await;
    let took = t0.elapsed();
    assert!(took < Duration::from_secs(6), "the crew took {took:?}");
    for (stop, _, _) in &phones {
        stop.store(true, Ordering::Relaxed);
    }
    for (_, _, h) in phones {
        h.await.unwrap();
    }
}

// Review round 4 🟡: reconnecting phones that are adopted but not connected yet are not fresh
// offers, so a crew-wide reconnect never fills the hub-wide pending cap against a new phone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnects_in_flight_never_hold_the_pending_cap_against_a_new_phone() {
    let hub = start_hub(4).await; // pending cap 2
    let base = hub.base.clone();
    let (stop_a, id_a, ha) = spawn_phone(&base, "A", None);
    let (stop_b, id_b, hb) = spawn_phone(&base, "B", None);
    wait_for("A and B connected", 5, || {
        hub.shared.snapshot().phones_connected == 2
    })
    .await;
    let (a, b) = (
        id_a.lock().unwrap().clone().unwrap(),
        id_b.lock().unwrap().clone().unwrap(),
    );
    for (stop, h) in [(stop_a, ha), (stop_b, hb)] {
        stop.store(true, Ordering::Relaxed);
        h.await.unwrap();
    }
    // Both come back; their new sessions are adopted but have not connected (silent offers).
    for (i, old) in [a, b].into_iter().enumerate() {
        let claim = serde_json::json!({"sdp": silent_offer(), "replaces": old});
        assert_eq!(
            post_as(&base, &format!("198.51.100.9{i}"), claim).await,
            200
        );
    }
    assert_eq!(hub.shared.snapshot().phones.len(), 2);
    assert_eq!(
        hub.shared.snapshot().phones_pending,
        0,
        "reconnects are not pending offers"
    );
    // A brand-new phone still gets a slot.
    assert_eq!(
        post_as(
            &base,
            "198.51.100.99",
            serde_json::json!({"sdp": silent_offer()})
        )
        .await,
        200
    );
}

#[test]
fn one_key_may_hold_half_the_pending_cap_at_least_three() {
    use intercom_hub::webrtc_leg::pending_cap_per_client;
    assert_eq!(pending_cap_per_client(16), 4);
    assert_eq!(pending_cap_per_client(64), 16);
    assert_eq!(pending_cap_per_client(4), 3);
    assert_eq!(pending_cap_per_client(1), 3);
}
