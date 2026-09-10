//! Bonjour advertise/discover for the LAN machine-invoke listener - a
//! second, sidecar-independent service, deliberately separate from
//! `bonjour`'s existing mobile-pairing advertisement (its own service type,
//! its own TXT shape, its own lifecycle) rather than a change to that
//! already-shipping code path.
//!
//! Unlike `bonjour::MobileBonjourAdvertisement` (native DNS-SD via libSystem
//! on macOS specifically to avoid several local Kanna processes contending
//! over one SO_REUSEPORT UDP/5353 socket), this uses `mdns-sd` uniformly on
//! every platform for both advertising and browsing. That is a real,
//! deliberate simplification, not an oversight: the contention `bonjour`'s
//! own comment describes is a multi-instance *development* concern, and a
//! macOS-native browse implementation (`DNSServiceBrowse` over the same
//! FFI) would be the equivalent long-term hardening this module does not
//! yet have.
//!
//! Discovery is deliberately inert on its own: a resolved candidate only
//! ever reaches [`AppState::set_lan_candidate`] - an address hint, never a
//! trust decision. Nothing here reads or writes `machine_trust`, checks an
//! account, or decides who to trust; `invoke_desktop`'s pinned-TLS client
//! is the only place a candidate is ever acted on, and it authenticates the
//! responder independently of anything this module observed.

use crate::http_api::AppState;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::thread::JoinHandle;

/// RFC 6763 section 7.2 limits a service name to 15 bytes; mdns-sd (and the
/// real network stack it shares the wire with, including the OS's own
/// mDNSResponder) reliably fails to resolve a name that exceeds it, so
/// `kanna-lan` (9 bytes) is deliberate, not a style choice - confirmed by
/// direct reproduction: an earlier `kanna-lan-routing` (17 bytes) never
/// resolved in this environment, while otherwise-identical advertise/browse
/// code resolved in under a second once shortened. Matches the existing
/// `_kanna-mobile._tcp.local.` (`bonjour::MOBILE_BONJOUR_SERVICE_TYPE`),
/// which stays under the same limit.
pub const LAN_ROUTING_SERVICE_TYPE: &str = "_kanna-lan._tcp.local.";

/// This module's own TXT-record shape version - independent of
/// `machine_trust::MACHINE_TRUST_PROTOCOL_VERSION` (a different record, a
/// different owner) and of the relay's `desktopRouting` capability version
/// (a different transport entirely). A resolution advertising any other
/// value is filtered out at discovery time: see `candidate_from_resolution`.
pub const LAN_ROUTING_PROTOCOL_VERSION: u32 = 1;

/// TXT record for the LAN routing service: identity and version only, never
/// a credential. `environment` and `protocolVersion` let a receiver filter
/// out a candidate advertised by a differently-environmented or
/// incompatible build sharing the same LAN (e.g. a staging desktop) without
/// needing to trust the label for anything beyond that - the real
/// authentication happens entirely later, in `invoke_desktop`.
fn lan_routing_txt<'a>(desktop_id: &'a str, environment: &'a str) -> Vec<(&'a str, String)> {
    vec![
        ("desktopId", desktop_id.to_string()),
        ("environment", environment.to_string()),
        ("protocolVersion", LAN_ROUTING_PROTOCOL_VERSION.to_string()),
    ]
}

/// Advertises this desktop's LAN machine-invoke listener. Holding this value
/// keeps the advertisement alive; dropping it withdraws it.
pub struct LanRoutingAdvertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl LanRoutingAdvertisement {
    pub fn start(desktop_id: &str, environment: &str, port: u16) -> Result<Self, String> {
        let daemon = ServiceDaemon::new()
            .map_err(|error| format!("failed to start mDNS daemon: {error}"))?;
        let txt = lan_routing_txt(desktop_id, environment);
        // Only routable addresses, matching bonjour.rs's own mobile
        // advertisement and for the identical reason: `enable_addr_auto`
        // would also publish loopback/link-local addresses, and a sibling
        // resolving this service would then try (and hang on) an address it
        // can never actually reach.
        let addresses = routable_lan_addresses();
        let mut service = ServiceInfo::new(
            LAN_ROUTING_SERVICE_TYPE,
            desktop_id,
            &format!("{desktop_id}.local."),
            &addresses[..],
            port,
            &txt[..],
        )
        .map_err(|error| format!("failed to build LAN routing Bonjour service: {error}"))?;
        if addresses.is_empty() {
            service = service.enable_addr_auto();
        }
        let fullname = service.get_fullname().to_string();
        daemon.register(service).map_err(|error| {
            let _ = daemon.shutdown();
            format!("failed to register LAN routing Bonjour service: {error}")
        })?;
        log::info!("advertising LAN routing service for {desktop_id} on port {port}");
        Ok(Self { daemon, fullname })
    }
}

/// Only routable addresses - mirrors `bonjour::routable_lan_addresses`
/// exactly (that copy is `cfg`-gated out of real macOS production builds,
/// since macOS's own mobile-pairing advertisement uses native DNS-SD
/// instead and never needs it; this module needs the same filtering
/// unconditionally, since it uses `mdns-sd` on every platform).
fn routable_lan_addresses() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .map(|interfaces| {
            interfaces
                .into_iter()
                .map(|interface| interface.addr.ip())
                .filter(is_routable_lan_address)
                .collect()
        })
        .unwrap_or_default()
}

fn is_routable_lan_address(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(v6) => {
            !v6.is_loopback() && !v6.is_unspecified() && (v6.segments()[0] & 0xffc0) != 0xfe80
        }
    }
}

impl Drop for LanRoutingAdvertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// The suffix a resolved/removed service's fullname carries after its
/// instance name (== desktop_id, since `LanRoutingAdvertisement::start`
/// names the instance after the desktop_id it advertises).
fn instance_name(fullname: &str) -> Option<&str> {
    fullname.strip_suffix(&format!(".{LAN_ROUTING_SERVICE_TYPE}"))
}

/// The candidate a resolution should record, given its already-extracted
/// desktop_id/address/port/environment/protocol-version - kept as a pure
/// function over primitives (rather than over `mdns_sd::ResolvedService`
/// directly, which is `#[non_exhaustive]` with no public constructor and so
/// cannot be built in a test) so the actual mapping logic stays unit-testable
/// without a real mDNS daemon. `start_discovery` is what extracts these from
/// a real event.
///
/// `environment` and `protocol_version` are validated *here*, as filters on
/// whether a candidate is worth recording at all - never as authority: a
/// candidate advertising a different environment (a staging sibling sharing
/// this LAN) or an unrecognized protocol version is simply never recorded,
/// exactly as if discovery had never observed it. Nothing about this
/// upgrades the candidate's trustworthiness once filtered in;
/// `invoke_desktop`'s pinned-TLS client still independently authenticates
/// the responder before anything is ever sent to this address.
fn candidate_from_resolution(
    desktop_id: Option<&str>,
    address: Option<IpAddr>,
    port: u16,
    advertised_environment: Option<&str>,
    advertised_protocol_version: Option<&str>,
    current_environment: &str,
) -> Option<(String, SocketAddr)> {
    if advertised_environment != Some(current_environment) {
        return None;
    }
    if advertised_protocol_version != Some(&LAN_ROUTING_PROTOCOL_VERSION.to_string()) {
        return None;
    }
    Some((desktop_id?.to_string(), SocketAddr::new(address?, port)))
}

/// Which candidate a removal clears, given the removed instance's fullname.
fn candidate_removed_by_fullname(fullname: &str) -> Option<String> {
    instance_name(fullname).map(str::to_string)
}

/// Starts browsing for other desktops' LAN routing listeners and applies
/// every resolution/removal to `state`'s candidate map for as long as the
/// returned handle's underlying thread runs (until the process exits or the
/// daemon shuts down - there is no explicit stop today, matching the fact
/// that nothing currently calls it outside of startup). Every candidate this
/// produces is an untrusted discovered peer, not a same-account sibling:
/// discovery never checks account, and a resolution is recorded for
/// whatever `desktopId` its TXT record claims regardless of who advertised
/// it. Account and authentication decisions belong entirely to the existing
/// trust/TLS route (`invoke_desktop`'s pinned-TLS client), which is what
/// actually decides whether a candidate is ever worth trusting.
pub fn start_discovery(state: Arc<AppState>) -> Result<JoinHandle<()>, String> {
    let daemon =
        ServiceDaemon::new().map_err(|error| format!("failed to start mDNS daemon: {error}"))?;
    let receiver = daemon
        .browse(LAN_ROUTING_SERVICE_TYPE)
        .map_err(|error| format!("failed to browse for LAN routing services: {error}"))?;
    let current_environment = state.config().environment.clone();
    let handle = std::thread::Builder::new()
        .name("kanna-lan-routing-discovery".to_string())
        .spawn(move || {
            let _daemon = daemon;
            while let Ok(event) = receiver.recv() {
                match event {
                    ServiceEvent::ServiceResolved(resolved) => {
                        let desktop_id = resolved.txt_properties.get_property_val_str("desktopId");
                        let address = resolved.addresses.iter().next().map(|ip| ip.to_ip_addr());
                        let advertised_environment =
                            resolved.txt_properties.get_property_val_str("environment");
                        let advertised_protocol_version = resolved
                            .txt_properties
                            .get_property_val_str("protocolVersion");
                        if let Some((desktop_id, address)) = candidate_from_resolution(
                            desktop_id,
                            address,
                            resolved.port,
                            advertised_environment,
                            advertised_protocol_version,
                            &current_environment,
                        ) {
                            log::info!("LAN routing candidate observed: {desktop_id} at {address}");
                            state.set_lan_candidate(desktop_id, address);
                        }
                    }
                    ServiceEvent::ServiceRemoved(_service_type, fullname) => {
                        if let Some(desktop_id) = candidate_removed_by_fullname(&fullname) {
                            log::info!("LAN routing candidate withdrawn: {desktop_id}");
                            state.remove_lan_candidate(&desktop_id);
                        }
                    }
                    _ => {}
                }
            }
        })
        .map_err(|error| format!("failed to start LAN routing discovery thread: {error}"))?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROTOCOL: &str = "1";

    #[test]
    fn a_resolution_with_a_desktop_id_and_address_becomes_a_candidate() {
        let address = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 5));
        let update = candidate_from_resolution(
            Some("desktop-target"),
            Some(address),
            4460,
            Some("development"),
            Some(PROTOCOL),
            "development",
        )
        .expect("update");
        assert_eq!(update.0, "desktop-target");
        assert_eq!(update.1, SocketAddr::new(address, 4460));
    }

    #[test]
    fn a_resolution_with_no_desktop_id_produces_no_update() {
        let address = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 5));
        assert!(candidate_from_resolution(
            None,
            Some(address),
            4460,
            Some("development"),
            Some(PROTOCOL),
            "development",
        )
        .is_none());
    }

    #[test]
    fn a_resolution_with_no_address_produces_no_update() {
        assert!(candidate_from_resolution(
            Some("desktop-target"),
            None,
            4460,
            Some("development"),
            Some(PROTOCOL),
            "development",
        )
        .is_none());
    }

    #[test]
    fn a_resolution_advertising_a_different_environment_produces_no_update() {
        let address = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 5));
        assert!(
            candidate_from_resolution(
                Some("desktop-staging"),
                Some(address),
                4460,
                Some("staging"),
                Some(PROTOCOL),
                "development",
            )
            .is_none(),
            "a same-LAN sibling in a different environment must never become a candidate"
        );
    }

    #[test]
    fn a_resolution_missing_its_environment_label_produces_no_update() {
        let address = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 5));
        assert!(candidate_from_resolution(
            Some("desktop-target"),
            Some(address),
            4460,
            None,
            Some(PROTOCOL),
            "development",
        )
        .is_none());
    }

    #[test]
    fn a_resolution_advertising_an_unrecognized_protocol_version_produces_no_update() {
        let address = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 5));
        assert!(
            candidate_from_resolution(
                Some("desktop-target"),
                Some(address),
                4460,
                Some("development"),
                Some("2"),
                "development",
            )
            .is_none(),
            "an unrecognized protocol version must never become a candidate"
        );
    }

    #[test]
    fn a_removed_services_fullname_yields_its_desktop_id() {
        let fullname = format!("desktop-target.{LAN_ROUTING_SERVICE_TYPE}");
        assert_eq!(
            candidate_removed_by_fullname(&fullname).as_deref(),
            Some("desktop-target")
        );
    }

    #[test]
    fn a_fullname_from_an_unrelated_service_type_yields_nothing() {
        assert!(candidate_removed_by_fullname("desktop-target._other._tcp.local.").is_none());
    }

    /// End to end against a real mDNS daemon on loopback interfaces: proves
    /// the whole advertise -> browse -> resolve -> AppState chain, not just
    /// the pure mapping functions above.
    #[tokio::test]
    async fn advertising_and_discovering_populate_the_real_candidate_map() {
        let state =
            crate::http_api::test_state_with_seed("desktop-e2e-observer", "E2E Mac", |_db| {});

        let _advertisement =
            LanRoutingAdvertisement::start("desktop-e2e-target", "development", 4460)
                .expect("start advertisement");
        let _discovery = start_discovery(Arc::clone(&state)).expect("start discovery");

        // Matches this crate's existing real mDNS integration test
        // (bonjour_multi_process.rs), which also budgets up to ~15-20s for
        // genuine multicast probe/announce/resolve round trips rather than
        // the sub-second timing a mock would allow.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if state.lan_candidate_for("desktop-e2e-target").is_some() {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("discovery did not observe the advertised service in time");
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}
