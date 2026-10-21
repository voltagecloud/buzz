//! Network reachability, as the browser's `online` event reports it to
//! buzz-app: when a default route appears again, every community showing a
//! saved snapshot reconnects.
//!
//! Sampled every 5 s: on Linux from the kernel's routing tables, elsewhere
//! by asking the system for a route to the internet. Where neither answers
//! the machine counts as always online, so nothing ever fires.

use crate::community::AppState;
use futures::StreamExt;
use gpui_kit::App;
use std::time::Duration;

const SAMPLE_EVERY: Duration = Duration::from_secs(5);

pub fn init(cx: &mut App) {
    let (sender, mut changes) = futures::channel::mpsc::unbounded();
    let started = std::thread::Builder::new()
        .name("buzz-network".into())
        .spawn(move || {
            let mut online = reachable().unwrap_or(true);
            loop {
                std::thread::sleep(SAMPLE_EVERY);
                let now = reachable().unwrap_or(true);
                if now && !online && sender.unbounded_send(()).is_err() {
                    return;
                }
                online = now;
            }
        });
    if let Err(error) = started {
        tracing::warn!(%error, "could not start the network monitor");
        return;
    }
    cx.spawn(async move |cx| {
        while changes.next().await.is_some() {
            cx.update(|cx| {
                for community in cx.global::<AppState>().communities.clone() {
                    community.read(cx).session.online();
                }
            });
        }
    })
    .detach();
}

/// Whether a default route exists, over IPv4 or IPv6.
#[cfg(target_os = "linux")]
fn reachable() -> Option<bool> {
    let v4 = std::fs::read_to_string("/proc/net/route").ok();
    let v6 = std::fs::read_to_string("/proc/net/ipv6_route").ok();
    if v4.is_none() && v6.is_none() {
        return None;
    }
    Some(v4.as_deref().is_some_and(default_v4) || v6.as_deref().is_some_and(default_v6))
}

/// Whether a route to the internet exists, over IPv4 or IPv6. Connecting
/// a UDP socket sends nothing: the system only picks the route, and
/// refuses when it has none. The addresses are reserved for documentation,
/// so only a default route reaches them.
#[cfg(not(target_os = "linux"))]
fn reachable() -> Option<bool> {
    use std::net::UdpSocket;
    let routed = |local: &str, remote: &str| {
        UdpSocket::bind(local).and_then(|socket| socket.connect(remote))
    };
    Some(routed("0.0.0.0:0", "192.0.2.1:9").is_ok() || routed("[::]:0", "[2001:db8::1]:9").is_ok())
}

/// `/proc/net/route`: a line with destination 0 and the up flag.
#[cfg(any(target_os = "linux", test))]
fn default_v4(table: &str) -> bool {
    table.lines().skip(1).any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields.len() > 3
            && fields[1] == "00000000"
            && u32::from_str_radix(fields[3], 16).is_ok_and(|flags| flags & 1 != 0)
    })
}

/// `/proc/net/ipv6_route`: `::/0` on any interface but loopback.
#[cfg(any(target_os = "linux", test))]
fn default_v6(table: &str) -> bool {
    table.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        fields.len() == 10
            && fields[0].bytes().all(|b| b == b'0')
            && fields[1] == "00"
            && fields[9] != "lo"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_default_routes() {
        let v4 = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
                  eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\n";
        assert!(default_v4(v4));
        let local = "Iface\tDestination\tGateway\tFlags\n eth0\t0001A8C0\t00000000\t0001\n";
        assert!(!default_v4(local));
        let v6 = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 \
                  fe800000000000000000000000000001 00000400 00000001 00000000 00000003 wlan0\n";
        assert!(default_v6(v6));
        assert!(!default_v6(&v6.replace("wlan0", "lo")));
    }
}
