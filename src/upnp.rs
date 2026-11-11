//! Best-effort automatic router port mapping (UPnP IGD), so a Host whose router has UPnP
//! enabled (the common case on consumer routers) doesn't need to touch router settings at
//! all to be reachable from outside their own network. This does NOT help every network —
//! it silently does nothing useful when the router has UPnP disabled, or when the Host's
//! ISP connection itself has no real public IPv4 address (CGNAT), since there is then no
//! external address for the router to even map a port on. Those cases need a manual
//! workaround (port-forward, or a VPN like Tailscale) documented separately; this module
//! just picks up the easy, zero-configuration subset of the problem for free.
//!
//! Runs as a detached, timeout-bounded background task — never blocks or fails startup.

use std::net::SocketAddrV4;
use std::time::Duration;

use igd_next::{
    PortMappingProtocol,
    aio::{Gateway, tokio::Tokio},
};
use tracing::{info, warn};

use crate::config::{Config, PortRange};

// igd-next's own SSDP discovery loop retries for its own default of 10s internally
// (`igd_next::common::options::DEFAULT_TIMEOUT`) before giving up — this wrapper's timeout
// must stay comfortably above that, or it cuts the search off before the router has a real
// chance to answer (which is exactly what a too-short value here looks like: a spurious
// "gateway search timed out" even on a router that actually supports UPnP).
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
/// Routers don't remember a mapping forever; renew well before it would expire.
const LEASE_DURATION_SECS: u32 = 60 * 60;
const RENEW_INTERVAL: Duration = Duration::from_secs(60 * 45);

pub fn spawn_best_effort_mapping(config: &Config) {
    let tcp_port = match config.web_server.bind_address {
        std::net::SocketAddr::V4(addr) => addr.port(),
        std::net::SocketAddr::V6(_) => {
            warn!("web_server.bind_address is IPv6 — UPnP IGD only maps IPv4, skipping");
            return;
        }
    };
    let udp_range = config.webrtc.port_range.clone();

    tokio::spawn(async move {
        run(tcp_port, udp_range).await;
    });
}

async fn run(tcp_port: u16, udp_range: Option<PortRange>) {
    loop {
        map_once(tcp_port, udp_range.as_ref()).await;
        tokio::time::sleep(RENEW_INTERVAL).await;
    }
}

async fn map_once(tcp_port: u16, udp_range: Option<&PortRange>) {
    let Some(local_ip) = local_ipv4() else {
        info!("UPnP: couldn't determine this machine's LAN IPv4 address, skipping");
        return;
    };

    let search_options = igd_next::SearchOptions {
        // igd-next's default binds the search socket to the wildcard 0.0.0.0, which on at
        // least some networks/configs falls through to macOS's catch-all reject route for
        // 224.0.0.0/4 instead of using the specific SSDP multicast group's own route —
        // confirmed by reproducing it with a bare socket: sendto() from a 0.0.0.0-bound
        // socket fails with EHOSTUNREACH ("No route to host"), while the identical send from
        // a socket bound to this machine's own LAN IP succeeds. Binding explicitly avoids
        // that reject route entirely.
        bind_addr: std::net::SocketAddr::V4(SocketAddrV4::new(local_ip, 0)),
        // Let igd-next's own loop govern the real timeout (it retries internally); the outer
        // tokio::time::timeout below is just a hard safety cap in case the library itself
        // ever hangs, not the thing that's supposed to fire in the normal "no router
        // answered" case — if it DOES fire, elapsed_ms logged below will say so explicitly.
        timeout: Some(SEARCH_TIMEOUT),
        ..Default::default()
    };
    let started = std::time::Instant::now();

    let gateway = match tokio::time::timeout(
        SEARCH_TIMEOUT + Duration::from_secs(5),
        igd_next::aio::tokio::search_gateway(search_options),
    )
    .await
    {
        Ok(Ok(gateway)) => gateway,
        Ok(Err(err)) => {
            info!(
                elapsed_ms = started.elapsed().as_millis(),
                "UPnP: no gateway found ({err}) — router likely has UPnP disabled or doesn't \
                 support it. Guests outside your network may need a manual port-forward or a \
                 VPN like Tailscale instead."
            );
            return;
        }
        Err(_) => {
            info!(
                elapsed_ms = started.elapsed().as_millis(),
                "UPnP: gateway search's own safety timeout fired — this means igd-next's \
                 internal loop hung instead of returning its own error, which usually points \
                 to something blocking outbound multicast (an active VPN, or a firewall \
                 dropping UDP 1900/239.255.255.250)"
            );
            return;
        }
    };

    // get_external_ip() is just a liveness check here (confirms the gateway actually
    // answers requests beyond the initial SSDP discovery) — `local_ip` for the mappings
    // themselves was already resolved above, to pick the search's bind address.
    if let Err(err) = gateway.get_external_ip().await {
        info!("UPnP: gateway didn't respond to external IP query ({err}), skipping");
        return;
    }

    map_port(&gateway, PortMappingProtocol::TCP, tcp_port, local_ip, "dawswithfriends-signaling").await;

    if let Some(range) = udp_range {
        for port in range.min..=range.max {
            map_port(&gateway, PortMappingProtocol::UDP, port, local_ip, "dawswithfriends-media").await;
        }
    }
}

async fn map_port(
    gateway: &Gateway<Tokio>,
    protocol: PortMappingProtocol,
    port: u16,
    local_ip: std::net::Ipv4Addr,
    description: &str,
) {
    let local_addr = std::net::SocketAddr::V4(SocketAddrV4::new(local_ip, port));
    match gateway
        .add_port(protocol, port, local_addr, LEASE_DURATION_SECS, description)
        .await
    {
        Ok(()) => info!("UPnP: mapped {protocol:?} port {port} on the router"),
        Err(err) => info!("UPnP: failed to map {protocol:?} port {port}: {err}"),
    }
}

/// The first non-loopback IPv4 address on this machine — same "best guess LAN interface"
/// approach HostApp itself uses (see SessionProcessManager.lanIPAddress()), just on the
/// web-server side where we don't have access to Swift's getifaddrs helper. Also used by
/// `api::stream::webrtc` to bind its UDP socket to this address instead of the wildcard
/// `0.0.0.0` — see that module for why (same reject-route issue this module works around).
pub(crate) fn local_ipv4() -> Option<std::net::Ipv4Addr> {
    use std::net::UdpSocket;

    // Doesn't actually send anything — connecting a UDP socket just asks the OS to pick the
    // local interface/address it would use to reach that destination, which is a common,
    // portable trick for finding "my" outbound LAN IP without parsing interface lists.
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()? {
        std::net::SocketAddr::V4(addr) => Some(*addr.ip()),
        std::net::SocketAddr::V6(_) => None,
    }
}
