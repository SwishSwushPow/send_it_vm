//! Finding a VM's IP address in the leases of macOS's NAT DHCP server.

use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;

use anyhow::{Context, Result};

use super::VmDir;
use crate::util;

pub const LEASES_FILE: &str = "/var/db/dhcpd_leases";

/// The IP address of the VM in `dir`, if it has one yet.
pub fn vm_ip(dir: &VmDir) -> Result<Option<Ipv4Addr>> {
    // Written when the VM first boots.
    let path = dir.mac();
    let mac = util::if_exists(fs::read_to_string(&path))
        .with_context(|| format!("reading {}", path.display()))?;
    match mac {
        Some(mac) => lease_for(mac.trim()),
        None => Ok(None),
    }
}

/// The IP address most recently leased to `mac`, if any.
fn lease_for(mac: &str) -> Result<Option<Ipv4Addr>> {
    let path = Path::new(LEASES_FILE);
    let text = util::if_exists(fs::read_to_string(path))
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(text.and_then(|text| find_lease(&text, mac)))
}

/// Entries look like this, with leading zeros dropped from the MAC's octets:
///
/// ```text
/// {
///     name=sendit
///     ip_address=192.168.64.4
///     hw_address=1,a2:9d:8:3e:f4:ec
///     identifier=1,a2:9d:8:3e:f4:ec
///     lease=0x6ab60865
/// }
/// ```
///
/// `lease` is the expiry time; a VM that rebooted may have several entries.
fn find_lease(text: &str, mac: &str) -> Option<Ipv4Addr> {
    let wanted = parse_mac(mac)?;
    let mut best: Option<(u64, Ipv4Addr)> = None;
    for entry in text.split('}') {
        let field = |name: &str| {
            entry.lines().find_map(|line| {
                let (key, value) = line.trim().split_once('=')?;
                (key == name).then_some(value)
            })
        };
        let Some(hw) = field("hw_address").and_then(|v| v.strip_prefix("1,")) else {
            continue;
        };
        if parse_mac(hw) != Some(wanted) {
            continue;
        }
        let Some(ip) = field("ip_address").and_then(|v| v.parse().ok()) else {
            continue;
        };
        let expiry = field("lease")
            .and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok())
            .unwrap_or(0);
        if best.is_none_or(|(e, _)| expiry >= e) {
            best = Some((expiry, ip));
        }
    }
    best.map(|(_, ip)| ip)
}

fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut mac = [0; 6];
    let mut octets = s.trim().split(':');
    for byte in &mut mac {
        *byte = u8::from_str_radix(octets.next()?, 16).ok()?;
    }
    octets.next().is_none().then_some(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEASES: &str = "\
{
\tname=sendit
\tip_address=192.168.64.7
\thw_address=ff,f1:f5:dd:7f:0:2:0:0:ab:11:f1:e0:82:2f:f9:c8:b:5a
\tidentifier=ff,f1:f5:dd:7f:0:2:0:0:ab:11:f1:e0:82:2f:f9:c8:b:5a
\tlease=0x6ab60aa8
}
{
\tname=sendit
\tip_address=192.168.64.4
\thw_address=1,a2:9d:8:3e:f4:ec
\tidentifier=1,a2:9d:8:3e:f4:ec
\tlease=0x6ab60865
}
{
\tname=sendit
\tip_address=192.168.64.9
\thw_address=1,a2:9d:8:3e:f4:ec
\tidentifier=1,a2:9d:8:3e:f4:ec
\tlease=0x6ab60900
}
{
\tname=other
\tip_address=192.168.64.5
\thw_address=1,2:0:0:0:0:1
\tlease=0x6ab60999
}
";

    #[test]
    fn finds_the_newest_lease_for_a_mac() {
        assert_eq!(
            find_lease(LEASES, "a2:9d:08:3e:f4:ec"),
            Some(Ipv4Addr::new(192, 168, 64, 9))
        );
        assert_eq!(
            find_lease(LEASES, "02:00:00:00:00:01"),
            Some(Ipv4Addr::new(192, 168, 64, 5))
        );
        assert_eq!(find_lease(LEASES, "02:00:00:00:00:02"), None);
        assert_eq!(find_lease("", "02:00:00:00:00:01"), None);
    }

    #[test]
    fn parses_macs() {
        assert_eq!(
            parse_mac("a2:9d:8:3e:f4:ec"),
            Some([0xa2, 0x9d, 8, 0x3e, 0xf4, 0xec])
        );
        assert_eq!(parse_mac("a2:9d:8:3e:f4"), None);
        assert_eq!(parse_mac("a2:9d:8:3e:f4:ec:00"), None);
        assert_eq!(parse_mac("zz:9d:8:3e:f4:ec"), None);
    }
}
