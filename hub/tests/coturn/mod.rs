//! A real TURN server for the relay tests (intercom#42 phase 3): coturn's `turnserver` as a user
//! process on loopback. The binary is `$INTERCOM_TURNSERVER` or `turnserver` on PATH (CI installs
//! the `coturn` package; on dev1 a `dpkg -x` copy runs through a wrapper, see
//! `.claude/rules/strih-intercom.md`). A missing binary FAILS the test, never skips it.
//!
//! The server listens on 127.0.0.2 and relays from 127.0.0.2; peers on 127.0.0.1 are DENIED, so a
//! hub bound to 127.0.0.1 is reachable through its relay only, never from another relay directly.
#![allow(dead_code)]

use std::net::{SocketAddr, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rtc_stun::message::{Message, TransactionId, BINDING_REQUEST};

pub const LISTEN_IP: &str = "127.0.0.2";
pub const REALM: &str = "intercom.test";

pub struct Coturn {
    child: Child,
    pub addr: SocketAddr,
    _dir: tempfile::TempDir,
}

impl Drop for Coturn {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_udp_port(ip: &str) -> u16 {
    UdpSocket::bind(format!("{ip}:0"))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The turnserver binary to run.
pub fn binary() -> String {
    std::env::var("INTERCOM_TURNSERVER").unwrap_or_else(|_| "turnserver".to_string())
}

/// A STUN Binding request answered = the server is up.
fn answers(addr: SocketAddr) -> bool {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut m = Message::new();
    m.build(&[Box::new(TransactionId::new()), Box::new(BINDING_REQUEST)])
        .unwrap();
    if s.send_to(&m.raw, addr).is_err() {
        return false;
    }
    let mut buf = [0u8; 512];
    s.recv_from(&mut buf).is_ok()
}

impl Coturn {
    /// Start coturn with these long-term users; returns once it answers a Binding request.
    pub fn start(users: &[(&str, &str)]) -> Coturn {
        let dir = tempfile::tempdir().unwrap();
        let port = free_udp_port(LISTEN_IP);
        let relay_base = 20_000 + u32::from(port % 400) * 100;
        let mut cmd = Command::new(binary());
        cmd.args([
            "-n",
            &format!("--listening-ip={LISTEN_IP}"),
            &format!("--listening-port={port}"),
            &format!("--relay-ip={LISTEN_IP}"),
            &format!("--min-port={relay_base}"),
            &format!("--max-port={}", relay_base + 99),
            "--lt-cred-mech",
            &format!("--realm={REALM}"),
            "--no-tls",
            "--no-dtls",
            "--no-tcp",
            "--no-tcp-relay",
            "--allow-loopback-peers",
            "--denied-peer-ip=127.0.0.1",
            "--no-cli",
            "--no-software-attribute",
            "--simple-log",
            &format!("--log-file={}/turn.log", dir.path().display()),
            &format!("--db={}/turndb", dir.path().display()),
            &format!("--pidfile={}/turn.pid", dir.path().display()),
        ]);
        for (u, p) in users {
            cmd.arg(format!("--user={u}:{p}"));
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let child = cmd.spawn().unwrap_or_else(|e| {
            panic!(
                "TEST FAILED: cannot start coturn's turnserver ({}): {e}. Install the coturn \
                 package or set INTERCOM_TURNSERVER to a turnserver binary.",
                binary()
            )
        });
        let addr: SocketAddr = format!("{LISTEN_IP}:{port}").parse().unwrap();
        let mut me = Coturn {
            child,
            addr,
            _dir: dir,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = me.child.try_wait() {
                panic!("TEST FAILED: turnserver exited at start ({status})");
            }
            if answers(addr) {
                return me;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = me.child.kill();
        panic!("TEST FAILED: turnserver did not answer on {addr} within 10 s");
    }
}
