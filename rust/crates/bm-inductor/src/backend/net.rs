/// Parse the port out of an API base URL (`http://127.0.0.1:8901` → 8901).
pub fn api_port(api: &str) -> u16 {
    api.trim_end_matches('/')
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8901)
}

/// The host an API base URL points at, lowercased and without port.
fn api_host(api: &str) -> String {
    let s = api.trim();
    let s = s.split("://").nth(1).unwrap_or(s);
    let s = s.split('/').next().unwrap_or(s);
    // `[::1]:8901` → `::1`.
    if let Some((h, _)) = s.rsplit_once("]:") {
        return h.strip_prefix('[').unwrap_or(h).to_lowercase();
    }
    match s.rsplit_once(':') {
        // `host:port` → `host`; a second colon means a bare IPv6 literal.
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !h.contains(':') => {
            h.to_lowercase()
        }
        _ => s.to_lowercase(),
    }
}

/// Whether the TUI's API URL is this machine: only then may a backend be
/// spawned here. Starting a "local" backend while watching a remote inductor
/// would orphan two processes nobody is looking at.
pub(crate) fn api_is_local(api: &str) -> bool {
    matches!(api_host(api).as_str(), "127.0.0.1" | "localhost" | "::1")
}

/// Where the inductor listens: remote workers dial the LAN address, so a
/// cluster backend must leave loopback. Pure so the choice is testable.
pub fn public_bind(has_remotes: bool) -> &'static str {
    if has_remotes {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    }
}

/// This machine's LAN address for remote workers to dial. macOS first
/// (`ipconfig`), then the interface table itself (`ipconfig` stays silent on
/// statically-addressed interfaces — exactly how lab NICs are configured).
pub fn lan_ip() -> anyhow::Result<String> {
    for iface in ["en0", "en1"] {
        if let Some(ip) = iface_ip(iface) {
            return Ok(ip);
        }
    }
    Err(anyhow::anyhow!("no usable address on en0/en1"))
}

/// Parse the dial-back address for one remote: ask the routing table which
/// interface reaches it (`route get` on macOS, `ip route get` on Linux), then
/// read that interface's address. A multi-homed inductor (lab NIC + wifi)
/// must hand each box the address on *its* subnet — the first address found
/// is routinely the wrong one.
pub fn dial_back_ip(remote: &str) -> anyhow::Result<String> {
    let out = std::process::Command::new("route")
        .arg("get")
        .arg(remote)
        .output();
    if let Ok(o) = out {
        let text = String::from_utf8_lossy(&o.stdout).to_string();
        if let Some(iface) = parse_route_iface(&text) {
            if let Some(ip) = iface_ip(&iface) {
                return Ok(ip);
            }
        }
    }
    let out = std::process::Command::new("ip")
        .args(["route", "get", remote])
        .output();
    if let Ok(o) = out {
        let text = String::from_utf8_lossy(&o.stdout).to_string();
        if let Some(ip) = parse_route_src(&text) {
            return Ok(ip);
        }
    }
    lan_ip()
}

fn usable_ipv4(s: &str) -> bool {
    s != "127.0.0.1"
        && s.split('.').count() == 4
        && s.chars().all(|c| c.is_ascii_digit() || c == '.')
        && s.split('.').all(|o| o.parse::<u8>().is_ok())
}

/// Address of one interface: DHCP answer first, interface table second.
fn iface_ip(iface: &str) -> Option<String> {
    let out = std::process::Command::new("ipconfig")
        .args(["getifaddr", iface])
        .output()
        .ok()?;
    let ip = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();
    if usable_ipv4(&ip) {
        return Some(ip);
    }
    let out = std::process::Command::new("ifconfig")
        .arg(iface)
        .output()
        .ok()?;
    parse_ifconfig_inet(&String::from_utf8_lossy(&out.stdout))
}

/// `route get 1.2.3.4` → `interface: en0` → `en0`. macOS.
fn parse_route_iface(out: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let t = l.trim();
        t.strip_prefix("interface:")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

/// `1.2.3.4 via 9.9.9.9 dev eth0 src 10.0.0.5 ...` → `10.0.0.5`. Linux.
fn parse_route_src(out: &str) -> Option<String> {
    let words: Vec<&str> = out.split_whitespace().collect();
    words
        .iter()
        .position(|w| *w == "src")
        .and_then(|i| words.get(i + 1))
        .map(|s| s.to_string())
        .filter(|s| usable_ipv4(s))
}

/// First `inet A.B.C.D` (not `inet6`) in `ifconfig` output.
fn parse_ifconfig_inet(out: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let t = l.trim();
        let rest = t.strip_prefix("inet ")?;
        let ip = rest.split_whitespace().next()?;
        usable_ipv4(ip).then(|| ip.to_string())
    })
}

pub(crate) fn is_local_addr(addr: &str) -> bool {
    matches!(addr, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_port_defaults_when_missing() {
        assert_eq!(api_port("http://127.0.0.1:8901"), 8901);
        assert_eq!(api_port("http://127.0.0.1:8901/"), 8901);
        assert_eq!(api_port("http://example:9999"), 9999);
        assert_eq!(api_port("http://example"), 8901);
    }

    #[test]
    fn only_loopback_counts_as_this_machine() {
        for a in [
            "http://127.0.0.1:8901",
            "http://localhost:8901",
            "http://[::1]:8901",
        ] {
            assert!(api_is_local(a), "{a}");
        }
        for a in ["http://192.168.2.7:8901", "http://example:8901"] {
            assert!(!api_is_local(a), "{a}");
        }
    }

    #[test]
    fn bind_follows_the_cluster_shape() {
        assert_eq!(public_bind(false), "127.0.0.1", "solo stays loopback");
        assert_eq!(public_bind(true), "0.0.0.0", "remotes need the LAN");
    }

    #[test]
    fn dial_back_parsers_read_routing_tables() {
        let mac = "route to: 192.168.2.2\ndestination: 192.168.2.2\n  interface: en0\n      flags: <UP,HOST>\n";
        assert_eq!(parse_route_iface(mac), Some("en0".into()));
        assert_eq!(parse_route_iface("nothing here\n"), None);
        let linux = "192.168.2.2 via 192.168.1.1 dev eth0 src 192.168.1.50 uid 1000\n";
        assert_eq!(parse_route_src(linux), Some("192.168.1.50".into()));
        assert_eq!(parse_route_src("no src here\n"), None);
        let ifc = "en0: flags=8863<UP>\n\tinet 192.168.2.1 netmask 0xffffff00 broadcast 192.168.2.255\n\tinet6 fe80::1%en0\n\tstatus: active\n";
        assert_eq!(parse_ifconfig_inet(ifc), Some("192.168.2.1".into()));
        assert_eq!(
            parse_ifconfig_inet("\tinet 127.0.0.1 netmask 0xff000000\n"),
            None
        );
        assert_eq!(parse_ifconfig_inet("no addresses\n"), None);
    }
}
