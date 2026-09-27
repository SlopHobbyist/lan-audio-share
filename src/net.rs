//! Sockets, peer discovery and the background threads that run them.
//!
//! There is no server and no rendezvous service. A receiver repeats a small
//! "I am listening" beacon; a sender collects those and unicasts audio to each
//! one. That means either end can be started or restarted in any order and the
//! link comes back on its own within a beacon interval.
//!
//! Beacons go out over both multicast and subnet broadcast, because networks
//! that quietly drop one often pass the other. If both are blocked (or the two
//! machines are on different subnets), the manual peer list in the settings is
//! the escape hatch.

use crate::config::Config;
use crate::protocol::{
    BEACON_INTERVAL_MS, Beacon, DISCOVERY_PORT, MCAST_GROUP, PEER_TIMEOUT_MS, PT_BYE, PT_HELLO,
    PT_SENDER, local_name,
};
use crate::stats::{Peer, Stats};
use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The set of addresses a sender is currently streaming to.
///
/// Wrapped so the audio callback can grab the current list with a `try_lock` and
/// an `Arc` clone, never allocating and never waiting on the discovery thread.
pub type PeerTargets = Arc<Mutex<Arc<Vec<SocketAddr>>>>;

pub fn new_peer_targets() -> PeerTargets {
    Arc::new(Mutex::new(Arc::new(Vec::new())))
}

fn mcast_addr() -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::from(MCAST_GROUP), DISCOVERY_PORT)
}

/// Open the shared discovery socket: bound to the well-known port, joined to the
/// multicast group, and able to send broadcasts.
///
/// `SO_REUSEADDR` matters here — without it a second instance on the same
/// machine cannot bind the port, and on Windows the bind fails outright.
pub fn discovery_socket() -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("could not create discovery socket")?;
    socket.set_reuse_address(true)?;
    let bind: SocketAddr = SocketAddr::from(([0, 0, 0, 0], DISCOVERY_PORT));
    socket
        .bind(&bind.into())
        .with_context(|| format!("could not bind discovery port {DISCOVERY_PORT}"))?;
    socket.set_broadcast(true)?;
    socket.set_multicast_ttl_v4(1)?;
    // Loop multicast back locally so a sender and receiver on one machine can
    // still find each other, which is how this gets tested.
    socket.set_multicast_loop_v4(true)?;
    // Joining on the unspecified interface lets the OS pick the default route.
    socket
        .join_multicast_v4(&Ipv4Addr::from(MCAST_GROUP), &Ipv4Addr::UNSPECIFIED)
        .ok();
    socket.set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(socket.into())
}

/// Open the socket a receiver listens for audio on.
///
/// Falls back to an ephemeral port if the configured one is taken, since the
/// beacon advertises whichever port we actually got.
pub fn audio_socket(preferred_port: u16) -> Result<UdpSocket> {
    match UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], preferred_port))) {
        Ok(socket) => Ok(socket),
        Err(_) => UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))
            .context("could not bind any audio port"),
    }
}

/// Socket a sender transmits from.
pub fn sender_socket() -> Result<UdpSocket> {
    let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))
        .context("could not open sending socket")?;
    socket.set_broadcast(true).ok();
    Ok(socket)
}

fn send_beacon(socket: &UdpSocket, beacon: &Beacon) {
    let bytes = beacon.encode();
    let _ = socket.send_to(&bytes, mcast_addr());
    // Broadcast as well: some networks pass one and not the other.
    let _ = socket.send_to(
        &bytes,
        SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT)),
    );
}

/// Handle for the background threads belonging to one active role. Dropping it
/// stops them.
pub struct NetThreads {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl NetThreads {
    fn new(stop: Arc<AtomicBool>) -> Self {
        Self {
            stop,
            handles: Vec::new(),
        }
    }

    fn push(&mut self, handle: JoinHandle<()>) {
        self.handles.push(handle);
    }
}

impl Drop for NetThreads {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Spawn a background thread that runs until its handle is dropped.
///
/// The body is handed the stop flag and is expected to check it regularly; any
/// socket it waits on should therefore carry a read timeout.
pub fn spawn_stoppable(
    name: &str,
    body: impl FnOnce(&AtomicBool) + Send + 'static,
) -> Result<NetThreads> {
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = NetThreads::new(stop.clone());
    let handle = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || body(&stop))
        .with_context(|| format!("could not start {name} thread"))?;
    threads.push(handle);
    Ok(threads)
}

/// Run a sender's discovery: collect receiver beacons into the peer list and
/// announce ourselves so the receiver can display our name.
pub fn spawn_sender_discovery(
    stats: Arc<Stats>,
    targets: PeerTargets,
    config: Config,
) -> Result<NetThreads> {
    let socket = discovery_socket()?;
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = NetThreads::new(stop.clone());

    let manual = config.parsed_manual_peers();
    let name = local_name();
    let rate = config.sample_rate;
    let channels = config.channels() as u8;

    let handle = std::thread::Builder::new()
        .name("discovery-send".into())
        .spawn(move || {
            let announce = Beacon {
                kind: PT_SENDER,
                audio_port: 0,
                sample_rate: rate,
                channels,
                name: name.clone(),
            };
            let timeout = Duration::from_millis(PEER_TIMEOUT_MS);
            let mut last_announce = Instant::now() - Duration::from_secs(1);
            let mut buf = [0u8; 512];

            // Seed the manual peers so audio flows even with no discovery at all.
            {
                let mut peers: Vec<Peer> = manual
                    .iter()
                    .map(|addr| Peer {
                        addr: *addr,
                        name: format!("{} (manual)", addr.ip()),
                        last_seen: Instant::now(),
                        manual: true,
                    })
                    .collect();
                if let Ok(mut guard) = stats.peers.lock() {
                    guard.append(&mut peers);
                }
                publish(&stats, &targets, timeout);
            }

            while !stop.load(Ordering::Relaxed) {
                if last_announce.elapsed() >= Duration::from_millis(BEACON_INTERVAL_MS) {
                    send_beacon(&socket, &announce);
                    last_announce = Instant::now();
                    // Expiring peers on the same tick keeps the list honest when
                    // a receiver disappears without saying goodbye.
                    publish(&stats, &targets, timeout);
                }

                let Ok((len, from)) = socket.recv_from(&mut buf) else {
                    continue;
                };
                let Some(beacon) = Beacon::parse(&buf[..len]) else {
                    continue;
                };
                let SocketAddr::V4(from_v4) = from else {
                    continue;
                };

                match beacon.kind {
                    PT_HELLO => {
                        let addr = SocketAddr::from((*from_v4.ip(), beacon.audio_port));
                        note_peer(&stats, addr, &beacon.name);
                        publish(&stats, &targets, timeout);
                    }
                    PT_BYE => {
                        let addr = SocketAddr::from((*from_v4.ip(), beacon.audio_port));
                        if let Ok(mut guard) = stats.peers.lock() {
                            guard.retain(|p| p.manual || p.addr != addr);
                        }
                        publish(&stats, &targets, timeout);
                    }
                    _ => {}
                }
            }
        })
        .context("could not start discovery thread")?;
    threads.push(handle);
    Ok(threads)
}

fn note_peer(stats: &Stats, addr: SocketAddr, name: &str) {
    if addr.port() == 0 {
        return;
    }
    if let Ok(mut guard) = stats.peers.lock() {
        if let Some(existing) = guard.iter_mut().find(|p| p.addr == addr) {
            existing.last_seen = Instant::now();
            if !name.is_empty() {
                existing.name = name.to_string();
            }
        } else {
            guard.push(Peer {
                addr,
                name: name.to_string(),
                last_seen: Instant::now(),
                manual: false,
            });
        }
    }
}

/// Drop expired peers and republish the address list the audio callback reads.
fn publish(stats: &Stats, targets: &PeerTargets, timeout: Duration) {
    let addrs: Vec<SocketAddr> = {
        let Ok(mut guard) = stats.peers.lock() else {
            return;
        };
        guard.retain(|p| p.is_live(timeout));
        guard.iter().map(|p| p.addr).collect()
    };
    if let Ok(mut guard) = targets.lock() {
        // Only swap when the set actually changed, so the callback's cached Arc
        // stays valid (and cheap) most of the time.
        if **guard != addrs {
            *guard = Arc::new(addrs);
        }
    }
}

/// Run a receiver's discovery: announce ourselves regularly and pick up the
/// sender's name for display.
pub fn spawn_receiver_beacon(
    stats: Arc<Stats>,
    audio_port: u16,
    sample_rate: u32,
    channels: u8,
) -> Result<NetThreads> {
    let socket = discovery_socket()?;
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = NetThreads::new(stop.clone());
    let name = local_name();

    let handle = std::thread::Builder::new()
        .name("discovery-recv".into())
        .spawn(move || {
            let mut hello = Beacon {
                kind: PT_HELLO,
                audio_port,
                sample_rate,
                channels,
                name,
            };
            let mut last_beacon = Instant::now() - Duration::from_secs(1);
            let mut buf = [0u8; 512];

            while !stop.load(Ordering::Relaxed) {
                if last_beacon.elapsed() >= Duration::from_millis(BEACON_INTERVAL_MS) {
                    send_beacon(&socket, &hello);
                    last_beacon = Instant::now();
                }

                let Ok((len, _from)) = socket.recv_from(&mut buf) else {
                    continue;
                };
                if let Some(beacon) = Beacon::parse(&buf[..len])
                    && beacon.kind == PT_SENDER
                    && !beacon.name.is_empty()
                    && let Ok(mut guard) = stats.remote_name.lock()
                    && *guard != beacon.name
                {
                    *guard = beacon.name;
                }
            }

            // Tell any sender we are going away so it stops immediately rather
            // than waiting for us to time out.
            hello.kind = PT_BYE;
            send_beacon(&socket, &hello);
        })
        .context("could not start beacon thread")?;
    threads.push(handle);
    Ok(threads)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// The no-configuration promise, exercised for real: start a receiver
    /// beacon and a sender's discovery on live sockets and check the sender
    /// works out where to send audio without being told anything.
    ///
    /// This depends on the machine passing multicast or broadcast to loopback,
    /// so a failure here is a local network-stack finding rather than a logic
    /// bug. The manual peer list exists for exactly that case.
    #[test]
    fn sender_discovers_a_receiver() {
        let receiver_stats = Arc::new(Stats::new());
        let sender_stats = Arc::new(Stats::new());
        let targets = new_peer_targets();

        let listen_port = 47_999u16;
        let _receiver = match spawn_receiver_beacon(receiver_stats, listen_port, 48_000, 2) {
            Ok(handle) => handle,
            Err(err) => {
                eprintln!("skipping: could not open a discovery socket ({err})");
                return;
            }
        };

        let _sender = match spawn_sender_discovery(
            sender_stats.clone(),
            targets.clone(),
            Config::default(),
        ) {
            Ok(handle) => handle,
            Err(err) => {
                eprintln!("skipping: could not open a discovery socket ({err})");
                return;
            }
        };

        // Beacons go out every 500 ms, so a few seconds is ample.
        let mut found = Vec::new();
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(100));
            found = targets.lock().map(|g| (**g).clone()).unwrap_or_default();
            if found.iter().any(|a| a.port() == listen_port) {
                break;
            }
        }

        assert!(
            found.iter().any(|a| a.port() == listen_port),
            "sender never discovered the receiver; targets were {found:?}"
        );

        // And the receiver should have learned the sender is out there.
        let peers = sender_stats.live_peers();
        assert!(
            peers.iter().any(|p| p.addr.port() == listen_port),
            "peer table did not record the receiver"
        );
    }
}
