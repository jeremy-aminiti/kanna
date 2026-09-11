use super::{ObservationBook, Resolution, ServiceKey, LAN_ROUTING_SERVICE_TYPE};
use crate::bonjour::{supervise_native_dns_sd, NativeDnsSdRunResult};
use crate::http_api::AppState;
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

type DnsServiceRef = *mut c_void;
type DnsServiceFlags = u32;
type DnsServiceError = i32;

type BrowseReply = unsafe extern "C" fn(
    DnsServiceRef,
    DnsServiceFlags,
    u32,
    DnsServiceError,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);
type ResolveReply = unsafe extern "C" fn(
    DnsServiceRef,
    DnsServiceFlags,
    u32,
    DnsServiceError,
    *const c_char,
    *const c_char,
    u16,
    u16,
    *const u8,
    *mut c_void,
);
type AddrInfoReply = unsafe extern "C" fn(
    DnsServiceRef,
    DnsServiceFlags,
    u32,
    DnsServiceError,
    *const c_char,
    *const libc::sockaddr,
    u32,
    *mut c_void,
);
type QueryRecordReply = unsafe extern "C" fn(
    DnsServiceRef,
    DnsServiceFlags,
    u32,
    DnsServiceError,
    *const c_char,
    u16,
    u16,
    u16,
    *const c_void,
    u32,
    *mut c_void,
);

const DNS_SERVICE_FLAGS_ADD: DnsServiceFlags = 0x2;
const DNS_SERVICE_ERR_NO_ERROR: DnsServiceError = 0;
const DNS_SERVICE_ERR_NO_SUCH_RECORD: DnsServiceError = -65_554;
const DNS_SERVICE_ERR_SERVICE_NOT_RUNNING: DnsServiceError = -65_563;
const DNS_SERVICE_ERR_DEFUNCT_CONNECTION: DnsServiceError = -65_569;
const DNS_SERVICE_PROTOCOL_IPV4: u32 = 0x1;
const DNS_SERVICE_PROTOCOL_IPV6: u32 = 0x2;
const DNS_TYPE_TXT: u16 = 16;
const DNS_TYPE_SRV: u16 = 33;
const DNS_CLASS_IN: u16 = 1;
const POLL_INTERVAL_MS: i32 = 250;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

unsafe extern "C" {
    fn DNSServiceBrowse(
        service_ref: *mut DnsServiceRef,
        flags: DnsServiceFlags,
        interface_index: u32,
        registration_type: *const c_char,
        domain: *const c_char,
        callback: BrowseReply,
        context: *mut c_void,
    ) -> DnsServiceError;
    fn DNSServiceResolve(
        service_ref: *mut DnsServiceRef,
        flags: DnsServiceFlags,
        interface_index: u32,
        name: *const c_char,
        registration_type: *const c_char,
        domain: *const c_char,
        callback: ResolveReply,
        context: *mut c_void,
    ) -> DnsServiceError;
    fn DNSServiceGetAddrInfo(
        service_ref: *mut DnsServiceRef,
        flags: DnsServiceFlags,
        interface_index: u32,
        protocols: u32,
        hostname: *const c_char,
        callback: AddrInfoReply,
        context: *mut c_void,
    ) -> DnsServiceError;
    fn DNSServiceQueryRecord(
        service_ref: *mut DnsServiceRef,
        flags: DnsServiceFlags,
        interface_index: u32,
        fullname: *const c_char,
        record_type: u16,
        record_class: u16,
        callback: QueryRecordReply,
        context: *mut c_void,
    ) -> DnsServiceError;
    fn DNSServiceRefSockFD(service_ref: DnsServiceRef) -> libc::c_int;
    fn DNSServiceProcessResult(service_ref: DnsServiceRef) -> DnsServiceError;
    fn DNSServiceRefDeallocate(service_ref: DnsServiceRef);
}

enum Event {
    Browse {
        added: bool,
        key: ServiceKey,
    },
    BrowseFailed(DnsServiceError),
    Resolved {
        key: ServiceKey,
        generation: u64,
        interface_index: u32,
        error: DnsServiceError,
        fullname: String,
        host: String,
        port: u16,
        txt: HashMap<String, String>,
    },
    Address {
        key: ServiceKey,
        generation: u64,
        interface_index: u32,
        error: DnsServiceError,
        address: Option<IpAddr>,
        added: bool,
    },
    RecordChanged {
        key: ServiceKey,
        browse_generation: u64,
        error: DnsServiceError,
        added: bool,
    },
}

enum EventPrelude {
    Process,
    Ignore,
    Retry(String),
}

struct BrowseContext {
    events: mpsc::Sender<Event>,
}

struct ResolveContext {
    events: mpsc::Sender<Event>,
    key: ServiceKey,
    generation: u64,
}

struct AddressContext {
    events: mpsc::Sender<Event>,
    key: ServiceKey,
    generation: u64,
}

struct QueryContext {
    events: mpsc::Sender<Event>,
    key: ServiceKey,
    browse_generation: u64,
    seen_initial_value: AtomicBool,
}

unsafe extern "C" fn browse_callback(
    _service_ref: DnsServiceRef,
    flags: DnsServiceFlags,
    interface_index: u32,
    error: DnsServiceError,
    name: *const c_char,
    registration_type: *const c_char,
    domain: *const c_char,
    context: *mut c_void,
) {
    if context.is_null() {
        return;
    }
    // SAFETY: The owning NativeRef keeps this context alive while callbacks
    // can run and deallocates the DNSServiceRef before freeing it.
    let context = unsafe { &*(context.cast::<BrowseContext>()) };
    if error != DNS_SERVICE_ERR_NO_ERROR {
        let _ = context.events.send(Event::BrowseFailed(error));
        return;
    }
    let _ = context.events.send(Event::Browse {
        added: flags & DNS_SERVICE_FLAGS_ADD != 0,
        key: ServiceKey {
            // SAFETY: DNSServiceBrowse supplies callback-owned C strings for
            // the duration of this invocation.
            name: unsafe { callback_string(name) },
            registration_type: unsafe { callback_string(registration_type) },
            domain: unsafe { callback_string(domain) },
            interface_index,
        },
    });
}

unsafe extern "C" fn resolve_callback(
    _service_ref: DnsServiceRef,
    _flags: DnsServiceFlags,
    interface_index: u32,
    error: DnsServiceError,
    _fullname: *const c_char,
    host: *const c_char,
    port: u16,
    txt_len: u16,
    txt_record: *const u8,
    context: *mut c_void,
) {
    if context.is_null() {
        return;
    }
    // SAFETY: See browse_callback; this context has the same ownership.
    let context = unsafe { &*(context.cast::<ResolveContext>()) };
    let txt = if error != DNS_SERVICE_ERR_NO_ERROR || txt_record.is_null() || txt_len == 0 {
        HashMap::new()
    } else {
        // SAFETY: DNSServiceResolve guarantees txt_len readable bytes for the
        // duration of the callback.
        parse_txt(unsafe { std::slice::from_raw_parts(txt_record, txt_len as usize) })
    };
    let _ = context.events.send(Event::Resolved {
        key: context.key.clone(),
        generation: context.generation,
        interface_index,
        error,
        // The remaining callback fields are undefined on an error.
        fullname: if error == DNS_SERVICE_ERR_NO_ERROR {
            unsafe { callback_string(_fullname) }
        } else {
            String::new()
        },
        host: if error == DNS_SERVICE_ERR_NO_ERROR {
            unsafe { callback_string(host) }
        } else {
            String::new()
        },
        port: if error == DNS_SERVICE_ERR_NO_ERROR {
            u16::from_be(port)
        } else {
            0
        },
        txt,
    });
}

unsafe extern "C" fn address_callback(
    _service_ref: DnsServiceRef,
    flags: DnsServiceFlags,
    interface_index: u32,
    error: DnsServiceError,
    _host: *const c_char,
    address: *const libc::sockaddr,
    _ttl: u32,
    context: *mut c_void,
) {
    if context.is_null() {
        return;
    }
    // SAFETY: See browse_callback; this context has the same ownership.
    let context = unsafe { &*(context.cast::<AddressContext>()) };
    let _ = context.events.send(Event::Address {
        key: context.key.clone(),
        generation: context.generation,
        interface_index,
        error,
        // SAFETY: DNSServiceGetAddrInfo supplies a sockaddr matching sa_family
        // for the duration of this callback.
        address: (error == DNS_SERVICE_ERR_NO_ERROR)
            .then(|| unsafe { sockaddr_ip(address) })
            .flatten(),
        added: error == DNS_SERVICE_ERR_NO_ERROR && flags & DNS_SERVICE_FLAGS_ADD != 0,
    });
}

unsafe extern "C" fn query_callback(
    _service_ref: DnsServiceRef,
    flags: DnsServiceFlags,
    _interface_index: u32,
    error: DnsServiceError,
    _fullname: *const c_char,
    _record_type: u16,
    _record_class: u16,
    _record_len: u16,
    _record_data: *const c_void,
    _ttl: u32,
    context: *mut c_void,
) {
    if context.is_null() {
        return;
    }
    // SAFETY: See browse_callback; this context has the same ownership.
    let context = unsafe { &*(context.cast::<QueryContext>()) };
    // Callback fields other than the error are undefined on failure.
    let added = error == DNS_SERVICE_ERR_NO_ERROR && flags & DNS_SERVICE_FLAGS_ADD != 0;
    let initial = context.seen_initial_value.swap(true, Ordering::AcqRel);
    if initial || !added || error != DNS_SERVICE_ERR_NO_ERROR {
        let _ = context.events.send(Event::RecordChanged {
            key: context.key.clone(),
            browse_generation: context.browse_generation,
            error,
            added,
        });
    }
}

unsafe fn callback_string(value: *const c_char) -> String {
    if value.is_null() {
        return String::new();
    }
    // SAFETY: The caller guarantees a callback-owned NUL-terminated string.
    unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .into_owned()
}

unsafe fn sockaddr_ip(address: *const libc::sockaddr) -> Option<IpAddr> {
    if address.is_null() {
        return None;
    }
    // SAFETY: The callback contract guarantees the header is readable.
    match unsafe { (*address).sa_family as libc::c_int } {
        libc::AF_INET => {
            // SAFETY: sa_family identifies sockaddr_in.
            let address = unsafe { &*address.cast::<libc::sockaddr_in>() };
            Some(IpAddr::V4(Ipv4Addr::from(
                address.sin_addr.s_addr.to_ne_bytes(),
            )))
        }
        libc::AF_INET6 => {
            // SAFETY: sa_family identifies sockaddr_in6.
            let address = unsafe { &*address.cast::<libc::sockaddr_in6>() };
            Some(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}

fn parse_txt(bytes: &[u8]) -> HashMap<String, String> {
    let mut properties = HashMap::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let length = bytes[offset] as usize;
        offset += 1;
        let Some(end) = offset.checked_add(length).filter(|end| *end <= bytes.len()) else {
            break;
        };
        if let Ok(entry) = std::str::from_utf8(&bytes[offset..end]) {
            if let Some((key, value)) = entry.split_once('=') {
                properties.insert(key.to_string(), value.to_string());
            }
        }
        offset = end;
    }
    properties
}

struct NativeRef<C> {
    raw: DnsServiceRef,
    context: *mut C,
}

impl<C> NativeRef<C> {
    fn socket(&self) -> Result<libc::c_int, String> {
        // SAFETY: raw is live for this owner's lifetime.
        let socket = unsafe { DNSServiceRefSockFD(self.raw) };
        (socket >= 0)
            .then_some(socket)
            .ok_or_else(|| "macOS DNS-SD operation has no event socket".to_string())
    }
}

impl<C> Drop for NativeRef<C> {
    fn drop(&mut self) {
        // Deallocation synchronously prevents later callbacks. Only then may
        // the callback context be freed.
        unsafe { DNSServiceRefDeallocate(self.raw) };
        unsafe { drop(Box::from_raw(self.context)) };
    }
}

struct ServiceResources {
    browse_generation: u64,
    resolution_generation: u64,
    resolve: Option<NativeRef<ResolveContext>>,
    address: Option<NativeRef<AddressContext>>,
    srv_watch: Option<NativeRef<QueryContext>>,
    txt_watch: Option<NativeRef<QueryContext>>,
}

pub struct Discovery {
    stop: SyncSender<()>,
    worker: Option<JoinHandle<()>>,
}

impl Discovery {
    pub(super) fn start(state: Arc<AppState>) -> Result<Self, String> {
        let (stop, stop_receiver) = mpsc::sync_channel(1);
        let (startup, startup_receiver) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("kanna-lan-routing-discovery".to_string())
            .spawn(move || supervise(state, stop_receiver, startup))
            .map_err(|error| format!("failed to start LAN routing discovery thread: {error}"))?;

        match startup_receiver.recv_timeout(STARTUP_TIMEOUT) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => Ok(Self {
                stop,
                worker: Some(worker),
            }),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = worker.join();
                Err("LAN routing discovery stopped during startup".to_string())
            }
        }
    }
}

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

fn supervise(state: Arc<AppState>, stop: Receiver<()>, startup: SyncSender<()>) {
    let environment = state.config().environment.clone();
    let mut observations = ObservationBook::default();
    supervise_native_dns_sd(
        &stop,
        RETRY_INTERVAL,
        || {
            observations.clear();
            observations.project(&state, &environment);
            browse_once(&state, &environment, &stop, &startup, &mut observations)
        },
        |error| log::warn!("LAN routing discovery failed: {error}; retrying"),
    );
    observations.clear();
    observations.project(&state, &environment);
}

fn browse_once(
    state: &AppState,
    environment: &str,
    stop: &Receiver<()>,
    startup: &SyncSender<()>,
    observations: &mut ObservationBook,
) -> NativeDnsSdRunResult {
    let (events, receiver) = mpsc::channel();
    let browse = match start_browse(events.clone()) {
        Ok(value) => value,
        Err(error) => return NativeDnsSdRunResult::Retry(error),
    };
    let _ = startup.try_send(());
    let mut resources: HashMap<ServiceKey, ServiceResources> = HashMap::new();

    loop {
        if matches!(
            stop.try_recv(),
            Ok(()) | Err(mpsc::TryRecvError::Disconnected)
        ) {
            return NativeDnsSdRunResult::Stopped;
        }

        let mut refs: Vec<DnsServiceRef> = vec![browse.raw];
        for resource in resources.values() {
            if let Some(resolve) = &resource.resolve {
                refs.push(resolve.raw);
            }
            if let Some(address) = &resource.address {
                refs.push(address.raw);
            }
            if let Some(srv_watch) = &resource.srv_watch {
                refs.push(srv_watch.raw);
            }
            if let Some(txt_watch) = &resource.txt_watch {
                refs.push(txt_watch.raw);
            }
        }
        let mut pollfds = Vec::with_capacity(refs.len());
        for service_ref in &refs {
            let socket = unsafe { DNSServiceRefSockFD(*service_ref) };
            if socket < 0 {
                return NativeDnsSdRunResult::Retry(
                    "macOS DNS-SD operation lost its event socket".to_string(),
                );
            }
            pollfds.push(libc::pollfd {
                fd: socket,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        // SAFETY: pollfds is a live contiguous array for this call.
        let result = unsafe {
            libc::poll(
                pollfds.as_mut_ptr(),
                pollfds.len() as libc::nfds_t,
                POLL_INTERVAL_MS,
            )
        };
        if result < 0 {
            return NativeDnsSdRunResult::Retry(std::io::Error::last_os_error().to_string());
        }
        for (descriptor, service_ref) in pollfds.iter().zip(refs) {
            let terminal = descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL);
            if terminal != 0 {
                return NativeDnsSdRunResult::Retry(format!(
                    "macOS mDNSResponder connection reported terminal poll events 0x{terminal:x}"
                ));
            }
            if descriptor.revents & libc::POLLIN != 0 {
                // SAFETY: Each ref is live and exclusively processed here.
                let error = unsafe { DNSServiceProcessResult(service_ref) };
                if error != DNS_SERVICE_ERR_NO_ERROR {
                    return NativeDnsSdRunResult::Retry(dns_error(
                        "process discovery result",
                        error,
                    ));
                }
            }
        }

        while let Ok(event) = receiver.try_recv() {
            match prepare_operation_event(&event, state, environment, observations, &mut resources)
            {
                EventPrelude::Process => {}
                EventPrelude::Ignore => continue,
                EventPrelude::Retry(error) => return NativeDnsSdRunResult::Retry(error),
            }
            match event {
                Event::Browse { added: true, key } => {
                    let generation = observations.begin(key.clone());
                    observations.project(state, environment);
                    let resolve = match start_resolve(events.clone(), key.clone(), generation) {
                        Ok(value) => value,
                        Err(error) => return NativeDnsSdRunResult::Retry(error),
                    };
                    resources.insert(
                        key,
                        ServiceResources {
                            browse_generation: generation,
                            resolution_generation: generation,
                            resolve: Some(resolve),
                            address: None,
                            srv_watch: None,
                            txt_watch: None,
                        },
                    );
                }
                Event::Browse { added: false, key } => {
                    resources.remove(&key);
                    observations.withdraw(&key);
                    observations.project(state, environment);
                }
                Event::BrowseFailed(error) => {
                    return NativeDnsSdRunResult::Retry(dns_error("browse", error));
                }
                Event::Resolved {
                    key,
                    generation,
                    interface_index,
                    error,
                    fullname,
                    host,
                    port,
                    txt,
                } => {
                    // DNSServiceResolveReply defines interface_index only for
                    // successful callbacks. Generation/context validity was
                    // already checked by prepare_operation_event, including
                    // failure cleanup that must not consult this payload.
                    if interface_index != key.interface_index {
                        continue;
                    }
                    debug_assert_eq!(error, DNS_SERVICE_ERR_NO_ERROR);
                    let resource = resources
                        .get_mut(&key)
                        .expect("current resolution has live resources");
                    observations.resolve(
                        &key,
                        generation,
                        Resolution {
                            desktop_id: txt.get("desktopId").cloned(),
                            environment: txt.get("environment").cloned(),
                            protocol_version: txt.get("protocolVersion").cloned(),
                            port,
                        },
                    );
                    observations.project(state, environment);
                    match start_address(events.clone(), key.clone(), generation, &host) {
                        Ok(address) => resource.address = Some(address),
                        Err(error) => return NativeDnsSdRunResult::Retry(error),
                    }
                    if resource.srv_watch.is_none() {
                        match start_query(
                            events.clone(),
                            key.clone(),
                            resource.browse_generation,
                            &fullname,
                            DNS_TYPE_SRV,
                        ) {
                            Ok(watch) => resource.srv_watch = Some(watch),
                            Err(error) => return NativeDnsSdRunResult::Retry(error),
                        }
                    }
                    if resource.txt_watch.is_none() {
                        match start_query(
                            events.clone(),
                            key.clone(),
                            resource.browse_generation,
                            &fullname,
                            DNS_TYPE_TXT,
                        ) {
                            Ok(watch) => resource.txt_watch = Some(watch),
                            Err(error) => return NativeDnsSdRunResult::Retry(error),
                        }
                    }
                }
                Event::Address {
                    key,
                    generation,
                    interface_index,
                    error,
                    address,
                    added,
                } => {
                    // As with resolve callbacks, address callback payload is
                    // meaningful only after a successful error code.
                    if interface_index != key.interface_index {
                        continue;
                    }
                    debug_assert_eq!(error, DNS_SERVICE_ERR_NO_ERROR);
                    if let Some(address) = address {
                        observations.address(&key, generation, address, added);
                        observations.project(state, environment);
                    }
                }
                Event::RecordChanged {
                    key,
                    browse_generation,
                    error,
                    added,
                } => {
                    let current = resources
                        .get(&key)
                        .is_some_and(|resource| resource.browse_generation == browse_generation);
                    if !current {
                        continue;
                    }
                    if error != DNS_SERVICE_ERR_NO_ERROR && error != DNS_SERVICE_ERR_NO_SUCH_RECORD
                    {
                        if responder_failed(error) {
                            return NativeDnsSdRunResult::Retry(dns_error(
                                "observe service records",
                                error,
                            ));
                        }
                        discard_failed_service(
                            state,
                            environment,
                            observations,
                            &mut resources,
                            &key,
                            "observe service records",
                            error,
                        );
                        continue;
                    }
                    let resource = resources
                        .get_mut(&key)
                        .expect("current record watcher has live resources");
                    let generation = observations.begin(key.clone());
                    resource.resolution_generation = generation;
                    resource.address = None;
                    resource.resolve = None;
                    observations.project(state, environment);
                    if added {
                        match start_resolve(events.clone(), key.clone(), generation) {
                            Ok(resolve) => resource.resolve = Some(resolve),
                            Err(error) => return NativeDnsSdRunResult::Retry(error),
                        }
                    }
                }
            }
        }
    }
}

/// Fence resolve/address callbacks by their Rust-owned context before reading
/// any callback payload. Apple's DNS-SD contract leaves fields such as the
/// interface index undefined when `errorCode` is nonzero, while the context's
/// service key and generation remain ours and identify the operation that
/// failed.
fn prepare_operation_event(
    event: &Event,
    state: &AppState,
    environment: &str,
    observations: &mut ObservationBook,
    resources: &mut HashMap<ServiceKey, ServiceResources>,
) -> EventPrelude {
    let (key, generation, error, action) = match event {
        Event::Resolved {
            key,
            generation,
            error,
            ..
        } => (key, *generation, *error, "resolve service"),
        Event::Address {
            key,
            generation,
            error,
            ..
        } => (key, *generation, *error, "observe service address"),
        _ => return EventPrelude::Process,
    };
    let current = resources.get(key).is_some_and(|resource| {
        resource.resolution_generation == generation
            && observations.current_generation(key) == Some(generation)
    });
    if !current {
        return EventPrelude::Ignore;
    }
    if error == DNS_SERVICE_ERR_NO_ERROR {
        return EventPrelude::Process;
    }
    if responder_failed(error) {
        return EventPrelude::Retry(dns_error(action, error));
    }
    discard_failed_service(
        state,
        environment,
        observations,
        resources,
        key,
        action,
        error,
    );
    EventPrelude::Ignore
}

fn responder_failed(error: DnsServiceError) -> bool {
    matches!(
        error,
        DNS_SERVICE_ERR_SERVICE_NOT_RUNNING | DNS_SERVICE_ERR_DEFUNCT_CONNECTION
    )
}

fn discard_failed_service(
    state: &AppState,
    environment: &str,
    observations: &mut ObservationBook,
    resources: &mut HashMap<ServiceKey, ServiceResources>,
    key: &ServiceKey,
    action: &str,
    error: DnsServiceError,
) {
    log::warn!(
        "{}; discarding incomplete LAN service {:?}",
        dns_error(action, error),
        key
    );
    resources.remove(key);
    observations.withdraw(key);
    observations.project(state, environment);
}

fn start_browse(events: mpsc::Sender<Event>) -> Result<NativeRef<BrowseContext>, String> {
    let registration_type = CString::new(
        LAN_ROUTING_SERVICE_TYPE
            .trim_end_matches(".local.")
            .to_string(),
    )
    .map_err(|error| format!("invalid LAN routing service type: {error}"))?;
    let domain = CString::new("local.").expect("static Bonjour domain has no NUL");
    let context = Box::into_raw(Box::new(BrowseContext { events }));
    let mut service_ref = ptr::null_mut();
    let error = unsafe {
        DNSServiceBrowse(
            &mut service_ref,
            0,
            0,
            registration_type.as_ptr(),
            domain.as_ptr(),
            browse_callback,
            context.cast(),
        )
    };
    native_ref(service_ref, context, "browse", error)
}

fn start_resolve(
    events: mpsc::Sender<Event>,
    key: ServiceKey,
    generation: u64,
) -> Result<NativeRef<ResolveContext>, String> {
    let name = CString::new(key.name.as_str())
        .map_err(|error| format!("invalid discovered service name: {error}"))?;
    let registration_type = CString::new(key.registration_type.as_str())
        .map_err(|error| format!("invalid discovered service type: {error}"))?;
    let domain = CString::new(key.domain.as_str())
        .map_err(|error| format!("invalid discovered service domain: {error}"))?;
    let context = Box::into_raw(Box::new(ResolveContext {
        events,
        key: key.clone(),
        generation,
    }));
    let mut service_ref = ptr::null_mut();
    let error = unsafe {
        DNSServiceResolve(
            &mut service_ref,
            0,
            key.interface_index,
            name.as_ptr(),
            registration_type.as_ptr(),
            domain.as_ptr(),
            resolve_callback,
            context.cast(),
        )
    };
    native_ref(service_ref, context, "resolve service", error)
}

fn start_address(
    events: mpsc::Sender<Event>,
    key: ServiceKey,
    generation: u64,
    host: &str,
) -> Result<NativeRef<AddressContext>, String> {
    let host = CString::new(host).map_err(|error| format!("invalid resolved hostname: {error}"))?;
    let context = Box::into_raw(Box::new(AddressContext {
        events,
        key: key.clone(),
        generation,
    }));
    let mut service_ref = ptr::null_mut();
    let error = unsafe {
        DNSServiceGetAddrInfo(
            &mut service_ref,
            0,
            key.interface_index,
            DNS_SERVICE_PROTOCOL_IPV4 | DNS_SERVICE_PROTOCOL_IPV6,
            host.as_ptr(),
            address_callback,
            context.cast(),
        )
    };
    native_ref(service_ref, context, "observe service address", error)
}

fn start_query(
    events: mpsc::Sender<Event>,
    key: ServiceKey,
    browse_generation: u64,
    fullname: &str,
    record_type: u16,
) -> Result<NativeRef<QueryContext>, String> {
    let fullname = CString::new(fullname)
        .map_err(|error| format!("invalid discovered service fullname: {error}"))?;
    let context = Box::into_raw(Box::new(QueryContext {
        events,
        key: key.clone(),
        browse_generation,
        seen_initial_value: AtomicBool::new(false),
    }));
    let mut service_ref = ptr::null_mut();
    let error = unsafe {
        DNSServiceQueryRecord(
            &mut service_ref,
            0,
            key.interface_index,
            fullname.as_ptr(),
            record_type,
            DNS_CLASS_IN,
            query_callback,
            context.cast(),
        )
    };
    native_ref(service_ref, context, "observe service records", error)
}

fn native_ref<C>(
    service_ref: DnsServiceRef,
    context: *mut C,
    action: &str,
    error: DnsServiceError,
) -> Result<NativeRef<C>, String> {
    if error != DNS_SERVICE_ERR_NO_ERROR {
        // SAFETY: The operation failed synchronously, so no callback owns it.
        unsafe { drop(Box::from_raw(context)) };
        return Err(dns_error(action, error));
    }
    if service_ref.is_null() {
        // A successful DNS-SD call promises an initialized reference. Keep
        // this defensive boundary on the Rust side before invoking another
        // FFI function with an invalid handle.
        unsafe { drop(Box::from_raw(context)) };
        return Err(format!(
            "failed to {action} through macOS mDNSResponder (missing DNS-SD reference)"
        ));
    }
    let reference = NativeRef {
        raw: service_ref,
        context,
    };
    reference.socket()?;
    Ok(reference)
}

fn dns_error(action: &str, error: DnsServiceError) -> String {
    format!("failed to {action} through macOS mDNSResponder (DNS-SD error {error})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn parses_multiple_txt_properties_and_ignores_a_truncated_tail() {
        let bytes = b"\x0bdesktopId=x\x17environment=development\x11protocolVersion=1\x08broken";
        let parsed = parse_txt(bytes);
        assert_eq!(parsed.get("desktopId").map(String::as_str), Some("x"));
        assert_eq!(
            parsed.get("environment").map(String::as_str),
            Some("development")
        );
        assert_eq!(parsed.get("protocolVersion").map(String::as_str), Some("1"));
        assert!(!parsed.contains_key("broken"));
    }

    #[test]
    fn supervisor_retries_a_failed_generation() {
        let (_stop_sender, stop_receiver) = mpsc::sync_channel(1);
        let attempts = Arc::new(Mutex::new(0));
        let observed = Arc::clone(&attempts);
        supervise_native_dns_sd(
            &stop_receiver,
            Duration::ZERO,
            move || {
                let mut attempts = observed.lock().unwrap();
                *attempts += 1;
                if *attempts == 1 {
                    NativeDnsSdRunResult::Retry("responder disconnected".to_string())
                } else {
                    NativeDnsSdRunResult::Stopped
                }
            },
            |_| {},
        );
        assert_eq!(*attempts.lock().unwrap(), 2);
    }

    #[test]
    fn cancellation_interrupts_recovery_wait() {
        let (stop_sender, stop_receiver) = mpsc::sync_channel(1);
        let (attempted, attempted_receiver) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            supervise_native_dns_sd(
                &stop_receiver,
                Duration::from_secs(60),
                || {
                    let _ = attempted.try_send(());
                    NativeDnsSdRunResult::Retry("responder disconnected".to_string())
                },
                |_| {},
            );
        });
        attempted_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("first generation attempted");
        stop_sender.send(()).unwrap();
        worker.join().unwrap();
    }

    fn projected_service(
        desktop_id: &str,
    ) -> (
        Arc<AppState>,
        ObservationBook,
        HashMap<ServiceKey, ServiceResources>,
        ServiceKey,
        u64,
    ) {
        let state = crate::http_api::test_state_with_seed(desktop_id, "Native callback", |_| {});
        let key = ServiceKey {
            name: "native-callback".to_string(),
            registration_type: LAN_ROUTING_SERVICE_TYPE.to_string(),
            domain: "local.".to_string(),
            interface_index: 27,
        };
        let mut observations = ObservationBook::default();
        let generation = observations.begin(key.clone());
        assert!(observations.resolve(
            &key,
            generation,
            Resolution {
                desktop_id: Some(desktop_id.to_string()),
                environment: Some("development".to_string()),
                protocol_version: Some(
                    crate::lan_discovery::LAN_ROUTING_PROTOCOL_VERSION.to_string()
                ),
                port: 4460,
            },
        ));
        assert!(observations.address(&key, generation, IpAddr::from([192, 168, 1, 20]), true,));
        observations.project(&state, "development");
        let resources = HashMap::from([(
            key.clone(),
            ServiceResources {
                browse_generation: generation,
                resolution_generation: generation,
                resolve: None,
                address: None,
                srv_watch: None,
                txt_watch: None,
            },
        )]);
        (state, observations, resources, key, generation)
    }

    #[test]
    fn current_resolve_error_with_undefined_interface_cleans_up_projected_service() {
        let (state, mut observations, mut resources, key, generation) =
            projected_service("desktop-current-error");
        assert!(state.lan_candidate_for("desktop-current-error").is_some());
        let (events, receiver) = mpsc::channel();
        let mut context = ResolveContext {
            events,
            key: key.clone(),
            generation,
        };

        // Fault-inject the actual native callback shape: on error, the SDK is
        // allowed to report interface zero and every payload pointer may be
        // undefined/null. The callback must carry its owned context through
        // to the event owner without inspecting those fields.
        unsafe {
            resolve_callback(
                ptr::null_mut(),
                0,
                0,
                -65_537,
                ptr::null(),
                ptr::null(),
                0,
                0,
                ptr::null(),
                (&mut context as *mut ResolveContext).cast(),
            );
        }
        let event = receiver.recv().expect("resolve callback event");
        assert!(matches!(
            prepare_operation_event(
                &event,
                &state,
                "development",
                &mut observations,
                &mut resources,
            ),
            EventPrelude::Ignore
        ));

        assert!(!resources.contains_key(&key));
        assert!(state.lan_candidate_for("desktop-current-error").is_none());
    }

    #[test]
    fn stale_address_error_with_undefined_interface_keeps_replacement_generation() {
        let (state, mut observations, mut resources, key, stale_generation) =
            projected_service("desktop-stale-error");
        let replacement_generation = observations.begin(key.clone());
        assert!(observations.resolve(
            &key,
            replacement_generation,
            Resolution {
                desktop_id: Some("desktop-replacement".to_string()),
                environment: Some("development".to_string()),
                protocol_version: Some(
                    crate::lan_discovery::LAN_ROUTING_PROTOCOL_VERSION.to_string()
                ),
                port: 4461,
            },
        ));
        assert!(observations.address(
            &key,
            replacement_generation,
            IpAddr::from([192, 168, 1, 21]),
            true,
        ));
        observations.project(&state, "development");
        resources.get_mut(&key).unwrap().resolution_generation = replacement_generation;
        let (events, receiver) = mpsc::channel();
        let mut stale_context = AddressContext {
            events,
            key: key.clone(),
            generation: stale_generation,
        };

        unsafe {
            address_callback(
                ptr::null_mut(),
                0,
                0,
                -65_537,
                ptr::null(),
                ptr::null(),
                0,
                (&mut stale_context as *mut AddressContext).cast(),
            );
        }
        let event = receiver.recv().expect("address callback event");
        assert!(matches!(
            prepare_operation_event(
                &event,
                &state,
                "development",
                &mut observations,
                &mut resources,
            ),
            EventPrelude::Ignore
        ));

        assert!(resources.contains_key(&key));
        assert_eq!(
            state.lan_candidate_for("desktop-replacement"),
            Some("192.168.1.21:4461".parse().unwrap()),
        );
    }
}
