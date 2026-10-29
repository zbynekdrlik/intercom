//! Each cambox return leg's address (issue 1431, a joint event: the cameras follow the strih that
//! `strih.lan` resolves to).
//!
//! A cambox sends its mic as VBAN stream `camN` to `strih.lan:6980` from an ephemeral port and
//! listens for its return mix on 6980 (`src/intercom.rs`). So the hub sends each `camN` return leg
//! to the SOURCE IP that `camN`'s own incoming stream arrives from, port 6980, learned per packet.
//! The TOML `host` (resolved at start and refreshed off the hot path) is only the START-UP address,
//! used before the first packet, and stays the fallback while nothing has been learned.
//!
//! What learning never does:
//! - create a participant: only a leg the matrix declares (`Matrix::vban_outputs`) has an entry,
//!   a stream of an undeclared name is foreign traffic as before;
//! - change what is mixed: the N-1 invariant lives in `Matrix::from_toml` + the engine; a learned
//!   address only re-targets the leg's own non-blocking socket (one per output slot, issue 1401);
//! - send to a box that is not talking to this hub: a learned address is one this hub just
//!   received the same `camN` stream from.
//!
//! Pure and std-only (serde for the `/api/state` view): the receive task learns, the block loop
//! reads [`ReturnAddrs::currents`], the refresh task moves the start-up address.

use std::net::{IpAddr, SocketAddr};

use serde::Serialize;

use crate::matrix::Matrix;

/// The port a cambox listens on for its return mix (the VBAN port, never the packet's source port).
pub const RETURN_PORT: u16 = intercom_vban::VBAN_PORT;

/// Packets in a row from one new source before a leg learns it.
pub const LEARN_CONFIRM_PACKETS: u32 = 1;

/// Where a leg's current return address comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReturnSource {
    /// No packet yet and the TOML host did not resolve: the leg sends nothing.
    Unresolved,
    /// No packet yet: the TOML host's resolved start-up address.
    Configured,
    /// The source IP of the leg's own incoming stream.
    Learned,
}

/// One leg's return address as `/api/state` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReturnAddrState {
    /// `ip:6980` (serde writes a `SocketAddr` as that string), null while unresolved.
    pub addr: Option<SocketAddr>,
    pub source: ReturnSource,
}

/// A change of a leg's current return address (logged once per change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnChange {
    /// The leg's participant name (`camN`).
    pub leg: String,
    pub old: Option<SocketAddr>,
    pub new: SocketAddr,
}

impl ReturnChange {
    /// `leg camN return address <old> -> <new>`, `unresolved` for no old address.
    pub fn log_line(&self) -> String {
        let old = self
            .old
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unresolved".to_string());
        format!("leg {} return address {} -> {}", self.leg, old, self.new)
    }
}

#[derive(Debug, Clone)]
struct ReturnLeg {
    participant: usize,
    name: String,
    configured: Option<SocketAddr>,
    learned: Option<SocketAddr>,
}

impl ReturnLeg {
    fn current(&self) -> Option<SocketAddr> {
        self.learned.or(self.configured)
    }
}

/// Every return leg's address, indexed by output slot (the order of `Matrix::vban_outputs`).
#[derive(Debug, Clone, Default)]
pub struct ReturnAddrs {
    legs: Vec<ReturnLeg>,
    /// The output slot of each participant id, `None` for a participant without a leg.
    slot_of: Vec<Option<usize>>,
}

impl ReturnAddrs {
    /// `legs` = `(participant id, leg name, resolved start-up address)` per output slot.
    pub fn new(n_participants: usize, legs: Vec<(usize, String, Option<SocketAddr>)>) -> Self {
        let mut slot_of = vec![None; n_participants];
        let legs: Vec<ReturnLeg> = legs
            .into_iter()
            .enumerate()
            .map(|(slot, (participant, name, configured))| {
                if let Some(s) = slot_of.get_mut(participant) {
                    *s = Some(slot);
                }
                ReturnLeg {
                    participant,
                    name,
                    configured,
                    learned: None,
                }
            })
            .collect();
        ReturnAddrs { legs, slot_of }
    }

    /// One leg per `Matrix::vban_outputs` entry, its host resolved by `resolve` (the start-up
    /// address; a one-shot lookup, never on the block hot path).
    pub fn from_matrix(matrix: &Matrix, resolve: impl Fn(&str) -> Option<SocketAddr>) -> Self {
        let legs = matrix
            .vban_outputs()
            .into_iter()
            .map(|(id, _, host)| (id, matrix.participants[id].name.clone(), resolve(&host)))
            .collect();
        Self::new(matrix.participants.len(), legs)
    }

    pub fn len(&self) -> usize {
        self.legs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.legs.is_empty()
    }

    /// The address output slot `slot` sends to now (learned, else the start-up address).
    pub fn current(&self, slot: usize) -> Option<SocketAddr> {
        self.legs.get(slot).and_then(ReturnLeg::current)
    }

    /// Every slot's current address (the block loop copies this out of the lock once per block).
    pub fn currents(&self) -> Vec<Option<SocketAddr>> {
        self.legs.iter().map(ReturnLeg::current).collect()
    }

    /// A refreshed start-up address for `slot`. A learned address is never overridden; the change
    /// is returned only when the slot's CURRENT address moved.
    pub fn set_configured(&mut self, slot: usize, addr: SocketAddr) -> Option<ReturnChange> {
        let leg = self.legs.get_mut(slot)?;
        let old = leg.current();
        leg.configured = Some(addr);
        change(leg, old)
    }

    /// A decoded packet of `participant`'s own stream arrived from `from`: its leg now sends to
    /// `from:6980`. `None` for a participant without a configured leg, or when the leg's current
    /// address did not move (the first packet from the configured address only marks it learned).
    pub fn learn(&mut self, participant: usize, from: IpAddr) -> Option<ReturnChange> {
        let slot = self.slot_of.get(participant).copied().flatten()?;
        let leg = &mut self.legs[slot];
        let old = leg.current();
        leg.learned = Some(SocketAddr::new(from, RETURN_PORT));
        change(leg, old)
    }

    /// Each leg's state for `/api/state`, as `(participant id, state)`.
    pub fn states(&self) -> Vec<(usize, ReturnAddrState)> {
        self.legs
            .iter()
            .map(|l| {
                let source = if l.learned.is_some() {
                    ReturnSource::Learned
                } else if l.configured.is_some() {
                    ReturnSource::Configured
                } else {
                    ReturnSource::Unresolved
                };
                (
                    l.participant,
                    ReturnAddrState {
                        addr: l.current(),
                        source,
                    },
                )
            })
            .collect()
    }
}

fn change(leg: &ReturnLeg, old: Option<SocketAddr>) -> Option<ReturnChange> {
    let new = leg.current()?;
    (old != Some(new)).then(|| ReturnChange {
        leg: leg.name.clone(),
        old,
        new,
    })
}

/// The receive task's per-participant memory of the last source IP, so the shared table (whose
/// lock the real-time block loop takes every block) is locked only when a stream's source changes,
/// never per packet.
#[derive(Debug, Clone, Default)]
pub struct LearnFilter {
    last: Vec<Option<IpAddr>>,
}

impl LearnFilter {
    pub fn new(n_participants: usize) -> Self {
        LearnFilter {
            last: vec![None; n_participants],
        }
    }

    /// True the first time `participant`'s stream arrives from `from` (and remembers it). An id
    /// outside the matrix is never new.
    pub fn is_new(&mut self, participant: usize, from: IpAddr) -> bool {
        match self.last.get_mut(participant) {
            Some(last) if *last != Some(from) => {
                *last = Some(from);
                true
            }
            _ => false,
        }
    }
}
