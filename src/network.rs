//! What a VM may connect to: the `network` setting, the destinations that
//! `network-allow` lets through anyway, and the nftables rules that apply
//! them in the guest.
//!
//! The guest enforces them, as root: the rules only filter connections out
//! of its network card (`en*`), made by the VM or forwarded for containers
//! in it, and leave replies to connections from outside alone, such as
//! `sendit ssh` and servers in the VM that the Mac reaches. Base images load
//! a ruleset that only lets DHCP and IPv6's neighbour and router discovery
//! out before the network comes up, and `network.sh` replaces it with
//! `ruleset` once the network is up, so the VM stays without one if
//! anything goes wrong on the way.

use std::fmt::{self, Write as _};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde::Deserialize;

/// The `network` setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Everything: the internet, the local network, the Mac and other VMs
    Open,
    /// The internet, but not the local network, the Mac or other VMs
    #[default]
    Internet,
    /// Nothing but what --network-allow lets through
    #[serde(rename = "none")]
    #[value(name = "none")]
    Offline,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Open => "open",
            Self::Internet => "internet",
            Self::Offline => "none",
        })
    }
}

/// An IPv4 or IPv6 network, or with the full prefix length, an address.
/// Its host bits are zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Net {
    V4(Ipv4Addr, u8),
    V6(Ipv6Addr, u8),
}

impl Net {
    /// The network of `len` bits that `addr` is in.
    fn v4(addr: Ipv4Addr, len: u8) -> Self {
        Self::V4(Ipv4Addr::from(u32::from(addr) & mask32(len)), len)
    }

    fn v6(addr: Ipv6Addr, len: u8) -> Self {
        Self::V6(Ipv6Addr::from(u128::from(addr) & mask128(len)), len)
    }
}

fn mask32(len: u8) -> u32 {
    u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0)
}

fn mask128(len: u8) -> u128 {
    u128::MAX.checked_shl(128 - u32::from(len)).unwrap_or(0)
}

impl FromStr for Net {
    type Err = anyhow::Error;

    /// `ADDR` or `ADDR/LEN`, for IPv4 or IPv6, without host bits.
    fn from_str(s: &str) -> Result<Self> {
        let (addr, len) = match s.split_once('/') {
            Some((addr, len)) => (addr, Some(len)),
            None => (s, None),
        };
        let parse_len = |max: u8| -> Result<u8> {
            let Some(len) = len else { return Ok(max) };
            len.parse()
                .ok()
                .filter(|len| *len <= max)
                .with_context(|| format!("invalid prefix length in {s:?}: expected 0 to {max}"))
        };
        let (net, exact) = if let Ok(v4) = addr.parse::<Ipv4Addr>() {
            let net = Self::v4(v4, parse_len(32)?);
            (net, matches!(net, Self::V4(a, _) if a == v4))
        } else if let Ok(v6) = addr.parse::<Ipv6Addr>() {
            let net = Self::v6(v6, parse_len(128)?);
            (net, matches!(net, Self::V6(a, _) if a == v6))
        } else {
            bail!("invalid address {s:?}: expected an IPv4 or IPv6 address");
        };
        ensure!(
            exact,
            "{s:?} has bits set past its prefix length; the network is {net}"
        );
        Ok(net)
    }
}

/// `ADDR/LEN`, or just `ADDR` for a single address.
impl fmt::Display for Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::V4(addr, 32) => write!(f, "{addr}"),
            Self::V4(addr, len) => write!(f, "{addr}/{len}"),
            Self::V6(addr, 128) => write!(f, "{addr}"),
            Self::V6(addr, len) => write!(f, "{addr}/{len}"),
        }
    }
}

/// Where a `network-allow` entry lets the VM connect to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The Mac, as `host.sendit.internal` (the NAT's gateway).
    Host,
    Net(Net),
}

/// A `network-allow` entry: `host[:PORT]`, `ADDR[/LEN][:PORT]` for IPv4,
/// `IPV6[/LEN]` or `[IPV6[/LEN]]:PORT`. Without a port, every port of the
/// destination, TCP and UDP alike; with one, that port over TCP and UDP.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Allow {
    pub target: Target,
    pub port: Option<u16>,
}

impl FromStr for Allow {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let expected = "expected host[:PORT], ADDR[/LEN][:PORT], or for IPv6, \
                        IPV6[/LEN] or [IPV6[/LEN]]:PORT";
        let (target, port) = if let Some(rest) = s.strip_prefix('[') {
            let (net, port) = rest
                .split_once(']')
                .with_context(|| format!("invalid destination {s:?}: no ] after ["))?;
            let port = match port {
                "" => None,
                port => Some(port.strip_prefix(':').with_context(|| {
                    format!("invalid destination {s:?}: expected :PORT after ]")
                })?),
            };
            let net: Net = net
                .parse()
                .with_context(|| format!("invalid destination {s:?}"))?;
            ensure!(
                matches!(net, Net::V6(..)),
                "invalid destination {s:?}: only IPv6 addresses go in brackets"
            );
            (Target::Net(net), port)
        } else {
            // More than one colon: IPv6, without a port.
            let (target, port) = match s.matches(':').count() {
                0 | 1 => match s.split_once(':') {
                    Some((target, port)) => (target, Some(port)),
                    None => (s, None),
                },
                _ => (s, None),
            };
            let target = match target {
                "host" => Target::Host,
                "" => bail!("invalid destination {s:?}: {expected}"),
                net => Target::Net(
                    net.parse()
                        .with_context(|| format!("invalid destination {s:?}: {expected}"))?,
                ),
            };
            (target, port)
        };
        let port = port
            .map(|port| {
                port.parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .with_context(|| {
                        format!("invalid port in {s:?}: expected a number from 1 to 65535")
                    })
            })
            .transpose()?;
        Ok(Self { target, port })
    }
}

impl TryFrom<String> for Allow {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

/// As `FromStr` reads it.
impl fmt::Display for Allow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.target, self.port) {
            (Target::Host, None) => f.write_str("host"),
            (Target::Host, Some(port)) => write!(f, "host:{port}"),
            (Target::Net(net), None) => write!(f, "{net}"),
            (Target::Net(net @ Net::V4(..)), Some(port)) => write!(f, "{net}:{port}"),
            (Target::Net(net @ Net::V6(..)), Some(port)) => write!(f, "[{net}]:{port}"),
        }
    }
}

/// The resolved network settings of a VM.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Network {
    pub mode: Mode,
    /// In the order the layers gave them, without repeats.
    pub allow: Vec<Allow>,
}

impl Network {
    /// What the VM may connect to, in words, e.g. for `sendit status`.
    pub fn describe(&self) -> String {
        let what = match self.mode {
            Mode::Open => "everything, the local network and the Mac included",
            Mode::Internet => "the internet, but not the local network, the Mac or other VMs",
            Mode::Offline => "nothing",
        };
        let mut text = format!("{} ({what}", self.mode);
        if !self.allow.is_empty() {
            let allow: Vec<_> = self.allow.iter().map(Allow::to_string).collect();
            let allow = allow.join(", ");
            if self.mode == Mode::Open {
                let _ = write!(text, "; network-allow has no effect: {allow}");
            } else {
                let _ = write!(text, "; also allows {allow}");
            }
        }
        text.push(')');
        text
    }
}

/// What `internet` blocks of IPv4, besides the Mac's own networks: this
/// network, private networks (RFC 1918), shared address space (RFC 6598,
/// which Tailscale uses), loopback, link-local addresses, IETF protocol
/// assignments, multicast, and the reserved block with the broadcast
/// address. Not 198.18.0.0/15: proxies on the Mac that hand out fake
/// addresses for the names they resolve use it for the internet.
const LOCAL_V4: [(Ipv4Addr, u8); 10] = [
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];

/// The same for IPv6: the unspecified and loopback addresses, unique local
/// addresses (which the Mac's NAT and Tailscale use), the old site-local
/// ones, link-local addresses (other VMs on the NAT's network included) and
/// multicast.
const LOCAL_V6: [(Ipv6Addr, u8); 6] = [
    (Ipv6Addr::UNSPECIFIED, 128),
    (Ipv6Addr::LOCALHOST, 128),
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
];

/// Where proxies on the Mac that answer DNS with fake addresses put them,
/// and their own interface: never blocked as one of the Mac's networks,
/// since the VM reaches the internet through them there.
const FAKE_IP_V4: (Ipv4Addr, u8) = (Ipv4Addr::new(198, 18, 0, 0), 15);

/// The well-known NAT64 prefix (RFC 6052), through which an IPv6-only
/// network can reach IPv4 addresses, private ones included, and the one for
/// local use (RFC 8215).
const NAT64_V6: (Ipv6Addr, u8) = (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96);
const LOCAL_NAT64_V6: (Ipv6Addr, u8) = (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48);

/// The first line of the ruleset, which `network.sh` prints on boot.
const SUMMARY_PREFIX: &str = "# sendit-network: ";

/// The nftables ruleset that applies `network` in the guest, given the
/// networks of the Mac's own interfaces (see `host_networks`), which
/// `internet` blocks too. `network.sh` adds the NAT's gateway, the Mac, to
/// the `gateway4` and `gateway6` sets: DNS goes to it, and `host` in
/// `network-allow` means it.
///
/// The ruleset replaces the `sendit` table as a whole, in one transaction,
/// and leaves other tables alone, e.g. Docker's or Tailscale's. Blocked TCP
/// connections are reset, so they fail at once with "Connection refused":
/// over IPv6, Linux takes an ICMP error for a connection attempt as a hint
/// and keeps trying for minutes. Others get an ICMP error, which fails
/// sending over UDP at once with "Operation not permitted". Multicast, such
/// as the kernel's IGMP reports and LLMNR, is dropped without logging, so
/// that it doesn't use up the log's burst.
pub fn ruleset(network: &Network, host_nets: &[Net]) -> String {
    let mut text = format!("{SUMMARY_PREFIX}{}\n", network.describe());
    text.push_str(
        "# Generated by sendit; applied by network.sh on boot.\n\
         table inet sendit\n\
         delete table inet sendit\n\
         table inet sendit {\n\
         \tset gateway4 { type ipv4_addr; }\n\
         \tset gateway6 { type ipv6_addr; }\n",
    );
    if network.mode == Mode::Open {
        text.push_str("}\n");
        return text;
    }
    if network.mode == Mode::Internet {
        let (v4, v6) = local_nets(host_nets);
        set(&mut text, "local4", "ipv4_addr", &v4);
        set(&mut text, "local6", "ipv6_addr", &v6);
    }
    text.push_str(
        "\tchain output {\n\
         \t\ttype filter hook output priority filter; policy accept;\n\
         \t\toifname \"en*\" jump outbound\n\
         \t}\n\
         \tchain forward {\n\
         \t\ttype filter hook forward priority filter; policy accept;\n\
         \t\toifname \"en*\" jump outbound\n\
         \t}\n\
         \tchain outbound {\n\
         \t\tct state established,related accept\n\
         \t\tudp sport 68 udp dport 67 accept\n\
         \t\tudp sport 546 udp dport 547 accept\n\
         \t\ticmpv6 type { nd-router-solicit, nd-neighbor-solicit, nd-neighbor-advert, \
         mld-listener-report, mld2-listener-report } accept\n",
    );
    if network.mode == Mode::Internet {
        text.push_str(
            "\t\tip daddr @gateway4 meta l4proto { tcp, udp } th dport 53 accept\n\
             \t\tip6 daddr @gateway6 meta l4proto { tcp, udp } th dport 53 accept\n",
        );
    }
    for allow in &network.allow {
        let port = match allow.port {
            Some(port) => format!(" meta l4proto {{ tcp, udp }} th dport {port}"),
            None => String::new(),
        };
        match allow.target {
            Target::Host => {
                let _ = writeln!(text, "\t\tip daddr @gateway4{port} accept");
                let _ = writeln!(text, "\t\tip6 daddr @gateway6{port} accept");
            }
            Target::Net(net @ Net::V4(..)) => {
                let _ = writeln!(text, "\t\tip daddr {net}{port} accept");
            }
            Target::Net(net @ Net::V6(..)) => {
                let _ = writeln!(text, "\t\tip6 daddr {net}{port} accept");
            }
        }
    }
    match network.mode {
        Mode::Internet => text.push_str(
            "\t\tip daddr @local4 jump blocked\n\
             \t\tip6 daddr @local6 jump blocked\n",
        ),
        _ => text.push_str("\t\tjump blocked\n"),
    }
    text.push_str(
        "\t}\n\
         \tchain blocked {\n\
         \t\tip daddr 224.0.0.0/4 drop\n\
         \t\tip6 daddr ff00::/8 drop\n\
         \t\tlimit rate 10/minute burst 5 packets log prefix \"sendit-network: blocked \" level info\n\
         \t\tmeta l4proto tcp reject with tcp reset\n\
         \t\treject with icmpx admin-prohibited\n\
         \t}\n\
         }\n",
    );
    text
}

/// Appends the declaration of a set of networks, which must not be empty.
fn set(text: &mut String, name: &str, kind: &str, nets: &[Net]) {
    let nets: Vec<_> = nets.iter().map(Net::to_string).collect();
    let _ = writeln!(
        text,
        "\tset {name} {{\n\t\ttype {kind}; flags interval; auto-merge;\n\t\telements = {{ {} }}\n\t}}",
        nets.join(", ")
    );
}

/// What `internet` blocks: `LOCAL_V4` and `LOCAL_V6`, and `host_nets`,
/// and the IPv4 ones also as NAT64 would reach them, along with the NAT64
/// prefix for local use.
fn local_nets(host_nets: &[Net]) -> (Vec<Net>, Vec<Net>) {
    let mut v4: Vec<_> = LOCAL_V4.iter().map(|&(a, len)| Net::v4(a, len)).collect();
    let mut v6: Vec<_> = LOCAL_V6.iter().map(|&(a, len)| Net::v6(a, len)).collect();
    for net in host_nets {
        match net {
            Net::V4(..) => v4.push(*net),
            Net::V6(..) => v6.push(*net),
        }
    }
    v6.push(Net::v6(LOCAL_NAT64_V6.0, LOCAL_NAT64_V6.1));
    let prefix = u128::from(NAT64_V6.0);
    for net in &v4 {
        if let Net::V4(addr, len) = *net {
            let mapped = prefix | u128::from(u32::from(addr));
            v6.push(Net::v6(Ipv6Addr::from(mapped), NAT64_V6.1 + len));
        }
    }
    v4.sort();
    v4.dedup();
    v6.sort();
    v6.dedup();
    (v4, v6)
}

/// The network that an interface of the Mac with `addr` and a prefix of
/// `len` bits is on, which the VM reaches through the Mac: the local
/// network, or that of a VPN. For a prefix shorter than /8 for IPv4 or /32
/// for IPv6, which no local network has, just the address, so that a
/// strange netmask can't block much of the internet.
pub fn host_net(addr: std::net::IpAddr, len: u8) -> Net {
    match addr {
        std::net::IpAddr::V4(addr) if (8..=32).contains(&len) => Net::v4(addr, len),
        std::net::IpAddr::V4(addr) => Net::V4(addr, 32),
        std::net::IpAddr::V6(addr) if (32..=128).contains(&len) => Net::v6(addr, len),
        std::net::IpAddr::V6(addr) => Net::V6(addr, 128),
    }
}

/// The networks of the Mac's interfaces other than loopback (see
/// `host_net`), as they are now: a VM keeps those of its boot. Empty if
/// they can't be listed; the private ranges are blocked anyway.
pub fn host_networks() -> Vec<Net> {
    let mut nets = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills in `list`, which freeifaddrs frees below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return nets;
    }
    let mut next = list;
    while !next.is_null() {
        // SAFETY: an entry of the list, which stays valid until it is freed.
        let entry = unsafe { &*next };
        next = entry.ifa_next;
        if entry.ifa_flags & libc::IFF_LOOPBACK as libc::c_uint != 0 {
            continue;
        }
        // SAFETY: the entry's sockaddrs, null or valid until freed.
        let Some(addr) = (unsafe { sockaddr_ip(entry.ifa_addr) }) else {
            continue;
        };
        // SAFETY: as above.
        let len = unsafe { mask_len(entry.ifa_netmask, addr) };
        let net = host_net(addr, len);
        if !overlaps(net, Net::v4(FAKE_IP_V4.0, FAKE_IP_V4.1)) {
            nets.push(net);
        }
    }
    // SAFETY: from getifaddrs, and freed once.
    unsafe { libc::freeifaddrs(list) };
    nets.sort();
    nets.dedup();
    nets
}

/// Whether `a` and `b` share any address: one contains the other.
fn overlaps(a: Net, b: Net) -> bool {
    match (a, b) {
        (Net::V4(a, a_len), Net::V4(b, b_len)) => {
            let len = a_len.min(b_len);
            Net::v4(a, len) == Net::v4(b, len)
        }
        (Net::V6(a, a_len), Net::V6(b, b_len)) => {
            let len = a_len.min(b_len);
            Net::v6(a, len) == Net::v6(b, len)
        }
        _ => false,
    }
}

/// The IP address in `sa`, if it holds one.
///
/// # Safety
/// `sa` is null or points to a sockaddr as large as its family's.
unsafe fn sockaddr_ip(sa: *const libc::sockaddr) -> Option<std::net::IpAddr> {
    if sa.is_null() {
        return None;
    }
    // SAFETY: the caller's.
    let family = libc::c_int::from(unsafe { (*sa).sa_family });
    match family {
        libc::AF_INET => {
            // SAFETY: an AF_INET sockaddr is a sockaddr_in.
            let sin = unsafe { std::ptr::read_unaligned(sa.cast::<libc::sockaddr_in>()) };
            Some(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)).into())
        }
        libc::AF_INET6 => {
            // SAFETY: an AF_INET6 sockaddr is a sockaddr_in6.
            let sin6 = unsafe { std::ptr::read_unaligned(sa.cast::<libc::sockaddr_in6>()) };
            Some(Ipv6Addr::from(sin6.sin6_addr.s6_addr).into())
        }
        _ => None,
    }
}

/// The prefix length of the netmask `mask` of `addr`, or the full length
/// if it has none or isn't contiguous. macOS may leave out the family of a
/// netmask, and trailing zero bytes of it, which its `sa_len` then doesn't
/// count; the address's family tells how to read it.
///
/// # Safety
/// `mask` is null or points to a sockaddr of `sa_len` bytes on macOS, or
/// as large as `addr`'s family's elsewhere.
unsafe fn mask_len(mask: *const libc::sockaddr, addr: std::net::IpAddr) -> u8 {
    let (offset, size, full) = match addr {
        std::net::IpAddr::V4(_) => (
            std::mem::offset_of!(libc::sockaddr_in, sin_addr),
            std::mem::size_of::<libc::sockaddr_in>(),
            32,
        ),
        std::net::IpAddr::V6(_) => (
            std::mem::offset_of!(libc::sockaddr_in6, sin6_addr),
            std::mem::size_of::<libc::sockaddr_in6>(),
            128,
        ),
    };
    if mask.is_null() {
        return full;
    }
    #[cfg(target_os = "macos")]
    // SAFETY: the caller's.
    let size = size.min(usize::from(unsafe { (*mask).sa_len }));
    let bytes = full as usize / 8;
    let mut bits = Vec::with_capacity(bytes);
    for i in offset..offset + bytes {
        // SAFETY: within the sockaddr's `size` bytes; past them, zero.
        bits.push(if i < size {
            unsafe { *mask.cast::<u8>().add(i) }
        } else {
            0
        });
    }
    prefix_len(&bits).unwrap_or(full)
}

/// The number of leading one bits of a netmask, if all others are zero.
fn prefix_len(mask: &[u8]) -> Option<u8> {
    let mut len = 0u8;
    let mut ended = false;
    for &byte in mask {
        if ended {
            if byte != 0 {
                return None;
            }
            continue;
        }
        let ones = byte.leading_ones();
        if ones < 8 && byte << ones != 0 {
            return None;
        }
        len += ones as u8;
        ended = ones < 8;
    }
    Some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(s: &str) -> Allow {
        s.parse().unwrap_or_else(|e| panic!("{s}: {e:#}"))
    }

    #[test]
    fn parses_modes() {
        let parse = |s: &str| Mode::from_str(s, false);
        assert_eq!(parse("open"), Ok(Mode::Open));
        assert_eq!(parse("internet"), Ok(Mode::Internet));
        assert_eq!(parse("none"), Ok(Mode::Offline));
        assert!(parse("offline").is_err());
        assert_eq!(Mode::default(), Mode::Internet);
        for mode in Mode::value_variants() {
            assert_eq!(parse(&mode.to_string()), Ok(*mode));
        }
    }

    #[test]
    fn parses_networks() {
        let net = |s: &str| s.parse::<Net>().unwrap();
        assert_eq!(net("10.0.0.0/8"), Net::V4(Ipv4Addr::new(10, 0, 0, 0), 8));
        assert_eq!(
            net("192.168.1.20"),
            Net::V4(Ipv4Addr::new(192, 168, 1, 20), 32)
        );
        assert_eq!(net("0.0.0.0/0"), Net::V4(Ipv4Addr::UNSPECIFIED, 0));
        assert_eq!(net("fd00::/8"), Net::V6("fd00::".parse().unwrap(), 8));
        assert_eq!(net("::1"), Net::V6(Ipv6Addr::LOCALHOST, 128));
        assert!("10.0.0.1/8".parse::<Net>().is_err());
        assert!("10.0.0.0/33".parse::<Net>().is_err());
        assert!("fd00::1/8".parse::<Net>().is_err());
        assert!("fd00::/129".parse::<Net>().is_err());
        assert!("10.0.0.0/".parse::<Net>().is_err());
        assert!("example.com".parse::<Net>().is_err());
        let error = "10.1.2.3/16".parse::<Net>().unwrap_err().to_string();
        assert!(error.contains("10.1.0.0/16"), "{error}");
    }

    #[test]
    fn parses_allowed_destinations() {
        let v4 = |a, b, c, d, len| Target::Net(Net::V4(Ipv4Addr::new(a, b, c, d), len));
        let v6 = |s: &str, len| Target::Net(Net::V6(s.parse().unwrap(), len));
        let cases = [
            ("host", Target::Host, None),
            ("host:11434", Target::Host, Some(11434)),
            ("192.168.1.20", v4(192, 168, 1, 20, 32), None),
            ("192.168.1.20:5432", v4(192, 168, 1, 20, 32), Some(5432)),
            ("10.20.0.0/16", v4(10, 20, 0, 0, 16), None),
            ("10.20.0.0/16:443", v4(10, 20, 0, 0, 16), Some(443)),
            ("fd00::1", v6("fd00::1", 128), None),
            ("fd00::/8", v6("fd00::", 8), None),
            ("[fd00::1]:80", v6("fd00::1", 128), Some(80)),
            ("[fd00::/8]:80", v6("fd00::", 8), Some(80)),
            ("[fd00::1]", v6("fd00::1", 128), None),
        ];
        for (s, target, port) in cases {
            let parsed = allow(s);
            assert_eq!(parsed, Allow { target, port }, "{s}");
            // Written out as read.
            assert_eq!(allow(&parsed.to_string()), parsed, "{s}");
        }
        assert_eq!(allow("[fd00::1]").to_string(), "fd00::1");
        for bad in [
            "",
            ":80",
            "host:",
            "host:0",
            "host:65536",
            "host:http",
            "localhost",
            "example.com:443",
            "10.0.0.1/8",
            "[10.0.0.1]:80",
            "[fd00::1]80",
            "[fd00::1",
            "fd00::1:80:x",
        ] {
            assert!(bad.parse::<Allow>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn describes_the_network() {
        let network = |mode, allow: &[&str]| Network {
            mode,
            allow: allow.iter().map(|s| s.parse().unwrap()).collect(),
        };
        assert_eq!(
            network(Mode::Internet, &[]).describe(),
            "internet (the internet, but not the local network, the Mac or other VMs)"
        );
        assert_eq!(
            network(Mode::Offline, &["host:11434", "[fd00::1]:80"]).describe(),
            "none (nothing; also allows host:11434, [fd00::1]:80)"
        );
        assert!(
            network(Mode::Open, &["host"])
                .describe()
                .contains("network-allow has no effect: host")
        );
    }

    #[test]
    fn writes_rulesets() {
        let host = [
            Net::V4(Ipv4Addr::new(192, 168, 64, 0), 24),
            Net::V4(Ipv4Addr::new(130, 83, 0, 0), 16),
            "2a02:810::/64".parse().unwrap(),
        ];
        let internet = ruleset(
            &Network {
                mode: Mode::Internet,
                allow: vec![
                    allow("host:11434"),
                    allow("[fd00::1]:80"),
                    allow("10.1.0.0/16"),
                ],
            },
            &host,
        );
        assert!(internet.starts_with("# sendit-network: internet ("));
        assert!(
            internet.contains("130.83.0.0/16, 169.254.0.0/16"),
            "{internet}"
        );
        assert!(internet.contains("2a02:810::/64"), "{internet}");
        assert!(internet.contains("th dport 53 accept"));
        assert!(
            internet.contains("ip daddr @gateway4 meta l4proto { tcp, udp } th dport 11434 accept")
        );
        assert!(
            internet.contains("ip6 daddr fd00::1 meta l4proto { tcp, udp } th dport 80 accept")
        );
        assert!(internet.contains("ip daddr 10.1.0.0/16 accept"));
        assert!(internet.contains("ip daddr @local4 jump blocked"));
        // The exceptions come before the blocks.
        assert!(internet.find("10.1.0.0/16 accept") < internet.find("@local4 jump"));

        let none = ruleset(
            &Network {
                mode: Mode::Offline,
                allow: vec![],
            },
            &host,
        );
        assert!(!none.contains("local4"));
        assert!(!none.contains("dport 53"));
        assert!(none.contains("\t\tjump blocked\n"));

        let open = ruleset(
            &Network {
                mode: Mode::Open,
                allow: vec![allow("host")],
            },
            &host,
        );
        assert!(!open.contains("chain"));
        assert!(open.contains("set gateway4"));
    }

    #[test]
    fn blocks_local_networks_through_nat64() {
        let (_, v6) = local_nets(&["130.83.0.0/16".parse().unwrap()]);
        for net in [
            "64:ff9b::c0a8:0/112",
            "64:ff9b::a00:0/104",
            "64:ff9b::8253:0/112",
        ] {
            assert!(v6.contains(&net.parse().unwrap()), "{net}: {v6:?}");
        }
        assert!(v6.contains(&"64:ff9b:1::/48".parse().unwrap()));
        // Not the internet through it.
        assert!(!v6.contains(&"64:ff9b::/96".parse().unwrap()));
    }

    #[test]
    fn tells_overlapping_networks() {
        let net = |s: &str| s.parse::<Net>().unwrap();
        assert!(overlaps(net("198.18.0.0/15"), net("198.18.0.1")));
        assert!(overlaps(net("198.18.0.0/16"), net("198.18.0.0/15")));
        assert!(overlaps(net("0.0.0.0/0"), net("10.0.0.0/8")));
        assert!(!overlaps(net("198.20.0.0/16"), net("198.18.0.0/15")));
        assert!(!overlaps(net("fd00::/8"), net("10.0.0.0/8")));
    }

    #[test]
    fn finds_networks_of_host_interfaces() {
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        assert_eq!(
            host_net(ip("192.168.1.20"), 24),
            "192.168.1.0/24".parse().unwrap()
        );
        assert_eq!(
            host_net(ip("100.90.71.111"), 32),
            "100.90.71.111".parse().unwrap()
        );
        assert_eq!(host_net(ip("8.8.8.8"), 1), "8.8.8.8".parse().unwrap());
        assert_eq!(
            host_net(ip("2a02:810::5"), 64),
            "2a02:810::/64".parse().unwrap()
        );
        assert_eq!(
            host_net(ip("2a02:810::5"), 3),
            "2a02:810::5".parse().unwrap()
        );
    }

    #[test]
    fn reads_prefix_lengths() {
        assert_eq!(prefix_len(&[255, 255, 255, 0]), Some(24));
        assert_eq!(prefix_len(&[255, 255, 240, 0]), Some(20));
        assert_eq!(prefix_len(&[0, 0, 0, 0]), Some(0));
        assert_eq!(prefix_len(&[255; 16]), Some(128));
        assert_eq!(prefix_len(&[255, 0, 255, 0]), None);
        assert_eq!(prefix_len(&[0b1011_0000, 0, 0, 0]), None);
    }

    #[test]
    fn agrees_with_the_guest_on_names() {
        // network.sh fills in the gateway sets of the table that the
        // ruleset and the base image's rules replace.
        let script = include_str!("assets/sendit-network.sh");
        let provision = include_str!("assets/provision.sh");
        let rules = ruleset(&Network::default(), &[]);
        for set in ["gateway4", "gateway6"] {
            assert!(script.contains(&format!("add element inet sendit {set} {{")));
            assert!(rules.contains(&format!("set {set} {{")));
        }
        assert!(rules.contains("\ndelete table inet sendit\n"));
        assert!(provision.contains("\ndelete table inet sendit\n"));
        assert!(rules.starts_with(SUMMARY_PREFIX));
        assert!(script.contains(&format!("s/^{SUMMARY_PREFIX}/sendit-network: /")));
        // The interfaces the rules filter are the ones networkd manages.
        assert!(provision.contains("[Match]\nName=en*\n"));
        assert!(rules.contains("oifname \"en*\" jump outbound"));
    }

    #[test]
    fn lists_networks_of_this_machine() {
        // Whatever they are, not loopback, and with their host bits cleared.
        for net in host_networks() {
            match net {
                Net::V4(addr, len) => {
                    assert!(!addr.is_loopback());
                    assert_eq!(Net::v4(addr, len), net);
                }
                Net::V6(addr, len) => {
                    assert!(!addr.is_loopback());
                    assert_eq!(Net::v6(addr, len), net);
                }
            }
        }
    }
}
