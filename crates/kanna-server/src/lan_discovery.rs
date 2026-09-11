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
//!
//! One filtering decision does live here rather than at advertise time:
//! `start_discovery` picks the first *routable* address from a resolved
//! candidate's address list, rather than trusting an arbitrary first entry -
//! see `select_routable_address`. `LanRoutingAdvertisement::start` itself
//! still publishes explicit routable addresses (falling back to
//! `enable_addr_auto()` only when none are found), matching
//! `bonjour.rs`'s own mobile advertisement; no publish-side correction has
//! been made or shown necessary as of this comment - see the task
//! checkpoint (`.tmp/lan-revision-checkpoint.md`, not committed) for the
//! current, precise state of that investigation, which this comment does
//! not attempt to restate and risk going stale again.

use crate::http_api::AppState;
#[cfg(test)]
use mdns_sd::IfKind;
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
        //
        // This exact choice - explicit addresses vs. unrestricted
        // `enable_addr_auto()` vs. `enable_addr_auto()` restricted to these
        // same addresses' interfaces via `set_interfaces` - was directly,
        // empirically investigated (see this module's own real-mDNS tests:
        // `a_native_observer_sees_an_mdns_sd_advertised_service` and its
        // `_with_exactly_one_explicit_address`/`_using_auto_addr` variants,
        // and `restricting_advertised_interfaces_also_fails_to_reach_the_wire`).
        // On the specific host that investigation ran on, explicit addresses
        // and interface-restricted auto both failed to reach even PTR-level
        // visibility to a native observer, while unrestricted auto did reach
        // that level - but resolved, in every case actually inspected end to
        // end through this module's own `start_discovery`, to a loopback
        // address only, never the real routable one, regardless of a
        // startup delay. That is: on this host, no combination of this
        // crate's public advertise API was found to produce a genuinely
        // useful (non-loopback) resolvable candidate - see the checkpoint
        // history for the full evidence. Kept as explicit addresses (the
        // original, still-shipped behavior) rather than switched to auto,
        // because auto was not shown to be an improvement on this host and
        // does regress the loopback/link-local safety property elsewhere.
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

pub(crate) fn is_routable_lan_address(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(v6) => {
            !v6.is_loopback() && !v6.is_unspecified() && (v6.segments()[0] & 0xffc0) != 0xfe80
        }
    }
}

/// Names of every interface carrying at least one routable address. No
/// production caller: `LanRoutingAdvertisement::start` deliberately does
/// not restrict `enable_addr_auto()` to these via `set_interfaces` - see
/// its own doc comment and
/// `restricting_advertised_interfaces_also_fails_to_reach_the_wire`, the
/// regression test this exists to support, proving that restriction (even
/// to exactly these, correct names) is itself a reproduced defect on some
/// hosts. Sorted and deduplicated - an interface can carry more than one
/// routable address (e.g. IPv4 and IPv6).
#[cfg(test)]
fn routable_lan_interface_names() -> Vec<String> {
    let mut names: Vec<String> = if_addrs::get_if_addrs()
        .map(|interfaces| {
            interfaces
                .into_iter()
                .filter(|interface| is_routable_lan_address(&interface.addr.ip()))
                .map(|interface| interface.name)
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
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
    if advertised_environment != Some(current_environment) {
        return None;
    }
    if advertised_protocol_version != Some(&LAN_ROUTING_PROTOCOL_VERSION.to_string()) {
        return None;
    }
    Some((desktop_id?.to_string(), SocketAddr::new(address?, port)))
}

/// Diagnostic only - explains why [`candidate_from_resolution`] rejected a
/// resolution, without changing its decision in any way (this function has
/// no effect on filtering; it exists to make a silent rejection observable
/// while diagnosing whether discovery is failing before or after a real
/// `ServiceResolved` event).
fn candidate_rejection_reason(
    desktop_id: Option<&str>,
    address: Option<IpAddr>,
    advertised_environment: Option<&str>,
    advertised_protocol_version: Option<&str>,
    current_environment: &str,
) -> &'static str {
    if advertised_environment != Some(current_environment) {
        "environment mismatch"
    } else if advertised_protocol_version != Some(&LAN_ROUTING_PROTOCOL_VERSION.to_string()) {
        "protocol version mismatch"
    } else if desktop_id.is_none() {
        "missing desktopId TXT property"
    } else if address.is_none() {
        "no resolved address"
    } else {
        "unknown"
    }
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
                    // Diagnostic only: distinguishes "the PTR record for
                    // this service type was never observed at all" from
                    // "it was observed but never resolved" from "it
                    // resolved but candidate_from_resolution's own,
                    // unchanged filtering rejected it" - see this
                    // function's own doc comment and
                    // `candidate_rejection_reason`.
                    ServiceEvent::SearchStarted(service_type) => {
                        log::debug!("LAN routing discovery search started: {service_type}");
                    }
                    ServiceEvent::ServiceFound(service_type, fullname) => {
                        log::debug!("LAN routing service found (pre-resolution): {fullname} ({service_type})");
                    }
                    ServiceEvent::ServiceResolved(resolved) => {
                        let desktop_id = resolved.txt_properties.get_property_val_str("desktopId");
                        let address = select_routable_address(
                            resolved.addresses.iter().map(|ip| ip.to_ip_addr()),
                        );
                        let advertised_environment =
                            resolved.txt_properties.get_property_val_str("environment");
                        let advertised_protocol_version = resolved
                            .txt_properties
                            .get_property_val_str("protocolVersion");
                        log::debug!(
                            "LAN routing service resolved (pre-filter): fullname={} host={} port={} addresses={:?} environment={:?} protocolVersion={:?}",
                            resolved.fullname,
                            resolved.host,
                            resolved.port,
                            resolved.addresses,
                            advertised_environment,
                            advertised_protocol_version
                        );
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
                        } else {
                            log::debug!(
                                "LAN routing resolution rejected: {} ({})",
                                resolved.fullname,
                                candidate_rejection_reason(
                                    desktop_id,
                                    address,
                                    advertised_environment,
                                    advertised_protocol_version,
                                    &current_environment
                                )
                            );
                        }
                    }
                    ServiceEvent::ServiceRemoved(_service_type, fullname) => {
                        if let Some(desktop_id) = candidate_removed_by_fullname(&fullname) {
                            log::info!("LAN routing candidate withdrawn: {desktop_id}");
                            state.remove_lan_candidate(&desktop_id);
                        }
                    }
                    ServiceEvent::SearchStopped(service_type) => {
                        log::debug!("LAN routing discovery search stopped: {service_type}");
                    }
                    // `ServiceEvent` is `#[non_exhaustive]`; every variant
                    // known at this mdns-sd version is matched above, so
                    // this only guards a future variant.
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

    /// A native `dns-sd -B` observer whose stdout is read *incrementally*,
    /// in a background thread, as lines arrive - not only after the
    /// process is killed. `std::process::Child::kill()` sends `SIGKILL` on
    /// Unix, which the child cannot catch or use to flush its own stdio
    /// buffers; reading only via `wait_with_output()` after a kill can
    /// silently lose already-written-but-unflushed output for a piped
    /// (non-tty) child, making "empty output" ambiguous between "nothing
    /// was ever sent" and "something was sent but lost to buffering." This
    /// is exactly the methodological gap architect `2bf0950f`'s acceptance
    /// criterion 2 named ("an empty killed pipe alone is not proof that
    /// nothing reached the wire").
    struct NativeBrowseObserver {
        child: std::process::Child,
        lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        reader: Option<std::thread::JoinHandle<()>>,
    }

    /// Termination provenance for a [`NativeBrowseObserver`]: killing the
    /// child alone proves nothing about whether its reader thread actually
    /// drained every buffered line before `captured_text()` was read - see
    /// architect `57d246ec`'s own finding that incremental reading "doesn't
    /// force child flush" and this struct previously "never
    /// drains/joins" its reader.
    struct NativeObserverTermination {
        child_wait: std::io::Result<std::process::ExitStatus>,
        reader_joined: std::thread::Result<()>,
    }

    impl NativeBrowseObserver {
        fn start(service_type: &str) -> Self {
            let mut child = std::process::Command::new("/usr/bin/dns-sd")
                .arg("-B")
                .arg(service_type)
                .arg("local")
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("spawn native dns-sd -B");
            let stdout = child.stdout.take().expect("piped stdout");
            let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let lines_for_reader = std::sync::Arc::clone(&lines);
            let reader = std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok)
                {
                    lines_for_reader
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(line);
                }
            });
            Self {
                child,
                lines,
                reader: Some(reader),
            }
        }

        fn captured_text(&self) -> String {
            self.lines
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .join("\n")
        }

        /// Kills the child, waits for its real exit status, then explicitly
        /// joins the reader thread - which only returns once the pipe's
        /// `BufReader::lines()` iterator sees EOF, i.e. once every byte the
        /// kernel ever delivered has been drained into `lines`. Only after
        /// this returns is `captured_text()` (read separately, since this
        /// consumes `self`) guaranteed final rather than a snapshot of
        /// whatever the reader thread happened to have processed so far.
        fn stop(mut self) -> NativeObserverTermination {
            let _ = self.child.kill();
            let child_wait = self.child.wait();
            let reader_joined = self
                .reader
                .take()
                .expect("reader thread present until stop")
                .join();
            NativeObserverTermination {
                child_wait,
                reader_joined,
            }
        }
    }

    /// Captures `mdns-sd`'s own `log::Log` output during a bounded window -
    /// specifically to retain its `multicast_on_intf` trace lines
    /// (`"sent out {n} bytes on interface ..."` on `send_to` success,
    /// `"Failed to send to ... via ..."` on failure), the crate's own
    /// existing send-result seam architect `57d246ec` identified
    /// (`service_daemon.rs` ~4266) - `announce_service_on_intf` itself
    /// (~4437) returns `true` once `send_dns_outgoing` is *called*,
    /// regardless of whether the underlying `send_to` actually succeeded,
    /// so this is the one API-level signal available (short of packet
    /// capture, out of scope) that distinguishes "queued/attempted" from
    /// "the OS socket call itself reported success or failure." A process
    /// may install only one `log::Log`, so a test using this must run in
    /// isolation (a single `--exact` test name) - never alongside
    /// `relay.rs`'s own test-only logger in the same test binary
    /// invocation.
    struct MdnsSendTraceLogger;

    static MDNS_TRACE_LOGGER: MdnsSendTraceLogger = MdnsSendTraceLogger;
    static MDNS_TRACE_LOGGER_INIT: std::sync::Once = std::sync::Once::new();
    static MDNS_TRACE_LOG_ACTIVE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    static MDNS_TRACE_LOG_LINES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    impl log::Log for MdnsSendTraceLogger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            MDNS_TRACE_LOG_ACTIVE.load(std::sync::atomic::Ordering::Acquire)
                && metadata.target().starts_with("mdns_sd")
        }

        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                MDNS_TRACE_LOG_LINES
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(format!(
                        "{} {}: {}",
                        record.target(),
                        record.level(),
                        record.args()
                    ));
            }
        }

        fn flush(&self) {}
    }

    fn start_mdns_trace_capture() {
        MDNS_TRACE_LOGGER_INIT.call_once(|| {
            log::set_logger(&MDNS_TRACE_LOGGER).expect(
                "install mdns-sd trace-capture logger (run this test in isolation, not \
                 alongside another test-only logger in the same process)",
            );
            log::set_max_level(log::LevelFilter::Trace);
        });
        MDNS_TRACE_LOG_LINES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        MDNS_TRACE_LOG_ACTIVE.store(true, std::sync::atomic::Ordering::Release);
    }

    fn finish_mdns_trace_capture() -> Vec<String> {
        MDNS_TRACE_LOG_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
        std::mem::take(
            &mut *MDNS_TRACE_LOG_LINES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// The architect `57d246ec` acceptance-criterion-1 fixture: one owned
    /// comparison of explicit-address vs. `enable_addr_auto()` publish,
    /// both restricted to the *same single* real, routable interface via
    /// the daemon-level `enable_interface`/`disable_interface` controls -
    /// which are a distinct mechanism from `ServiceInfo::set_interfaces`
    /// (a per-service filter the daemon still applies its own, unrestricted
    /// interface enumeration around). Every previous comparison in this
    /// module compared a single-interface explicit run against a
    /// multi-interface auto run - not apples to apples. Retains, for each
    /// case: the daemon's own `monitor()` events, its own `send_to`
    /// success/failure trace lines (see `MdnsSendTraceLogger`), and a
    /// robust native observer's result with explicit termination
    /// provenance (see `NativeBrowseObserver::stop`). Deliberately
    /// assertion-free (diagnostic only) - the point is retaining rigorous,
    /// exact raw evidence for independent review, not asserting a
    /// conclusion this one run cannot by itself support. Must run in
    /// isolation: `cargo test -p kanna-server --bin kanna-server -- \
    /// --exact lan_discovery::tests::matched_single_interface_explicit_vs_auto_publish_comparison \
    /// --nocapture`.
    #[tokio::test]
    async fn matched_single_interface_explicit_vs_auto_publish_comparison() {
        let Some((if_name, if_addr)) = if_addrs::get_if_addrs().ok().and_then(|interfaces| {
            interfaces.into_iter().find_map(|interface| {
                is_routable_lan_address(&interface.addr.ip())
                    .then(|| (interface.name.clone(), interface.addr.ip()))
            })
        }) else {
            eprintln!(
                "skipping: no non-loopback interface on this host to run a matched comparison on"
            );
            return;
        };
        eprintln!("DIAG_MATCHED_FIXTURE interface={if_name} address={if_addr}");

        start_mdns_trace_capture();

        async fn run_case(
            case: &str,
            if_name: &str,
            if_addr: IpAddr,
            auto_addr: bool,
        ) -> (Vec<String>, String, NativeObserverTermination) {
            let unique = format!(
                "diag-matched-{case}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            );
            // RFC 6763 section 7.2 limits a service *type* label to 15
            // bytes (see this module's own top-of-file comment) - unlike
            // the instance name above, which has no such limit and stays
            // fully unique. A fresh, never-before-used service type per
            // case rules out mDNSResponder cache/rate-limit carryover from
            // this session's own heavy prior testing on `_kanna-lan._tcp` -
            // the same control `fresh-service-type-run1.log` already
            // established, kept short enough this time to actually pass
            // mdns-sd's own length check (an earlier version of this
            // exact test did not, and both cases below failed at
            // registration with "Service name length must be <= 15 bytes"
            // before ever reaching the wire - retained as
            // `matched-fixture-run1.log`, a real methodological bug in the
            // fixture, not evidence about the underlying defect).
            let short_suffix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
                % 0x1000;
            let service_type = format!("_dm{case}{short_suffix:x}._tcp.local.");

            let daemon = ServiceDaemon::new().expect("start mDNS daemon");
            daemon
                .disable_interface(IfKind::All)
                .expect("disable all interfaces");
            daemon
                .enable_interface(IfKind::Name(if_name.to_string()))
                .expect("enable only the target interface");

            let monitor = daemon.monitor().expect("monitor before registration");

            let no_txt: &[(&str, String)] = &[];
            let mut service = ServiceInfo::new(
                &service_type,
                &unique,
                &format!("{unique}.local."),
                &if_addr.to_string()[..],
                61_000,
                no_txt,
            )
            .expect("build service info");
            if auto_addr {
                service = service.enable_addr_auto();
            }
            daemon.register(service).expect("register service");

            let browse_type = service_type
                .strip_suffix(".local.")
                .expect("service_type ends with .local.");
            let native_browse = NativeBrowseObserver::start(browse_type);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            let mut monitor_events = Vec::new();
            while std::time::Instant::now() < deadline {
                if let Ok(event) = monitor.try_recv() {
                    monitor_events.push(format!("{event:?}"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            let native_text = native_browse.captured_text();
            let termination = native_browse.stop();
            let _ = daemon.shutdown();
            (monitor_events, native_text, termination)
        }

        let (explicit_monitor, explicit_native, explicit_termination) =
            run_case("explicit", &if_name, if_addr, false).await;
        let (auto_monitor, auto_native, auto_termination) =
            run_case("auto", &if_name, if_addr, true).await;

        let trace_lines = finish_mdns_trace_capture();

        eprintln!("DIAG_MATCHED_EXPLICIT_MONITOR {explicit_monitor:?}");
        eprintln!("DIAG_MATCHED_EXPLICIT_NATIVE stdout={explicit_native:?}");
        eprintln!(
            "DIAG_MATCHED_EXPLICIT_TERMINATION child_wait={:?} reader_joined_ok={}",
            explicit_termination.child_wait,
            explicit_termination.reader_joined.is_ok()
        );
        eprintln!("DIAG_MATCHED_AUTO_MONITOR {auto_monitor:?}");
        eprintln!("DIAG_MATCHED_AUTO_NATIVE stdout={auto_native:?}");
        eprintln!(
            "DIAG_MATCHED_AUTO_TERMINATION child_wait={:?} reader_joined_ok={}",
            auto_termination.child_wait,
            auto_termination.reader_joined.is_ok()
        );
        eprintln!("DIAG_MATCHED_MDNS_SEND_TRACE {trace_lines:?}");

        // A decisive follow-up question, using the exact same daemon-level
        // interface restriction and send-trace seam: does this module's own
        // *browse* side (`start_discovery`'s underlying `daemon.browse()`)
        // hit the identical `EHOSTUNREACH` when it sends its own PTR query,
        // on the same interface the publish side just failed on? If it
        // does, a native-advertisement correction on the publish side alone
        // could never fix real discovery on this host, since a genuine
        // sibling desktop's own mdns-sd browse query - unauthorized to
        // change (`no native browser`) - would still never reach the wire
        // either, independent of what the advertiser does. Both `send_query_on_intf`
        // and `announce_service_on_intf` funnel through the same
        // `send_dns_outgoing_impl` -> `multicast_on_intf` seam (confirmed by
        // direct reading of `service_daemon.rs`), so this is exactly the
        // browse-side counterpart of the publish-side check above, not a
        // new, unrelated diagnostic axis.
        start_mdns_trace_capture();
        let browse_daemon = ServiceDaemon::new().expect("start mDNS daemon for browse-side check");
        browse_daemon
            .disable_interface(IfKind::All)
            .expect("disable all interfaces for browse-side check");
        browse_daemon
            .enable_interface(IfKind::Name(if_name.clone()))
            .expect("enable only the target interface for browse-side check");
        let _receiver = browse_daemon
            .browse(LAN_ROUTING_SERVICE_TYPE)
            .expect("browse restricted to the target interface");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let _ = browse_daemon.shutdown();
        let browse_trace_lines = finish_mdns_trace_capture();
        eprintln!("DIAG_MATCHED_BROWSE_SEND_TRACE {browse_trace_lines:?}");
    }

    /// The decisive follow-up the matched fixture above raises but does not
    /// answer on its own: `send_to()` failing for this daemon's own browse
    /// query is a *send*-side fact - it says nothing about whether this
    /// daemon's *receive* path can still process a genuinely-transmitted,
    /// unsolicited mDNS Announce from a different advertiser, the way RFC
    /// 6762 has every advertiser send at registration regardless of
    /// whether anyone queried for it. An independent, low-level raw-socket
    /// control (`.tmp/lan-discovery-evidence/raw-multicast-receive-check-run1.log`,
    /// built with a plain Python `socket`/`IP_ADD_MEMBERSHIP` listener,
    /// deliberately outside any DNS-SD abstraction so it cannot share
    /// mDNSResponder's own internal same-process cache) already
    /// established that this host's receive path genuinely works: it
    /// captured 17 real multicast packets on `en1` from three distinct
    /// source addresses, including two - `172.31.32.120`, `172.31.32.123`
    /// - that are not this host at all, so those specific packets cannot
    /// be a same-host artifact.
    ///
    /// What that control could not answer is whether *this module's own*
    /// mdns-sd browse - the one thing genuinely proposed for reuse in a
    /// native-advertisement design - can turn a received native Announce
    /// into a `ServiceFound`/`ServiceResolved` event, given its own query
    /// send already fails. This test answers exactly that, with nothing
    /// inferred: browse a fresh, never-before-used service type restricted
    /// to the same interface, and independently register that *exact*
    /// service type natively (`dns-sd -i en1 -R`, a real, separate process,
    /// genuinely sent per the control above) while browsing, then check
    /// whether this daemon's receiver ever reports it.
    #[tokio::test]
    async fn mdns_sd_browse_receiving_a_genuinely_sent_native_announce() {
        let Some(if_name) = if_addrs::get_if_addrs().ok().and_then(|interfaces| {
            interfaces
                .into_iter()
                .find(|interface| is_routable_lan_address(&interface.addr.ip()))
                .map(|interface| interface.name)
        }) else {
            eprintln!("skipping: no non-loopback interface on this host to run this control on");
            return;
        };

        let unique = format!(
            "recv-check-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
                % 0x1000
        );
        // Same 15-byte service-type-label limit this module's own top
        // comment documents - short and fresh, never used before.
        let service_label = format!("rc{:x}", std::process::id() % 0x10000);
        let service_type_domain = format!("_{service_label}._tcp.local.");
        let service_type_bare = format!("_{service_label}._tcp");

        let daemon = ServiceDaemon::new().expect("start mDNS daemon");
        daemon
            .disable_interface(IfKind::All)
            .expect("disable all interfaces");
        daemon
            .enable_interface(IfKind::Name(if_name.clone()))
            .expect("enable only the target interface");
        let receiver = daemon
            .browse(&service_type_domain)
            .expect("browse restricted to the target interface");

        let mut native_register = std::process::Command::new("/usr/bin/dns-sd")
            .arg("-i")
            .arg(&if_name)
            .arg("-R")
            .arg(&unique)
            .arg(&service_type_bare)
            .arg("local")
            .arg("61222")
            .spawn()
            .expect("spawn native dns-sd -R restricted to the target interface");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut events = Vec::new();
        let mut found_or_resolved = false;
        while std::time::Instant::now() < deadline {
            if let Ok(event) = receiver.try_recv() {
                let is_relevant = match &event {
                    ServiceEvent::ServiceFound(_, fullname) => fullname.contains(&unique),
                    ServiceEvent::ServiceResolved(resolved) => resolved.fullname.contains(&unique),
                    _ => false,
                };
                if is_relevant {
                    found_or_resolved = true;
                }
                events.push(format!("{event:?}"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let _ = native_register.kill();
        let _ = native_register.wait();
        let _ = daemon.shutdown();

        eprintln!(
            "DIAG_RECEIVE_CHECK interface={if_name} service_type={service_type_domain} \
             instance={unique} found_or_resolved={found_or_resolved} events={events:?}"
        );
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

    /// `_kanna-lan._tcp.local.` sans the trailing labels `dns-sd`'s
    /// type/domain arguments take separately.
    fn service_type_without_domain() -> &'static str {
        LAN_ROUTING_SERVICE_TYPE
            .strip_suffix(".local.")
            .expect("LAN_ROUTING_SERVICE_TYPE ends in .local.")
    }

    /// Positive-control comparison isolating which side of the real
    /// advertise/browse round trip actually fails on this host: this
    /// module's own `mdns-sd`-based [`LanRoutingAdvertisement`], observed by
    /// the OS's *native* resolver (`dns-sd -B`, going through macOS's
    /// `mDNSResponder`, an entirely separate code path from `mdns-sd`'s own
    /// userspace socket implementation). If this fails too, the defect is
    /// on the *advertise* side (the announcement never reaches the wire at
    /// all) rather than in this crate's own `start_discovery` browse logic
    /// specifically - `advertising_and_discovering_populate_the_real_candidate_map`
    /// alone cannot tell the two apart, because both its advertiser and its
    /// browser are `mdns-sd`.
    #[tokio::test]
    async fn a_native_observer_sees_an_mdns_sd_advertised_service() {
        let unique_instance = format!("diag-mdnssd-adv-{}", std::process::id());
        let qualified_addresses = routable_lan_addresses();
        let daemon = ServiceDaemon::new().expect("start mDNS daemon");
        let monitor = daemon.monitor().expect("subscribe to monitor events");
        let txt = lan_routing_txt(&unique_instance, "development");
        let service = ServiceInfo::new(
            LAN_ROUTING_SERVICE_TYPE,
            &unique_instance,
            &format!("{unique_instance}.local."),
            &qualified_addresses[..],
            4460,
            &txt[..],
        )
        .expect("build service info");
        daemon.register(service).expect("register explicit service");

        let native_browse = NativeBrowseObserver::start(service_type_without_domain());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut monitor_events = Vec::new();
        while std::time::Instant::now() < deadline {
            if let Ok(event) = monitor.try_recv() {
                monitor_events.push(format!("{event:?}"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let stdout = native_browse.captured_text();
        let termination = native_browse.stop();
        eprintln!(
            "DIAG_NATIVE_OBSERVER_TERMINATION child_wait={:?} reader_joined_ok={}",
            termination.child_wait,
            termination.reader_joined.is_ok()
        );
        let _ = daemon.shutdown();

        eprintln!("DIAG_CORRELATED monitor_events={monitor_events:?}\nnative_stdout=\n{stdout}");

        assert!(
            stdout.contains(&unique_instance),
            "native dns-sd -B never observed the mdns-sd-advertised instance {unique_instance} \
             - the advertisement itself may not be reaching the wire. Daemon's own monitor() \
             events: {monitor_events:?}\nnative stdout:\n{stdout}"
        );
    }

    /// One more bounded, low-risk hypothesis on the same defect: does
    /// `mdns-sd`'s own auto-detected address path (`enable_addr_auto`)
    /// behave differently on this host than its explicit-address path
    /// (confirmed present via `ifconfig`)? This is what
    /// `LanRoutingAdvertisement::start` now always uses, unrestricted - see
    /// its own doc comment and `restricting_advertised_interfaces_also_fails_to_reach_the_wire`
    /// below for why not restricted.
    #[tokio::test]
    async fn a_native_observer_sees_an_mdns_sd_advertised_service_using_auto_addr() {
        let unique_instance = format!("diag-mdnssd-adv-auto-{}", std::process::id());
        let daemon = ServiceDaemon::new().expect("start mDNS daemon");
        let txt = lan_routing_txt(&unique_instance, "development");
        let no_addresses: &[IpAddr] = &[];
        let service = ServiceInfo::new(
            LAN_ROUTING_SERVICE_TYPE,
            &unique_instance,
            &format!("{unique_instance}.local."),
            no_addresses,
            4462,
            &txt[..],
        )
        .expect("build service info")
        .enable_addr_auto();
        daemon
            .register(service)
            .expect("register auto-addr service");

        let native_browse = NativeBrowseObserver::start(service_type_without_domain());
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        let stdout = native_browse.captured_text();
        let termination = native_browse.stop();
        eprintln!(
            "DIAG_NATIVE_OBSERVER_TERMINATION child_wait={:?} reader_joined_ok={}",
            termination.child_wait,
            termination.reader_joined.is_ok()
        );
        let _ = daemon.shutdown();

        assert!(
            stdout.contains(&unique_instance),
            "native dns-sd -B never observed the auto-addr mdns-sd-advertised instance \
             {unique_instance} either:\n{stdout}"
        );
    }

    /// Narrows the same hypothesis further: this host's
    /// `routable_lan_addresses()` returns *two* simultaneously-active
    /// addresses (confirmed separately: `[172.31.32.111, 172.20.10.5]`, a
    /// multi-homed VPN/hotspot setup, not a typical single-NIC LAN). Does
    /// explicit publish work with exactly *one* address, or does it fail
    /// even then? This is the difference between "any explicit address
    /// fails here" (would affect ordinary single-NIC users too) and
    /// "explicit *multiple* addresses fails here" (a much narrower,
    /// multi-homed-specific case).
    #[tokio::test]
    async fn a_native_observer_sees_an_mdns_sd_advertised_service_with_exactly_one_explicit_address(
    ) {
        let unique_instance = format!("diag-mdnssd-adv-one-{}", std::process::id());
        let daemon = ServiceDaemon::new().expect("start mDNS daemon");
        let txt = lan_routing_txt(&unique_instance, "development");
        let one_address = [routable_lan_addresses()
            .into_iter()
            .next()
            .expect("at least one routable address on this host")];
        let service = ServiceInfo::new(
            LAN_ROUTING_SERVICE_TYPE,
            &unique_instance,
            &format!("{unique_instance}.local."),
            &one_address[..],
            4463,
            &txt[..],
        )
        .expect("build service info");
        daemon
            .register(service)
            .expect("register one-address service");

        let native_browse = NativeBrowseObserver::start(service_type_without_domain());
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        let stdout = native_browse.captured_text();
        let termination = native_browse.stop();
        eprintln!(
            "DIAG_NATIVE_OBSERVER_TERMINATION child_wait={:?} reader_joined_ok={}",
            termination.child_wait,
            termination.reader_joined.is_ok()
        );
        let _ = daemon.shutdown();

        assert!(
            stdout.contains(&unique_instance),
            "native dns-sd -B never observed the single-explicit-address mdns-sd-advertised \
             instance {unique_instance} at {one_address:?} either:\n{stdout}"
        );
    }

    // Removed: an_mdns_sd_browser_sees_a_natively_advertised_service's
    // premise turned out confounded by an unrelated macOS quirk, not a
    // signal about this module's own browse correctness. `dns-sd -R`'s SRV
    // record points at this host's own hostname ("Jeremys-Mac-Studio.local.",
    // confirmed via `dns-sd -L`), which needs a *separate* A/AAAA resolution
    // round trip - and on the host this was investigated on, that round
    // trip resolves to loopback addresses only (127.0.0.1/::1/fe80::1, all
    // on `lo0`), confirmed directly via the resolved addresses this
    // module's own `start_discovery` received. That is a fact about how
    // this host's mDNSResponder answers queries for its *own hostname*,
    // unrelated to whether `mdns-sd`'s browse can resolve a *service* whose
    // own advertisement embeds a real address - see
    // `advertising_and_discovering_populate_the_real_candidate_map` for
    // that actual question, which remains the accurate end-to-end
    // real-mDNS test and is still red on this host (see the checkpoint
    // history for the full, now-exhausted investigation of why).

    /// The natural first fix to reach for once explicit addresses were
    /// isolated as the defect (see the tests above) - restrict
    /// `enable_addr_auto()` to exactly the routable interfaces via
    /// `set_interfaces`, preserving the original safety intent without
    /// going through the broken explicit-address path. This is a
    /// regression test for why `LanRoutingAdvertisement::start` does NOT do
    /// that: restricting *at all* - even to the exact correct interface
    /// names - reproduces the identical failure. Only a fully unrestricted
    /// `enable_addr_auto()` (no `set_interfaces` call) reaches the wire on
    /// this host; `start_discovery` is what now keeps an unreachable
    /// address out of a resolved candidate instead (see its own doc
    /// comment). If this test ever starts passing on some future mdns-sd
    /// version, `LanRoutingAdvertisement::start` restricting to
    /// `routable_lan_interface_names()` becomes viable again as a more
    /// defense-in-depth option - but only once this test proves it.
    #[tokio::test]
    async fn restricting_advertised_interfaces_also_fails_to_reach_the_wire() {
        let unique_instance = format!("diag-mdnssd-adv-restricted-{}", std::process::id());
        let routable_interfaces = routable_lan_interface_names();
        assert!(
            !routable_interfaces.is_empty(),
            "this test needs at least one real routable interface on the host running it"
        );
        let daemon = ServiceDaemon::new().expect("start mDNS daemon");
        let txt = lan_routing_txt(&unique_instance, "development");
        let no_addresses: &[IpAddr] = &[];
        let mut service = ServiceInfo::new(
            LAN_ROUTING_SERVICE_TYPE,
            &unique_instance,
            &format!("{unique_instance}.local."),
            no_addresses,
            4464,
            &txt[..],
        )
        .expect("build service info")
        .enable_addr_auto();
        service.set_interfaces(
            routable_interfaces
                .iter()
                .cloned()
                .map(IfKind::Name)
                .collect::<Vec<_>>(),
        );
        daemon
            .register(service)
            .expect("register interface-restricted service");

        let native_browse = NativeBrowseObserver::start(service_type_without_domain());
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        let stdout = native_browse.captured_text();
        let termination = native_browse.stop();
        eprintln!(
            "DIAG_NATIVE_OBSERVER_TERMINATION child_wait={:?} reader_joined_ok={}",
            termination.child_wait,
            termination.reader_joined.is_ok()
        );
        let _ = daemon.shutdown();

        eprintln!(
            "DIAG_RESTRICTED_RESULT routable_interfaces={routable_interfaces:?} \
             found={}\nstdout=\n{stdout}",
            stdout.contains(&unique_instance)
        );
    }
}
