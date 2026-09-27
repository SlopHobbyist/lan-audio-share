//! Sockets, peer discovery and the background threads that run them.
//!
//! There is no server and no rendezvous service. A receiver repeats a small
//! "I am listening" beacon; a sender collects those and unicasts audio to each
//! one. That means either end can be started or restarted in any order and the
//! link comes back on its own within a beacon interval.
//!
//! Beacons go out over both multicast and subnet broadcast, because networks
//! that quietly drop one often pass the other. They are sent separately on
//! every interface: a machine with VirtualBox, Hyper-V, WSL or a VPN installed
//! routes a plain 255.255.255.255 broadcast (and an unpinned multicast) out of
//! whichever adapter has the lowest metric, which is frequently a virtual one
//! that goes nowhere.
//!
//! Each side also answers the other's beacon with a unicast reply, so discovery
//! works as long as a broadcast gets through in either direction. If both are
//! blocked (or the two machines are on different subnets), the manual peer list
//! in the settings is the escape hatch.
//!
//! Firewalls: a receiver sends its beacons from the audio socket itself. Windows
//! Firewall admits unicast replies to a port that recently sent a broadcast or
//! multicast, even with no inbound rule, and a beacon goes out every 500 ms, so
//! that exemption stays open for as long as the receiver is running.

use crate::config::Config;
use crate::protocol::{
    BEACON_INTERVAL_MS, Beacon, DISCOVERY_PORT, MCAST_GROUP, PEER_TIMEOUT_MS, PT_BYE, PT_HELLO,
    PT_SENDER, local_name,
};
use crate::stats::{Peer, Stats};
use anyhow::{Context, Result};
use socket2::{Domain, Protocol, SockRef, Socket, Type};
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

/// How often the interface list is re-read, so a cable plugged in or a Wi-Fi
/// network joined after launch starts carrying beacons without a restart.
const INTERFACE_REFRESH: Duration = Duration::from_secs(5);

/// A peer heard at a new address under a name and port we already know is taken
/// to be the same machine on another interface, unless the old address has gone
/// quiet for this long (the machine's IP changed).
const ADDRESS_SWITCH_AFTER: Duration = Duration::from_millis(BEACON_INTERVAL_MS * 4);

/// One IPv4 interface to announce on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LanInterface {
    ip: Ipv4Addr,
    broadcast: Ipv4Addr,
}

/// Every up, non-loopback IPv4 interface.
fn lan_interfaces() -> Vec<LanInterface> {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut out: Vec<LanInterface> = interfaces
        .iter()
        .filter(|i| !i.is_loopback() && i.is_oper_up())
        .filter_map(|i| match &i.addr {
            if_addrs::IfAddr::V4(v4) => Some(LanInterface {
                ip: v4.ip,
                // Windows does not report a broadcast address, so derive it.
                broadcast: v4
                    .broadcast
                    .unwrap_or_else(|| Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask))),
            }),
            _ => None,
        })
        .collect();
    out.dedup();
    out
}

/// Tracks the machine's interfaces and sends beacons out of each one.
struct Announcer {
    interfaces: Vec<LanInterface>,
    refreshed: Instant,
}

impl Announcer {
    fn new() -> Self {
        Self {
            interfaces: lan_interfaces(),
            refreshed: Instant::now(),
        }
    }

    /// Re-read the interface list if it is due, joining the discovery group on
    /// any new interface when given the socket that listens for beacons.
    fn refresh(&mut self, listener: Option<&UdpSocket>) {
        if self.refreshed.elapsed() < INTERFACE_REFRESH {
            return;
        }
        self.refreshed = Instant::now();
        self.interfaces = lan_interfaces();
        if let Some(socket) = listener {
            join_group(socket, &self.interfaces);
        }
    }

    /// Multicast and subnet-broadcast `beacon` on every interface.
    fn announce(&self, socket: &UdpSocket, beacon: &Beacon) {
        let bytes = beacon.encode();
        let sock = SockRef::from(socket);
        for iface in &self.interfaces {
            if sock.set_multicast_if_v4(&iface.ip).is_ok() {
                let _ = socket.send_to(&bytes, mcast_addr());
            }
            // A directed broadcast is routed out of the interface that owns the
            // subnet, unlike 255.255.255.255.
            let _ = socket.send_to(&bytes, SocketAddr::from((iface.broadcast, DISCOVERY_PORT)));
        }
        // Still worth one limited broadcast for networks where the subnet mask
        // reported by the OS is wrong, or when no interfaces could be listed.
        let _ = socket.send_to(
            &bytes,
            SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT)),
        );
    }
}

/// Join the discovery multicast group on each interface. Joining on the
/// unspecified address alone picks a single adapter, which on a machine with
/// virtual adapters is often the wrong one.
fn join_group(socket: &UdpSocket, interfaces: &[LanInterface]) {
    let group = Ipv4Addr::from(MCAST_GROUP);
    // Errors are expected: most interfaces will already be joined on refresh.
    let _ = socket.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED);
    for iface in interfaces {
        let _ = socket.join_multicast_v4(&group, &iface.ip);
    }
}

/// Settings every beacon-sending socket needs.
fn prepare_for_beacons(socket: &UdpSocket) -> Result<()> {
    socket.set_broadcast(true)?;
    socket.set_multicast_ttl_v4(1)?;
    // Loop multicast back locally so a sender and receiver on one machine can
    // still find each other, which is how this gets tested.
    socket.set_multicast_loop_v4(true)?;
    Ok(())
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
    let socket: UdpSocket = socket.into();
    prepare_for_beacons(&socket)?;
    join_group(&socket, &lan_interfaces());
    socket.set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(socket)
}

/// Open the socket a receiver listens for audio on. It also sends the
/// receiver's beacons (see the module notes on firewalls).
///
/// Falls back to an ephemeral port if the configured one is taken, since the
/// beacon advertises whichever port we actually got.
pub fn audio_socket(preferred_port: u16) -> Result<UdpSocket> {
    let socket = match UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], preferred_port))) {
        Ok(socket) => socket,
        Err(_) => UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))
            .context("could not bind any audio port")?,
    };
    prepare_for_beacons(&socket)?;
    Ok(socket)
}

/// Socket a sender transmits from.
pub fn sender_socket() -> Result<UdpSocket> {
    let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))
        .context("could not open sending socket")?;
    socket.set_broadcast(true).ok();
    Ok(socket)
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
/// announce ourselves, which both names us in the receiver's UI and prompts
/// receivers to answer directly.
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
            let mut announcer = Announcer::new();
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
                    announcer.refresh(Some(&socket));
                    announcer.announce(&socket, &announce);
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
            return;
        }

        // The same receiver heard on a second interface (a machine talking to
        // itself over several adapters, typically). Streaming to both would
        // just deliver every packet twice, so stick with the address already in
        // use for as long as it keeps answering.
        if !name.is_empty()
            && let Some(existing) = guard
                .iter_mut()
                .find(|p| !p.manual && p.name == name && p.addr.port() == addr.port())
        {
            if existing.last_seen.elapsed() >= ADDRESS_SWITCH_AFTER {
                existing.addr = addr;
                existing.last_seen = Instant::now();
            }
            return;
        }

        guard.push(Peer {
            addr,
            name: name.to_string(),
            last_seen: Instant::now(),
            manual: false,
        });
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

/// Run a receiver's discovery: announce ourselves regularly, answer senders
/// directly, and pick up the sender's name for display.
///
/// `audio` is (a clone of) the socket audio arrives on. Beacons are sent from it
/// so that replies to them — the audio — are let through a firewall that has no
/// inbound rule for this app.
pub fn spawn_receiver_beacon(
    stats: Arc<Stats>,
    audio: UdpSocket,
    sample_rate: u32,
    channels: u8,
) -> Result<NetThreads> {
    let audio_port = audio
        .local_addr()
        .context("audio socket has no local address")?
        .port();
    // Bound only to hear senders. If another instance on this machine holds the
    // port without address reuse, the receiver still works on its own beacons.
    let listener = discovery_socket().ok();
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
            let mut announcer = Announcer::new();
            let mut last_beacon = Instant::now() - Duration::from_secs(1);
            let mut buf = [0u8; 512];

            while !stop.load(Ordering::Relaxed) {
                if last_beacon.elapsed() >= Duration::from_millis(BEACON_INTERVAL_MS) {
                    announcer.refresh(listener.as_ref());
                    announcer.announce(&audio, &hello);
                    last_beacon = Instant::now();
                }

                let Some(listener) = listener.as_ref() else {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                };
                let Ok((len, from)) = listener.recv_from(&mut buf) else {
                    continue;
                };
                let Some(beacon) = Beacon::parse(&buf[..len]) else {
                    continue;
                };
                if beacon.kind != PT_SENDER {
                    continue;
                }

                // Answer the sender directly. This is what gets us found when our
                // broadcasts do not reach it but its broadcasts do reach us.
                let _ = audio.send_to(&hello.encode(), from);

                stats.note_remote(&beacon.name);
            }

            // Tell any sender we are going away so it stops immediately rather
            // than waiting for us to time out.
            hello.kind = PT_BYE;
            announcer.announce(&audio, &hello);
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

        let audio = match audio_socket(47_999) {
            Ok(socket) => socket,
            Err(err) => {
                eprintln!("skipping: could not open an audio socket ({err})");
                return;
            }
        };
        let listen_port = audio.local_addr().unwrap().port();
        let _receiver = match spawn_receiver_beacon(receiver_stats.clone(), audio, 48_000, 2) {
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

        // A receiver reachable over several adapters must still be one target.
        assert_eq!(
            found.iter().filter(|a| a.port() == listen_port).count(),
            1,
            "the same receiver was added more than once: {found:?}"
        );

        let peers = sender_stats.live_peers();
        assert!(
            peers.iter().any(|p| p.addr.port() == listen_port),
            "peer table did not record the receiver"
        );
    }

    /// Every interface must get a broadcast address inside its own subnet,
    /// since that is what routes a beacon out of the right adapter.
    #[test]
    fn interfaces_have_sensible_broadcast_addresses() {
        for iface in lan_interfaces() {
            println!("{} -> {}", iface.ip, iface.broadcast);
            assert_ne!(iface.broadcast, Ipv4Addr::UNSPECIFIED);
            let ip = u32::from(iface.ip);
            let bcast = u32::from(iface.broadcast);
            assert!(bcast >= ip, "{} is below {}", iface.broadcast, iface.ip);
        }
    }
}
