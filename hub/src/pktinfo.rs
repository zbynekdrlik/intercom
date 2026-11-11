//! Receive a UDP datagram together with the interface it came in on and its destination address
//! (`IP_PKTINFO`), so the VBAN and Janus RTP sockets can drop what arrived on the Dante leg
//! (intercom#25, [`crate::dante_guard`]). Linux, IPv4 sockets.

use std::io;
use std::mem::{size_of, zeroed};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::Interest;

use crate::dante_guard::{DanteGuard, PacketInfo};

/// Ask the kernel to attach `IP_PKTINFO` to every datagram the socket receives. The hub binds IPv4
/// while its Dante guard is active ([`crate::dante_guard::check_bind`]); Linux also attaches it to
/// the IPv4-mapped datagrams of a dual-stack socket, never to native IPv6 ones.
pub fn enable(socket: &impl AsRawFd) -> io::Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: a valid fd and a c_int option value of the right size.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_PKTINFO,
            (&on as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// [`enable`] for a socket the Dante guard filters: a failure stops the caller while the guard is
/// active (the filter would be blind), and is only a warning when there is nothing to filter.
pub fn enable_guarded(socket: &impl AsRawFd, guard: &DanteGuard, what: &str) -> anyhow::Result<()> {
    match enable(socket) {
        Ok(()) => Ok(()),
        Err(e) if guard.active() => Err(anyhow::anyhow!(
            "{what}: IP_PKTINFO refused ({e}); the Dante filter needs it (intercom#25)"
        )),
        Err(e) => {
            tracing::warn!(what, error = %e, "no IP_PKTINFO on this socket (no Dante leg to filter)");
            Ok(())
        }
    }
}

/// The next datagram the Dante guard admits on `port`; the refused ones are counted and dropped
/// here, before any caller can parse one or learn an address from it (intercom#25).
pub async fn recv_admitted(
    socket: &tokio::net::UdpSocket,
    buf: &mut [u8],
    guard: &DanteGuard,
    port: u16,
) -> io::Result<(usize, SocketAddr)> {
    loop {
        let (n, from, info) = recv_from_with_info(socket, buf).await?;
        if !guard.check_datagram(port, info, from) {
            return Ok((n, from));
        }
    }
}

/// `recv_from` plus the datagram's [`PacketInfo`] (None when the socket has no `IP_PKTINFO`, or
/// the control message was truncated).
pub async fn recv_from_with_info(
    socket: &tokio::net::UdpSocket,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr, Option<PacketInfo>)> {
    let fd = socket.as_raw_fd();
    socket
        .async_io(Interest::READABLE, || recvmsg(fd, buf))
        .await
}

/// One warning for a truncated control message (the buffer holds `IP_PKTINFO` with room to spare).
static CTRUNC_WARNED: AtomicBool = AtomicBool::new(false);

/// Control buffer, aligned for `cmsghdr`.
#[repr(C, align(8))]
struct Control([u8; 64]);

fn recvmsg(fd: RawFd, buf: &mut [u8]) -> io::Result<(usize, SocketAddr, Option<PacketInfo>)> {
    // SAFETY: plain-old-data structs, zero is a valid value for each.
    let mut from: libc::sockaddr_storage = unsafe { zeroed() };
    let mut control = Control([0; 64]);
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: as above.
    let mut msg: libc::msghdr = unsafe { zeroed() };
    msg.msg_name = (&mut from as *mut libc::sockaddr_storage).cast();
    msg.msg_namelen = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.0.as_mut_ptr().cast();
    msg.msg_controllen = control.0.len() as _;
    // SAFETY: msg points at live buffers sized as declared; the fd is the caller's socket.
    let n = unsafe { libc::recvmsg(fd, &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let peer = sockaddr_to_std(&from)?;
    let info = if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        if !CTRUNC_WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(%peer, "recvmsg: control message truncated, no IP_PKTINFO for this datagram (warned once)");
        }
        None
    } else {
        packet_info(&msg)
    };
    Ok((n as usize, peer, info))
}

fn sockaddr_to_std(addr: &libc::sockaddr_storage) -> io::Result<SocketAddr> {
    match i32::from(addr.ss_family) {
        libc::AF_INET => {
            // SAFETY: the family says sockaddr_in.
            let a =
                unsafe { &*(addr as *const libc::sockaddr_storage as *const libc::sockaddr_in) };
            Ok(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)),
                u16::from_be(a.sin_port),
            )))
        }
        libc::AF_INET6 => {
            // SAFETY: the family says sockaddr_in6.
            let a =
                unsafe { &*(addr as *const libc::sockaddr_storage as *const libc::sockaddr_in6) };
            Ok(SocketAddr::V6(SocketAddrV6::new(
                a.sin6_addr.s6_addr.into(),
                u16::from_be(a.sin6_port),
                a.sin6_flowinfo,
                a.sin6_scope_id,
            )))
        }
        f => Err(io::Error::other(format!(
            "recvmsg: unexpected address family {f}"
        ))),
    }
}

fn packet_info(msg: &libc::msghdr) -> Option<PacketInfo> {
    // SAFETY: walking the control messages the kernel wrote into msg's control buffer.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msg);
        while !cmsg.is_null() {
            let c = &*cmsg;
            if c.cmsg_level == libc::IPPROTO_IP && c.cmsg_type == libc::IP_PKTINFO {
                let p = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const libc::in_pktinfo);
                return Some(PacketInfo {
                    ifindex: p.ipi_ifindex as u32,
                    dst: Ipv4Addr::from(u32::from_be(p.ipi_addr.s_addr)),
                });
            }
            cmsg = libc::CMSG_NXTHDR(msg, cmsg);
        }
    }
    None
}
