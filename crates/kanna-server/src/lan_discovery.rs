//! Bonjour advertise/discover for the LAN machine-invoke listener - a
//! second, sidecar-independent service, deliberately separate from
//! `bonjour`'s existing mobile-pairing advertisement (its own service type,
//! its own TXT shape, its own lifecycle) rather than a change to that
//! already-shipping code path.
//!
//! On macOS both advertisement and discovery use the system DNS-SD broker,
//! sharing `bonjour`'s existing native registration supervisor. This avoids
//! competing raw UDP/5353 sockets and gives mDNSResponder ownership of
//! interface changes, expiry, and late join. Other platforms retain the
//! existing `mdns-sd` implementation and wire format.
//!
//! Discovery is deliberately inert on its own: a resolved candidate only
//! ever reaches [`AppState::set_lan_candidate`] - an address hint, never a
//! trust decision. Nothing here reads or writes `machine_trust`, checks an
//! account, or decides who to trust; `invoke_desktop`'s pinned-TLS client
//! is the only place a candidate is ever acted on, and it authenticates the
//! responder independently of anything this module observed.
//!
//! Resolved addresses are filtered before projection. The system responder
//! may report loopback or link-local records alongside a usable interface
//! address; those are never offered to the pinned-TLS client.

use crate::http_api::AppState;
#[cfg(not(target_os = "macos"))]
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

#[cfg(target_os = "macos")]
mod macos;

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
#[cfg(not(target_os = "macos"))]
pub struct LanRoutingAdvertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

#[cfg(not(target_os = "macos"))]
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
        let auto_addr = addresses.is_empty();
        let mut service = ServiceInfo::new(
            LAN_ROUTING_SERVICE_TYPE,
            desktop_id,
            &format!("{desktop_id}.local."),
            &addresses[..],
            port,
            &txt[..],
        )
        .map_err(|error| format!("failed to build LAN routing Bonjour service: {error}"))?;
        if auto_addr {
            service = service.enable_addr_auto();
        }
        let fullname = service.get_fullname().to_string();
        log::debug!(
            "registering LAN routing Bonjour service: fullname={fullname} port={port} \
             addresses={addresses:?} addr_auto={auto_addr}"
        );
        daemon.register(service).map_err(|error| {
            let _ = daemon.shutdown();
            format!("failed to register LAN routing Bonjour service: {error}")
        })?;
        log::info!("advertising LAN routing service for {desktop_id} on port {port}");
        Ok(Self { daemon, fullname })
    }
}

/// Only routable addresses - mirrors `bonjour::routable_lan_addresses`
/// exactly. The non-macOS advertiser supplies these explicitly; macOS's
/// default-host registration lets mDNSResponder own address refresh instead.
#[cfg(not(target_os = "macos"))]
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

pub(crate) fn is_routable_lan_address(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(v6) => {
            !v6.is_loopback() && !v6.is_unspecified() && (v6.segments()[0] & 0xffc0) != 0xfe80
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl Drop for LanRoutingAdvertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

#[cfg(target_os = "macos")]
pub struct LanRoutingAdvertisement {
    _native: crate::bonjour::NativeBonjourAdvertisement,
}

#[cfg(target_os = "macos")]
impl LanRoutingAdvertisement {
    pub fn start(desktop_id: &str, environment: &str, port: u16) -> Result<Self, String> {
        let native = crate::bonjour::NativeBonjourAdvertisement::start_service(
            desktop_id,
            LAN_ROUTING_SERVICE_TYPE,
            &lan_routing_txt(desktop_id, environment),
            port,
        )?;
        Ok(Self { _native: native })
    }
}

/// The address a resolution should hand to `candidate_from_resolution`, given
/// a resolved service's full address list. The advertiser's own address list
/// may (by design - see `LanRoutingAdvertisement::start`'s doc comment)
/// include loopback/link-local addresses alongside a real routable one;
/// picking a routable address specifically, rather than an arbitrary first
/// entry, is what keeps this module's original safety property - never hand a
/// sibling an address it can never actually reach - even though that
/// filtering can no longer happen at advertise time. Kept as a pure function
/// over `IpAddr` so the selection policy (first routable address wins,
/// original order preserved) stays unit-testable without a real mDNS
/// resolution.
fn select_routable_address(addresses: impl IntoIterator<Item = IpAddr>) -> Option<IpAddr> {
    addresses.into_iter().find(is_routable_lan_address)
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
    let desktop_id = desktop_id?.trim();
    if desktop_id.is_empty() || port == 0 {
        return None;
    }
    if advertised_environment != Some(current_environment) {
        return None;
    }
    if advertised_protocol_version != Some(&LAN_ROUTING_PROTOCOL_VERSION.to_string()) {
        return None;
    }
    Some((desktop_id.to_string(), SocketAddr::new(address?, port)))
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct ServiceKey {
    pub(super) name: String,
    pub(super) registration_type: String,
    pub(super) domain: String,
    pub(super) interface_index: u32,
}

impl ServiceKey {
    #[cfg(not(target_os = "macos"))]
    fn mdns(fullname: &str) -> Self {
        Self {
            name: fullname.to_string(),
            registration_type: LAN_ROUTING_SERVICE_TYPE.to_string(),
            domain: "local.".to_string(),
            interface_index: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Resolution {
    pub(super) desktop_id: Option<String>,
    pub(super) environment: Option<String>,
    pub(super) protocol_version: Option<String>,
    pub(super) port: u16,
}

#[derive(Clone, Debug, Default)]
struct Observation {
    generation: u64,
    resolution: Option<Resolution>,
    addresses: BTreeSet<IpAddr>,
}

/// Ephemeral DNS-SD bookkeeping. A service can exist on several interfaces,
/// and callbacks from a deallocated resolve/address operation can already be
/// queued when a newer browse generation replaces it. Keeping both the
/// interface and generation here prevents either case from removing or
/// resurrecting the wrong candidate.
#[derive(Default)]
pub(super) struct ObservationBook {
    next_generation: u64,
    observations: BTreeMap<ServiceKey, Observation>,
    projected: BTreeMap<String, SocketAddr>,
}

impl ObservationBook {
    pub(super) fn begin(&mut self, key: ServiceKey) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.observations.insert(
            key,
            Observation {
                generation,
                ..Observation::default()
            },
        );
        generation
    }

    pub(super) fn current_generation(&self, key: &ServiceKey) -> Option<u64> {
        self.observations.get(key).map(|value| value.generation)
    }

    pub(super) fn resolve(
        &mut self,
        key: &ServiceKey,
        generation: u64,
        resolution: Resolution,
    ) -> bool {
        let Some(observation) = self.observations.get_mut(key) else {
            return false;
        };
        if observation.generation != generation {
            return false;
        }
        observation.resolution = Some(resolution);
        observation.addresses.clear();
        true
    }

    #[cfg(not(target_os = "macos"))]
    fn replace_addresses(
        &mut self,
        key: &ServiceKey,
        generation: u64,
        addresses: impl IntoIterator<Item = IpAddr>,
    ) -> bool {
        let Some(observation) = self.observations.get_mut(key) else {
            return false;
        };
        if observation.generation != generation {
            return false;
        }
        observation.addresses = addresses.into_iter().collect();
        true
    }

    #[cfg(any(target_os = "macos", test))]
    pub(super) fn address(
        &mut self,
        key: &ServiceKey,
        generation: u64,
        address: IpAddr,
        added: bool,
    ) -> bool {
        let Some(observation) = self.observations.get_mut(key) else {
            return false;
        };
        if observation.generation != generation {
            return false;
        }
        if added {
            observation.addresses.insert(address);
        } else {
            observation.addresses.remove(&address);
        }
        true
    }

    pub(super) fn withdraw(&mut self, key: &ServiceKey) {
        self.observations.remove(key);
    }

    pub(super) fn clear(&mut self) {
        self.observations.clear();
    }

    fn desired(&self, current_environment: &str) -> BTreeMap<String, SocketAddr> {
        let mut desired = BTreeMap::new();
        for observation in self.observations.values() {
            let Some(resolution) = observation.resolution.as_ref() else {
                continue;
            };
            let candidate = candidate_from_resolution(
                resolution.desktop_id.as_deref(),
                select_routable_address(observation.addresses.iter().copied()),
                resolution.port,
                resolution.environment.as_deref(),
                resolution.protocol_version.as_deref(),
                current_environment,
            );
            if let Some((desktop_id, address)) = candidate {
                desired.entry(desktop_id).or_insert(address);
            }
        }
        desired
    }

    pub(super) fn project(&mut self, state: &AppState, current_environment: &str) {
        let desired = self.desired(current_environment);
        for desktop_id in self.projected.keys() {
            if !desired.contains_key(desktop_id) {
                log::info!("LAN routing candidate withdrawn: {desktop_id}");
                state.remove_lan_candidate(desktop_id);
            }
        }
        for (desktop_id, address) in &desired {
            if self.projected.get(desktop_id) != Some(address) {
                log::info!("LAN routing candidate observed: {desktop_id} at {address}");
                state.set_lan_candidate(desktop_id.clone(), *address);
            }
        }
        self.projected = desired;
    }
}

/// Owns the active browser and every per-service resolver/address observer.
/// Dropping it synchronously stops and joins the worker, so no callback
/// context outlives its DNSServiceRef or the server state it projects into.
#[cfg(target_os = "macos")]
pub use macos::Discovery;

#[cfg(target_os = "macos")]
pub fn start_discovery(state: Arc<AppState>) -> Result<Discovery, String> {
    Discovery::start(state)
}

#[cfg(not(target_os = "macos"))]
pub struct Discovery {
    stop: std::sync::mpsc::SyncSender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[cfg(not(target_os = "macos"))]
impl Discovery {
    fn start(state: Arc<AppState>) -> Result<Self, String> {
        let daemon = ServiceDaemon::new()
            .map_err(|error| format!("failed to start mDNS daemon: {error}"))?;
        let receiver = daemon
            .browse(LAN_ROUTING_SERVICE_TYPE)
            .map_err(|error| format!("failed to browse for LAN routing services: {error}"))?;
        let environment = state.config().environment.clone();
        let (stop, stop_receiver) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("kanna-lan-routing-discovery".to_string())
            .spawn(move || {
                let mut observations = ObservationBook::default();
                loop {
                    if matches!(
                        stop_receiver.try_recv(),
                        Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected)
                    ) {
                        break;
                    }
                    let event = match receiver.recv_timeout(std::time::Duration::from_millis(250)) {
                        Ok(event) => event,
                        Err(flume::RecvTimeoutError::Timeout) => continue,
                        Err(flume::RecvTimeoutError::Disconnected) => break,
                    };
                    match event {
                        ServiceEvent::ServiceFound(_, fullname) => {
                            let key = ServiceKey::mdns(&fullname);
                            if observations.current_generation(&key).is_none() {
                                observations.begin(key);
                            }
                        }
                        ServiceEvent::ServiceResolved(resolved) => {
                            let key = ServiceKey::mdns(&resolved.fullname);
                            let generation = observations
                                .current_generation(&key)
                                .unwrap_or_else(|| observations.begin(key.clone()));
                            observations.resolve(
                                &key,
                                generation,
                                Resolution {
                                    desktop_id: resolved
                                        .txt_properties
                                        .get_property_val_str("desktopId")
                                        .map(str::to_string),
                                    environment: resolved
                                        .txt_properties
                                        .get_property_val_str("environment")
                                        .map(str::to_string),
                                    protocol_version: resolved
                                        .txt_properties
                                        .get_property_val_str("protocolVersion")
                                        .map(str::to_string),
                                    port: resolved.port,
                                },
                            );
                            observations.replace_addresses(
                                &key,
                                generation,
                                resolved.addresses.iter().map(|value| value.to_ip_addr()),
                            );
                            observations.project(&state, &environment);
                        }
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            observations.withdraw(&ServiceKey::mdns(&fullname));
                            observations.project(&state, &environment);
                        }
                        _ => {}
                    }
                }
                observations.clear();
                observations.project(&state, &environment);
                let _ = daemon.shutdown();
            })
            .map_err(|error| format!("failed to start LAN routing discovery thread: {error}"))?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}

#[cfg(not(target_os = "macos"))]
impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                log::warn!("LAN routing discovery worker panicked during shutdown");
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn start_discovery(state: Arc<AppState>) -> Result<Discovery, String> {
    Discovery::start(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROTOCOL: &str = "1";

    #[test]
    fn select_routable_address_skips_leading_unroutable_addresses_in_a_mixed_set() {
        let addresses = [
            IpAddr::from([127, 0, 0, 1]),
            IpAddr::from([169, 254, 1, 1]),
            IpAddr::from([192, 168, 1, 42]),
            IpAddr::from([10, 0, 0, 7]),
        ];
        assert_eq!(
            select_routable_address(addresses),
            Some(IpAddr::from([192, 168, 1, 42])),
            "should pick the first routable address, skipping loopback/link-local ones ahead of it"
        );
    }

    #[test]
    fn select_routable_address_is_none_when_every_address_is_unroutable() {
        let addresses = [
            IpAddr::from([127, 0, 0, 1]),
            IpAddr::from([169, 254, 1, 1]),
            IpAddr::from([0, 0, 0, 0]),
        ];
        assert_eq!(
            select_routable_address(addresses),
            None,
            "an all-loopback/link-local/unspecified address set has no usable candidate"
        );
    }

    #[test]
    fn select_routable_address_is_none_for_an_empty_set() {
        assert_eq!(select_routable_address(std::iter::empty()), None);
    }
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
    fn a_resolution_with_an_empty_desktop_id_or_zero_port_produces_no_update() {
        let address = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 5));
        assert!(candidate_from_resolution(
            Some("  "),
            Some(address),
            4460,
            Some("development"),
            Some(PROTOCOL),
            "development",
        )
        .is_none());
        assert!(candidate_from_resolution(
            Some("desktop-target"),
            Some(address),
            0,
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

    fn observation_key(interface_index: u32) -> ServiceKey {
        ServiceKey {
            name: "service-instance".to_string(),
            registration_type: LAN_ROUTING_SERVICE_TYPE.to_string(),
            domain: "local.".to_string(),
            interface_index,
        }
    }

    fn valid_resolution(desktop_id: &str, port: u16) -> Resolution {
        Resolution {
            desktop_id: Some(desktop_id.to_string()),
            environment: Some("development".to_string()),
            protocol_version: Some(PROTOCOL.to_string()),
            port,
        }
    }

    #[test]
    fn observations_add_update_and_remove_the_exact_projected_desktop_id() {
        let state = crate::http_api::test_state_with_seed("observer", "Observer", |_| {});
        let key = observation_key(4);
        let mut book = ObservationBook::default();
        let generation = book.begin(key.clone());
        assert!(book.resolve(&key, generation, valid_resolution("desktop-old", 4460)));
        assert!(book.address(&key, generation, IpAddr::from([192, 168, 1, 9]), true));
        book.project(&state, "development");
        assert_eq!(
            state.lan_candidate_for("desktop-old"),
            Some("192.168.1.9:4460".parse().unwrap())
        );

        assert!(book.resolve(&key, generation, valid_resolution("desktop-new", 4470)));
        assert!(book.address(&key, generation, IpAddr::from([192, 168, 1, 10]), true));
        book.project(&state, "development");
        assert!(state.lan_candidate_for("desktop-old").is_none());
        assert_eq!(
            state.lan_candidate_for("desktop-new"),
            Some("192.168.1.10:4470".parse().unwrap())
        );

        book.withdraw(&key);
        book.project(&state, "development");
        assert!(state.lan_candidate_for("desktop-new").is_none());
    }

    #[test]
    fn removing_one_interface_keeps_another_live_observation() {
        let state = crate::http_api::test_state_with_seed("observer", "Observer", |_| {});
        let mut book = ObservationBook::default();
        let first = observation_key(4);
        let second = observation_key(7);
        for (key, address) in [
            (&first, IpAddr::from([192, 168, 1, 9])),
            (&second, IpAddr::from([10, 0, 0, 9])),
        ] {
            let generation = book.begin(key.clone());
            book.resolve(key, generation, valid_resolution("desktop-target", 4460));
            book.address(key, generation, address, true);
        }
        book.project(&state, "development");
        assert!(state.lan_candidate_for("desktop-target").is_some());
        book.withdraw(&first);
        book.project(&state, "development");
        assert_eq!(
            state.lan_candidate_for("desktop-target"),
            Some("10.0.0.9:4460".parse().unwrap())
        );
    }

    #[test]
    fn stale_callbacks_and_partial_resolutions_never_project() {
        let state = crate::http_api::test_state_with_seed("observer", "Observer", |_| {});
        let key = observation_key(4);
        let mut book = ObservationBook::default();
        let stale = book.begin(key.clone());
        let current = book.begin(key.clone());
        assert!(!book.resolve(&key, stale, valid_resolution("desktop-stale", 4460)));
        assert!(!book.address(&key, stale, IpAddr::from([192, 168, 1, 8]), true));
        assert!(book.resolve(
            &key,
            current,
            Resolution {
                desktop_id: None,
                ..valid_resolution("ignored", 4460)
            }
        ));
        book.address(&key, current, IpAddr::from([192, 168, 1, 9]), true);
        book.project(&state, "development");
        assert!(state.lan_candidate_desktop_ids().is_empty());

        book.withdraw(&key);
        assert!(!book.resolve(
            &key,
            current,
            valid_resolution("desktop-after-withdrawal", 4460)
        ));
        assert!(!book.address(&key, current, IpAddr::from([192, 168, 1, 10]), true));
        book.project(&state, "development");
        assert!(state.lan_candidate_desktop_ids().is_empty());
    }

    #[test]
    fn address_removal_and_browser_recovery_clear_stale_candidates() {
        let state = crate::http_api::test_state_with_seed("observer", "Observer", |_| {});
        let key = observation_key(4);
        let mut book = ObservationBook::default();
        let generation = book.begin(key.clone());
        book.resolve(&key, generation, valid_resolution("desktop-target", 4460));
        let address = IpAddr::from([192, 168, 1, 9]);
        book.address(&key, generation, address, true);
        book.project(&state, "development");
        book.address(&key, generation, address, false);
        book.project(&state, "development");
        assert!(state.lan_candidate_for("desktop-target").is_none());

        book.address(&key, generation, address, true);
        book.project(&state, "development");
        assert!(state.lan_candidate_for("desktop-target").is_some());
        book.clear();
        book.project(&state, "development");
        assert!(state.lan_candidate_for("desktop-target").is_none());
    }

    /// End to end against the production platform backend: proves the whole
    /// advertise -> browse -> resolve -> address observation -> AppState
    /// chain, not just the pure mapping functions above.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn advertising_and_discovering_populate_the_real_candidate_map() {
        let state =
            crate::http_api::test_state_with_seed("desktop-e2e-observer", "E2E Mac", |_db| {});

        let advertisement =
            LanRoutingAdvertisement::start("desktop-e2e-target", "development", 4460)
                .expect("start advertisement");
        let discovery = start_discovery(Arc::clone(&state)).expect("start discovery");

        // Matches this crate's existing real Bonjour integration test
        // (bonjour_multi_process.rs), which also budgets up to ~15-20s for
        // genuine multicast probe/announce/resolve round trips rather than
        // the sub-second timing a mock would allow.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(candidate) = state.lan_candidate_for("desktop-e2e-target") {
                assert_eq!(candidate.port(), 4460);
                assert!(is_routable_lan_address(&candidate.ip()));
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("discovery did not observe the advertised service in time");
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        drop(advertisement);
        let withdrawal_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while state.lan_candidate_for("desktop-e2e-target").is_some() {
            assert!(
                std::time::Instant::now() < withdrawal_deadline,
                "withdrawn advertisement remained projected"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let invalid = crate::bonjour::NativeBonjourAdvertisement::start_service(
            "desktop-e2e-target",
            LAN_ROUTING_SERVICE_TYPE,
            &[
                ("desktopId", "desktop-e2e-target".to_string()),
                ("environment", "staging".to_string()),
                ("protocolVersion", "2".to_string()),
            ],
            4461,
        )
        .expect("start incompatible advertisement");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(
            state.lan_candidate_for("desktop-e2e-target").is_none(),
            "an incompatible TXT environment/version must not be projected"
        );
        drop(invalid);

        let replacement = LanRoutingAdvertisement::start("desktop-e2e-target", "development", 4462)
            .expect("start replacement advertisement");
        let replacement_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if state
                .lan_candidate_for("desktop-e2e-target")
                .is_some_and(|candidate| candidate.port() == 4462)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < replacement_deadline,
                "replacement SRV port was not projected"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        drop(discovery);
        assert!(
            state.lan_candidate_for("desktop-e2e-target").is_none(),
            "stopping the discovery owner must clear its projected candidates"
        );
        drop(replacement);
    }
}
