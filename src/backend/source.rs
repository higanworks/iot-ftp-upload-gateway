use std::io;
use std::net::Ipv4Addr;

use crate::config::BackendSourceConfig;

/// One IPv4 address configured on a local network interface. An interface with several
/// addresses (e.g. EC2 secondary private IPs) yields several of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceAddr {
    pub interface: String,
    pub ip: Ipv4Addr,
}

/// Lists the host's interface addresses. A trait so tests can supply a fixed set instead of
/// whatever the machine running them happens to have.
pub trait InterfaceLister: Send + Sync {
    fn list(&self) -> io::Result<Vec<InterfaceAddr>>;
}

/// The real thing: `getifaddrs`, IPv4 only (the gateway is IPv4-only). The OS does not report
/// whether an interface is up, so an address that turns out to be unusable is found out when
/// connecting from it fails, not here.
pub struct SystemInterfaces;

impl InterfaceLister for SystemInterfaces {
    fn list(&self) -> io::Result<Vec<InterfaceAddr>> {
        Ok(if_addrs::get_if_addrs()?
            .into_iter()
            .filter_map(|interface| match interface.addr {
                if_addrs::IfAddr::V4(v4) => Some(InterfaceAddr {
                    interface: interface.name,
                    ip: v4.ip,
                }),
                if_addrs::IfAddr::V6(_) => None,
            })
            .collect())
    }
}

/// Interface names that are virtual plumbing (loopback, container bridges, tunnels), never the
/// path to a backend. Skipped when no `include_interfaces` is configured.
const BUILT_IN_VIRTUAL_INTERFACES: &[&str] = &[
    "lo",
    "docker*",
    "br-*",
    "veth*",
    "virbr*",
    "cni*",
    "flannel*",
    "cali*",
    "tun*",
    "tap*",
    "tailscale*",
    "wg*",
];

/// Source addresses to rotate through, given the host's interface addresses.
///
/// - With `include_interfaces`, only addresses on matching interfaces are candidates; that is
///   explicit, so loopback and the built-in virtual names are not filtered out of it.
/// - Without it, every address is a candidate except loopback, link-local, and those on a
///   built-in virtual interface.
/// - Either way, addresses on `exclude_interfaces` are then removed.
///
/// Names accept `*` and `?` wildcards. The result is sorted and free of duplicates, so the
/// rotation order stays the same from one refresh to the next.
pub fn select_sources(addrs: &[InterfaceAddr], config: &BackendSourceConfig) -> Vec<Ipv4Addr> {
    let explicit = !config.include_interfaces.is_empty();
    let mut selected: Vec<Ipv4Addr> = addrs
        .iter()
        .filter(|addr| !addr.ip.is_unspecified())
        .filter(|addr| {
            if explicit {
                matches_any(&config.include_interfaces, &addr.interface)
            } else {
                !addr.ip.is_loopback()
                    && !addr.ip.is_link_local()
                    && !BUILT_IN_VIRTUAL_INTERFACES
                        .iter()
                        .any(|pattern| matches_name(pattern, &addr.interface))
            }
        })
        .filter(|addr| !matches_any(&config.exclude_interfaces, &addr.interface))
        .map(|addr| addr.ip)
        .collect();
    selected.sort();
    selected.dedup();
    selected
}

/// Lists the host's interfaces and picks the source addresses per `config`.
pub fn resolve_sources(
    lister: &dyn InterfaceLister,
    config: &BackendSourceConfig,
) -> io::Result<Vec<Ipv4Addr>> {
    Ok(select_sources(&lister.list()?, config))
}

fn matches_any(patterns: &[String], name: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| matches_name(pattern.trim(), name))
}

/// Whether `pattern` matches the interface `name` an address is reported under, or the interface
/// that name is an alias of.
///
/// Linux reports a labeled alias -- `ip addr add ... label ens5:1`, or a legacy `ifcfg-ens5:1`
/// secondary address, both common ways of putting an EC2 secondary private IP on an ENI -- under
/// the label, `ens5:1`, although the address is on `ens5`. Matching the part before the colon as
/// well means `include_interfaces: [ens5]` covers every address on `ens5`, labeled or not, while
/// a pattern naming the alias itself (`ens5:1`, or `ens5:*`) still singles that one out.
fn matches_name(pattern: &str, name: &str) -> bool {
    glob_match(pattern, name)
        || name
            .split_once(':')
            .is_some_and(|(interface, _alias)| glob_match(pattern, interface))
}

/// Matches `text` against `pattern`, where `*` is any run of characters (including none) and `?`
/// is exactly one. Case-sensitive, like interface names.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    // Where the last `*` was, and how much of `text` it has swallowed so far, to retry with one
    // more character when a later mismatch shows the star should have taken more.
    let mut star: Option<(usize, usize)> = None;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendSourceMode;

    fn addr(interface: &str, ip: [u8; 4]) -> InterfaceAddr {
        InterfaceAddr {
            interface: interface.to_string(),
            ip: Ipv4Addr::from(ip),
        }
    }

    fn config(include: &[&str], exclude: &[&str]) -> BackendSourceConfig {
        BackendSourceConfig {
            mode: BackendSourceMode::Rotate,
            include_interfaces: include.iter().map(|s| s.to_string()).collect(),
            exclude_interfaces: exclude.iter().map(|s| s.to_string()).collect(),
            ..BackendSourceConfig::default()
        }
    }

    fn ip(a: u8, b: u8, c: u8, d: u8) -> Ipv4Addr {
        Ipv4Addr::new(a, b, c, d)
    }

    /// A typical Docker-host-network EC2 instance with two ENIs.
    fn ec2_host() -> Vec<InterfaceAddr> {
        vec![
            addr("lo", [127, 0, 0, 1]),
            addr("ens5", [10, 0, 1, 10]),
            addr("ens6", [10, 0, 2, 20]),
            addr("docker0", [172, 17, 0, 1]),
            addr("br-1a2b3c4d5e6f", [172, 18, 0, 1]),
            addr("veth0a1b2c3", [169, 254, 7, 7]),
        ]
    }

    #[test]
    fn glob_matches_literals_and_wildcards() {
        assert!(glob_match("eth0", "eth0"));
        assert!(!glob_match("eth0", "eth1"));
        assert!(glob_match("eth*", "eth0"));
        assert!(glob_match("eth*", "eth"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
        assert!(glob_match("ens?", "ens5"));
        assert!(!glob_match("ens?", "ens"));
        assert!(!glob_match("ens?", "ens55"));
        assert!(glob_match("br-*", "br-1a2b3c"));
        assert!(glob_match("*0", "eth0"));
        assert!(glob_match("e*h*0", "eth0"));
        assert!(!glob_match("e*h*1", "eth0"));
        assert!(glob_match("a*b*c", "aXbYbZc"), "star must be able to retry");
        assert!(!glob_match("eth0", "ETH0"), "case-sensitive");
        assert!(!glob_match("", "x"));
        assert!(glob_match("", ""));
    }

    #[test]
    fn auto_detection_skips_loopback_link_local_and_virtual_interfaces() {
        let sources = select_sources(&ec2_host(), &config(&[], &[]));
        assert_eq!(sources, [ip(10, 0, 1, 10), ip(10, 0, 2, 20)]);
    }

    #[test]
    fn include_selects_only_matching_interfaces() {
        let sources = select_sources(&ec2_host(), &config(&["ens6"], &[]));
        assert_eq!(sources, [ip(10, 0, 2, 20)]);

        let sources = select_sources(&ec2_host(), &config(&["ens*"], &[]));
        assert_eq!(sources, [ip(10, 0, 1, 10), ip(10, 0, 2, 20)]);
    }

    #[test]
    fn include_is_explicit_so_it_overrides_the_built_in_exclusions() {
        let sources = select_sources(&ec2_host(), &config(&["lo", "docker0"], &[]));
        assert_eq!(sources, [ip(127, 0, 0, 1), ip(172, 17, 0, 1)]);
    }

    #[test]
    fn exclude_removes_interfaces_in_both_modes() {
        let sources = select_sources(&ec2_host(), &config(&[], &["ens6"]));
        assert_eq!(sources, [ip(10, 0, 1, 10)]);

        let sources = select_sources(&ec2_host(), &config(&["ens*"], &["ens5"]));
        assert_eq!(sources, [ip(10, 0, 2, 20)]);
    }

    #[test]
    fn every_address_of_an_interface_is_its_own_source() {
        let host = vec![
            addr("ens5", [10, 0, 1, 10]),
            addr("ens5", [10, 0, 1, 11]),
            addr("ens5", [10, 0, 1, 12]),
        ];
        let sources = select_sources(&host, &config(&[], &[]));
        assert_eq!(
            sources,
            [ip(10, 0, 1, 10), ip(10, 0, 1, 11), ip(10, 0, 1, 12)]
        );
    }

    /// An EC2 instance with one ENI carrying its primary address plus secondary private IPs, some
    /// of them configured as labeled aliases (`ens5:1`) and some unlabeled.
    fn single_eni_with_secondary_ips() -> Vec<InterfaceAddr> {
        vec![
            addr("lo", [127, 0, 0, 1]),
            addr("ens5", [10, 0, 1, 10]),
            addr("ens5", [10, 0, 1, 11]),
            addr("ens5:1", [10, 0, 1, 12]),
            addr("ens5:2", [10, 0, 1, 13]),
            addr("docker0", [172, 17, 0, 1]),
        ]
    }

    #[test]
    fn auto_detection_uses_every_address_on_one_eni_labeled_or_not_in_ascending_order() {
        let sources = select_sources(&single_eni_with_secondary_ips(), &config(&[], &[]));
        assert_eq!(
            sources,
            [
                ip(10, 0, 1, 10),
                ip(10, 0, 1, 11),
                ip(10, 0, 1, 12),
                ip(10, 0, 1, 13)
            ]
        );
    }

    #[test]
    fn including_an_interface_includes_its_labeled_aliases() {
        let sources = select_sources(&single_eni_with_secondary_ips(), &config(&["ens5"], &[]));
        assert_eq!(
            sources,
            [
                ip(10, 0, 1, 10),
                ip(10, 0, 1, 11),
                ip(10, 0, 1, 12),
                ip(10, 0, 1, 13)
            ]
        );
    }

    #[test]
    fn excluding_an_interface_excludes_its_labeled_aliases() {
        let sources = select_sources(&single_eni_with_secondary_ips(), &config(&[], &["ens5"]));
        assert!(sources.is_empty());
    }

    #[test]
    fn a_pattern_naming_an_alias_singles_out_just_that_alias() {
        let host = single_eni_with_secondary_ips();
        assert_eq!(
            select_sources(&host, &config(&["ens5:1"], &[])),
            [ip(10, 0, 1, 12)]
        );
        assert_eq!(
            select_sources(&host, &config(&["ens5:*"], &[])),
            [ip(10, 0, 1, 12), ip(10, 0, 1, 13)]
        );
        // Excluding one alias leaves the rest of the ENI in use.
        assert_eq!(
            select_sources(&host, &config(&[], &["ens5:2"])),
            [ip(10, 0, 1, 10), ip(10, 0, 1, 11), ip(10, 0, 1, 12)]
        );
    }

    #[test]
    fn aliases_of_virtual_interfaces_are_virtual_too() {
        let host = vec![
            addr("docker0:1", [172, 17, 0, 5]),
            addr("lo:1", [127, 0, 1, 1]),
            addr("veth0a1b2c3:1", [10, 99, 0, 1]),
            addr("ens5", [10, 0, 1, 10]),
        ];
        assert_eq!(select_sources(&host, &config(&[], &[])), [ip(10, 0, 1, 10)]);
    }

    #[test]
    fn result_is_sorted_and_deduplicated_regardless_of_listing_order() {
        let host = vec![
            addr("ens6", [10, 0, 2, 20]),
            addr("ens5", [10, 0, 1, 10]),
            addr("ens5", [10, 0, 1, 10]),
        ];
        let sources = select_sources(&host, &config(&[], &[]));
        assert_eq!(sources, [ip(10, 0, 1, 10), ip(10, 0, 2, 20)]);
    }

    #[test]
    fn unspecified_address_is_never_a_source() {
        let host = vec![addr("ens5", [0, 0, 0, 0])];
        assert!(select_sources(&host, &config(&[], &[])).is_empty());
        assert!(select_sources(&host, &config(&["ens5"], &[])).is_empty());
    }

    #[test]
    fn nothing_matching_yields_no_sources() {
        assert!(select_sources(&ec2_host(), &config(&["nope*"], &[])).is_empty());
        assert!(select_sources(&[], &config(&[], &[])).is_empty());
    }

    struct FixedInterfaces(Vec<InterfaceAddr>);

    impl InterfaceLister for FixedInterfaces {
        fn list(&self) -> io::Result<Vec<InterfaceAddr>> {
            Ok(self.0.clone())
        }
    }

    struct FailingInterfaces;

    impl InterfaceLister for FailingInterfaces {
        fn list(&self) -> io::Result<Vec<InterfaceAddr>> {
            Err(io::Error::other("getifaddrs failed"))
        }
    }

    #[test]
    fn resolve_sources_applies_the_selection_to_what_the_lister_reports() {
        let lister = FixedInterfaces(ec2_host());
        let sources = resolve_sources(&lister, &config(&[], &[])).unwrap();
        assert_eq!(sources, [ip(10, 0, 1, 10), ip(10, 0, 2, 20)]);
    }

    #[test]
    fn resolve_sources_propagates_a_listing_failure() {
        assert!(resolve_sources(&FailingInterfaces, &config(&[], &[])).is_err());
    }

    #[test]
    fn system_interfaces_can_be_listed() {
        // Which interfaces exist depends on the machine; only that listing works is portable.
        let addrs = SystemInterfaces.list().unwrap();
        assert!(addrs.iter().all(|a| !a.interface.is_empty()));
    }
}
