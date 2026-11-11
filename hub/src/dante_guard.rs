//! The hub's non-Dante services stay off the Dante leg (intercom#25, dstack two-networks.md §4).
//!
//! The hub is both a Dante device (dlink, bound to the Dante address by dstack) and a LAN service:
//! the HTTP/PWA, the VBAN socket and the Janus RTP socket. dstack's ruling (intercom#25, option B):
//! those keep listening on all addresses and drop at once whatever arrives on the Dante leg, so a
//! LAN address that changes needs no rebind.
//!
//! The Dante leg is known two ways, the union of both:
//! - the dlink interface from the matrix (`[dlink] iface`): its ifindex, its IPv4 address and that
//!   address's subnet, re-resolved by [`DanteGuard::spawn_refresher`] (dlink brings the leg up and a
//!   DHCP lease can move it);
//! - `DANTE_SUBNET` (§4's per-app setting, CIDR, comma-separated), from the environment: the site's
//!   subnet is not in a public repo.
//!
//! [`DanteGuard`] is the one place that decides; the TCP accept loop ([`crate::http_serve`]), the
//! VBAN and Janus RTP receive loops ([`crate::pktinfo`]) and the NDI discovery ask it. A refusal is
//! counted per port and warned once per port (§4: the PC's layout is only the second line of
//! defence, so a refusal means that line was not there).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

/// The environment variable that names the Dante subnet(s), §4's per-app setting.
pub const DANTE_SUBNET_ENV: &str = "DANTE_SUBNET";

/// How often the dlink interface is re-resolved.
pub const REFRESH_EVERY: Duration = Duration::from_secs(2);

/// An IPv4 subnet in CIDR form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Net {
    network: Ipv4Addr,
    prefix: u8,
}

impl Ipv4Net {
    /// The subnet `addr/prefix`; host bits of `addr` are cleared. `prefix` above 32 is refused.
    pub fn new(addr: Ipv4Addr, prefix: u8) -> Result<Self> {
        if prefix > 32 {
            bail!("prefix /{prefix} is above /32");
        }
        Ok(Self {
            network: Ipv4Addr::from(u32::from(addr) & mask(prefix)),
            prefix,
        })
    }

    pub fn network(&self) -> Ipv4Addr {
        self.network
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & mask(self.prefix) == u32::from(self.network)
    }
}

impl std::fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

impl FromStr for Ipv4Net {
    type Err = anyhow::Error;

    /// `a.b.c.d/n`, or a bare address as `/32`.
    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (
                a,
                p.parse::<u8>()
                    .with_context(|| format!("bad prefix in '{s}'"))?,
            ),
            None => (s, 32),
        };
        let addr: Ipv4Addr = addr
            .parse()
            .with_context(|| format!("bad IPv4 address in '{s}'"))?;
        Ipv4Net::new(addr, prefix)
    }
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

/// Parse a `DANTE_SUBNET` value: one or more CIDRs separated by commas. Empty means none.
pub fn parse_subnets(value: &str) -> Result<Vec<Ipv4Net>> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Ipv4Net::from_str)
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("{DANTE_SUBNET_ENV}='{value}'"))
}

/// What arrived with a datagram, from `IP_PKTINFO`: the interface it came in on and the
/// destination address in its IP header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketInfo {
    pub ifindex: u32,
    pub dst: Ipv4Addr,
}

/// The Dante leg as the hub knows it right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DanteNets {
    /// The dlink interface's index, when it exists.
    pub ifindex: Option<u32>,
    /// The dlink interface's IPv4 address, when it has one.
    pub iface_addr: Option<Ipv4Addr>,
    /// Every Dante subnet: the dlink interface's own and `DANTE_SUBNET`'s.
    pub nets: Vec<Ipv4Net>,
}

impl DanteNets {
    /// Whether `ip` (an IPv4-mapped IPv6 address counts as its IPv4) is a Dante address.
    pub fn is_dante(&self, ip: IpAddr) -> bool {
        let v4 = match ip {
            IpAddr::V4(v4) => v4,
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => v4,
                None => return false,
            },
        };
        self.iface_addr == Some(v4) || self.nets.iter().any(|n| n.contains(v4))
    }

    /// A TCP connection is refused when its local address is a Dante address.
    pub fn refuses_connection(&self, local: SocketAddr) -> bool {
        self.is_dante(local.ip())
    }

    /// A datagram is refused when it came in on the dlink interface or was sent to a Dante address.
    /// Without packet info (a socket that is not IPv4) nothing can be told, so nothing is refused.
    pub fn refuses_datagram(&self, info: Option<PacketInfo>) -> bool {
        match info {
            Some(i) => self.ifindex == Some(i.ifindex) || self.is_dante(IpAddr::V4(i.dst)),
            None => false,
        }
    }
}

/// The per-port refusal counts. The first refusal on a port is warned once.
#[derive(Debug, Default)]
pub struct Refusals {
    by_port: Mutex<BTreeMap<u16, u64>>,
}

impl Refusals {
    /// Count one refusal on `port`; true when it is the port's first.
    pub fn record(&self, port: u16) -> bool {
        let mut map = self.by_port.lock().unwrap_or_else(|e| e.into_inner());
        let n = map.entry(port).or_insert(0);
        *n += 1;
        *n == 1
    }

    pub fn snapshot(&self) -> BTreeMap<u16, u64> {
        self.by_port
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// The hub's one decision point for "is this the Dante leg".
#[derive(Debug)]
pub struct DanteGuard {
    iface: Option<String>,
    configured: Vec<Ipv4Net>,
    nets: RwLock<DanteNets>,
    refusals: Refusals,
}

impl DanteGuard {
    /// A guard for the dlink interface `iface` (if any) and the `configured` subnets, resolved now.
    pub fn new(iface: Option<String>, configured: Vec<Ipv4Net>) -> Arc<Self> {
        let nets = build_nets(iface.as_deref(), &configured, resolve_iface);
        Arc::new(Self {
            iface,
            configured,
            nets: RwLock::new(nets),
            refusals: Refusals::default(),
        })
    }

    /// [`DanteGuard::new`] with the subnets read from `DANTE_SUBNET`. A malformed value is an error
    /// (the hub refuses to start rather than run without the filter it was told to apply).
    pub fn from_env(iface: Option<String>) -> Result<Arc<Self>> {
        let configured = match std::env::var(DANTE_SUBNET_ENV) {
            Ok(v) => parse_subnets(&v)?,
            Err(std::env::VarError::NotPresent) => Vec::new(),
            Err(e) => return Err(anyhow!("{DANTE_SUBNET_ENV}: {e}")),
        };
        Ok(Self::new(iface, configured))
    }

    /// Whether the guard filters anything: a dlink interface or a configured subnet.
    pub fn active(&self) -> bool {
        self.iface.is_some() || !self.configured.is_empty()
    }

    pub fn iface(&self) -> Option<&str> {
        self.iface.as_deref()
    }

    /// The Dante leg as known right now.
    pub fn current(&self) -> DanteNets {
        self.nets.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Re-resolve the dlink interface; returns the new view when it changed.
    pub fn refresh(&self) -> Option<DanteNets> {
        let fresh = build_nets(self.iface.as_deref(), &self.configured, resolve_iface);
        let mut cur = self.nets.write().unwrap_or_else(|e| e.into_inner());
        if *cur == fresh {
            return None;
        }
        *cur = fresh.clone();
        Some(fresh)
    }

    /// Whether `ip` is a Dante address right now.
    pub fn is_dante(&self, ip: IpAddr) -> bool {
        self.nets
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_dante(ip)
    }

    /// Decide an accepted TCP connection; a refusal is counted and logged. True = refuse it.
    pub fn check_connection(&self, local: SocketAddr, peer: SocketAddr) -> bool {
        let refused = self
            .nets
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .refuses_connection(local);
        if refused {
            self.note_refusal("tcp", local.port(), peer);
        }
        refused
    }

    /// Decide a received datagram on `port`; a refusal is counted and logged. True = drop it.
    pub fn check_datagram(&self, port: u16, info: Option<PacketInfo>, from: SocketAddr) -> bool {
        let refused = self
            .nets
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .refuses_datagram(info);
        if refused {
            self.note_refusal("udp", port, from);
        }
        refused
    }

    /// The refusal counts per local port (`/api/state` `dante_refusals`).
    pub fn refusals(&self) -> BTreeMap<u16, u64> {
        self.refusals.snapshot()
    }

    fn note_refusal(&self, proto: &str, port: u16, peer: SocketAddr) {
        if self.refusals.record(port) {
            tracing::warn!(
                proto,
                port,
                %peer,
                iface = self.iface.as_deref().unwrap_or("-"),
                "dante-guard: refused traffic that arrived on the Dante leg (counted per port in /api/state dante_refusals; further refusals on this port are not logged)"
            );
        }
    }

    /// Re-resolve the dlink interface every [`REFRESH_EVERY`] and log each change. Only when there
    /// is an interface to follow.
    pub fn spawn_refresher(self: &Arc<Self>) {
        if self.iface.is_none() {
            return;
        }
        let guard = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REFRESH_EVERY);
            loop {
                tick.tick().await;
                if let Some(now) = guard.refresh() {
                    tracing::info!(
                        iface = guard.iface.as_deref().unwrap_or("-"),
                        ifindex = ?now.ifindex,
                        addr = ?now.iface_addr,
                        nets = %format_nets(&now.nets),
                        "dante-guard: the Dante leg changed"
                    );
                }
            }
        });
    }

    /// One log line naming what the guard filters.
    pub fn log_start(&self) {
        let now = self.current();
        if !self.active() {
            tracing::info!(
                "dante-guard: no dlink interface and no {DANTE_SUBNET_ENV}: nothing to filter"
            );
            return;
        }
        tracing::info!(
            iface = self.iface.as_deref().unwrap_or("-"),
            ifindex = ?now.ifindex,
            addr = ?now.iface_addr,
            nets = %format_nets(&now.nets),
            "dante-guard: the HTTP, VBAN and Janus RTP sockets refuse traffic on the Dante leg"
        );
    }
}

fn format_nets(nets: &[Ipv4Net]) -> String {
    if nets.is_empty() {
        return "-".to_string();
    }
    nets.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// The resolved view of one interface: its index and its first IPv4 address with that address's
/// subnet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IfaceInfo {
    pub ifindex: u32,
    pub addr: Option<(Ipv4Addr, Ipv4Net)>,
}

/// Build the view from an interface resolver (pure given `resolve`).
pub fn build_nets(
    iface: Option<&str>,
    configured: &[Ipv4Net],
    resolve: impl Fn(&str) -> Option<IfaceInfo>,
) -> DanteNets {
    let mut nets = DanteNets {
        nets: configured.to_vec(),
        ..DanteNets::default()
    };
    if let Some(info) = iface.and_then(resolve) {
        nets.ifindex = Some(info.ifindex);
        if let Some((addr, net)) = info.addr {
            nets.iface_addr = Some(addr);
            if !nets.nets.contains(&net) {
                nets.nets.push(net);
            }
        }
    }
    nets
}

/// Resolve `name` with `if_nametoindex` and `getifaddrs`. None when the interface does not exist.
pub fn resolve_iface(name: &str) -> Option<IfaceInfo> {
    let cname = std::ffi::CString::new(name).ok()?;
    // SAFETY: a valid NUL-terminated string; the call only reads it.
    let ifindex = unsafe { libc::if_nametoindex(cname.as_ptr()) };
    if ifindex == 0 {
        return None;
    }
    Some(IfaceInfo {
        ifindex,
        addr: iface_ipv4(name),
    })
}

fn iface_ipv4(name: &str) -> Option<(Ipv4Addr, Ipv4Net)> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we free below with freeifaddrs.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return None;
    }
    let mut found = None;
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: `cur` is a node of the list getifaddrs returned and is not freed yet.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_name.is_null() || ifa.ifa_addr.is_null() || ifa.ifa_netmask.is_null() {
            continue;
        }
        // SAFETY: a NUL-terminated name owned by the list.
        let ifa_name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) };
        if ifa_name.to_bytes() != name.as_bytes() {
            continue;
        }
        // SAFETY: ifa_addr points at a sockaddr whose family field is valid to read.
        if i32::from(unsafe { (*ifa.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        // SAFETY: AF_INET: both pointers are sockaddr_in.
        let (addr, netmask) = unsafe {
            let a = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let m = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
            (
                Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)),
                u32::from_be(m.sin_addr.s_addr),
            )
        };
        let prefix = netmask.leading_ones() as u8;
        if let Ok(net) = Ipv4Net::new(addr, prefix) {
            found = Some((addr, net));
            break;
        }
    }
    // SAFETY: the list getifaddrs allocated, freed once.
    unsafe { libc::freeifaddrs(head) };
    found
}

/// Refuse a configured listen address inside the Dante leg: whatever the hub binds (and tells
/// Janus, for the RTP bind) must never be a Dante address (§4).
pub fn check_bind(guard: &DanteGuard, what: &str, addr: SocketAddr) -> Result<()> {
    if guard.is_dante(addr.ip()) {
        bail!("{what} {addr} is a Dante address: a non-Dante service never binds the Dante leg (intercom#25, two-networks §4)");
    }
    Ok(())
}
