//! Each cambox return leg's address (issue 1431): RED stub, the behaviour lands in the GREEN commit.

use std::net::{IpAddr, SocketAddr};

use serde::Serialize;

use crate::matrix::Matrix;

/// The port a cambox listens on for its return mix.
pub const RETURN_PORT: u16 = intercom_vban::VBAN_PORT;

/// Where a leg's current return address comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReturnSource {
    Unresolved,
    Configured,
    Learned,
}

/// One leg's return address as `/api/state` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReturnAddrState {
    pub addr: Option<String>,
    pub source: ReturnSource,
}

/// A change of a leg's current return address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnChange {
    pub leg: String,
    pub old: Option<SocketAddr>,
    pub new: SocketAddr,
}

impl ReturnChange {
    pub fn log_line(&self) -> String {
        String::new()
    }
}

#[derive(Debug, Clone)]
struct ReturnLeg {
    participant: usize,
    configured: Option<SocketAddr>,
}

/// Every return leg's address, indexed by output slot.
#[derive(Debug, Clone, Default)]
pub struct ReturnAddrs {
    legs: Vec<ReturnLeg>,
}

impl ReturnAddrs {
    pub fn new(_n_participants: usize, legs: Vec<(usize, String, Option<SocketAddr>)>) -> Self {
        ReturnAddrs {
            legs: legs
                .into_iter()
                .map(|(participant, _, configured)| ReturnLeg {
                    participant,
                    configured,
                })
                .collect(),
        }
    }

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

    pub fn current(&self, slot: usize) -> Option<SocketAddr> {
        self.legs.get(slot).and_then(|l| l.configured)
    }

    pub fn currents(&self) -> Vec<Option<SocketAddr>> {
        self.legs.iter().map(|l| l.configured).collect()
    }

    pub fn set_configured(&mut self, slot: usize, addr: SocketAddr) -> Option<ReturnChange> {
        if let Some(l) = self.legs.get_mut(slot) {
            l.configured = Some(addr);
        }
        None
    }

    pub fn learn(&mut self, _participant: usize, _from: IpAddr) -> Option<ReturnChange> {
        None
    }

    pub fn states(&self) -> Vec<(usize, ReturnAddrState)> {
        self.legs
            .iter()
            .map(|l| {
                (
                    l.participant,
                    ReturnAddrState {
                        addr: l.configured.map(|a| a.to_string()),
                        source: ReturnSource::Configured,
                    },
                )
            })
            .collect()
    }
}

/// The receive task's per-participant memory of the last source IP, so the shared table is locked
/// only when a stream's source changes.
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

    pub fn is_new(&mut self, _participant: usize, _from: IpAddr) -> bool {
        false
    }
}
