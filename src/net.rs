// Sentinel CameraNode - Camera streaming node for Sentinel Command Center
// Copyright (C) 2026  SourceBox LLC
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//! The LAN address this node advertises.
//!
//! Command Center builds Home Assistant's `local_url` from it, so it has
//! to be an address another machine on the LAN can reach. The old answer
//! was "whichever local address routes to 8.8.8.8", which on a machine
//! running a full-tunnel VPN is the VPN's: a node behind ProtonVPN
//! advertised `10.2.0.2` (its `proton0` address) instead of its real LAN
//! address, and Home Assistant elsewhere on the network could not play.
//!
//! Resolution order:
//! 1. `SOURCEBOX_SENTRY_LAN_IP`, if set to an IP address.
//! 2. The first private IPv4 address (10/8, 172.16/12, 192.168/16) on an
//!    interface that is not loopback, a VPN, a container bridge or a
//!    virtual machine network.
//! 3. The old route-based answer, so nothing that worked before breaks.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

/// The override variable.
pub const ENV: &str = "SOURCEBOX_SENTRY_LAN_IP";

/// Interface-name prefixes that are never the LAN: VPNs, container and
/// VM bridges. Matched case-insensitively.
const VIRTUAL_PREFIXES: &[&str] = &[
    "lo",
    "docker",
    "br-",
    "veth",
    "virbr",
    "vmnet",
    "vboxnet",
    "podman",
    "cni",
    "flannel",
    "cali",
    "tun",
    "tap",
    "wg",
    "proton",
    "tailscale",
    "zt",
    "utun",
    "nordlynx",
    "mullvad",
    "ppp",
    "ipsec",
];

/// The address to advertise, or `None` when the machine has none.
pub fn lan_ip() -> Option<String> {
    if let Some(ip) = std::env::var(ENV)
        .ok()
        .and_then(|v| v.trim().parse::<IpAddr>().ok())
    {
        return Some(ip.to_string());
    }
    let interfaces: Vec<(String, IpAddr)> = if_addrs::get_if_addrs()
        .map(|list| {
            list.into_iter()
                .map(|i| {
                    let ip = i.ip();
                    (i.name, ip)
                })
                .collect()
        })
        .unwrap_or_default();
    choose(&interfaces, route_ip()).map(|ip| ip.to_string())
}

/// The local address the OS would use to reach the internet.
fn route_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

/// Steps 2 and 3, on a given interface list and route answer.
pub fn choose(interfaces: &[(String, IpAddr)], route: Option<IpAddr>) -> Option<IpAddr> {
    let lan = |ip: &IpAddr| matches!(ip, IpAddr::V4(v4) if is_private(v4));
    // The routed address is the best answer when it is itself a LAN
    // address on a real interface: that is the normal, no-VPN case.
    if let Some(routed) = route {
        if interfaces
            .iter()
            .any(|(name, ip)| *ip == routed && !is_virtual(name) && lan(ip))
        {
            return Some(routed);
        }
    }
    interfaces
        .iter()
        .find(|(name, ip)| !is_virtual(name) && lan(ip))
        .map(|(_, ip)| *ip)
        .or(route)
}

fn is_private(ip: &Ipv4Addr) -> bool {
    ip.is_private() && !ip.is_loopback()
}

fn is_virtual(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    VIRTUAL_PREFIXES.iter().any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, ip: &str) -> (String, IpAddr) {
        (name.to_string(), ip.parse().unwrap())
    }

    /// The machine this was found on: ProtonVPN's tunnel holds the
    /// default route, so the old answer was its address.
    #[test]
    fn a_vpn_holding_the_default_route_is_skipped() {
        let interfaces = [
            iface("lo", "127.0.0.1"),
            iface("ens18", "10.0.0.251"),
            iface("br-2ae72049bbf6", "172.18.0.1"),
            iface("docker0", "172.17.0.1"),
            iface("proton0", "10.2.0.2"),
        ];
        let routed = Some("10.2.0.2".parse().unwrap());
        assert_eq!(
            choose(&interfaces, routed),
            Some("10.0.0.251".parse().unwrap())
        );
    }

    #[test]
    fn without_a_vpn_the_routed_address_wins() {
        let interfaces = [
            iface("docker0", "172.17.0.1"),
            iface("eth0", "192.168.1.40"),
            iface("wlan0", "192.168.1.41"),
        ];
        let routed = Some("192.168.1.41".parse().unwrap());
        assert_eq!(choose(&interfaces, routed), routed);
    }

    #[test]
    fn container_and_vpn_interfaces_are_never_chosen() {
        for name in [
            "docker0",
            "br-1234",
            "veth9",
            "wg0",
            "tailscale0",
            "tun0",
            "utun3",
        ] {
            let interfaces = [iface(name, "10.9.9.9")];
            assert_eq!(choose(&interfaces, None), None, "{name}");
        }
    }

    #[test]
    fn with_no_lan_address_the_old_answer_stands() {
        let interfaces = [iface("eth0", "203.0.113.7")];
        let routed = Some("203.0.113.7".parse().unwrap());
        assert_eq!(choose(&interfaces, routed), routed);
    }
}
