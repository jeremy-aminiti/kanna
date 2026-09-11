# LAN Discovery Multicast E2E Gap

Two accepted scenarios for LAN-first same-account desktop-to-desktop
routing cannot be proven end-to-end on this development machine: empty-
store bootstrap through real mDNS discovery
(`tests/remote-e2e/src/lan-desktop-routing.e2e.test.ts`), and real CLI/MCP
provenance reporting `"lan"` for a candidate a real, separately-spawned
`kanna-server` process actually discovered on its own. The HTTP/JSON
contract itself is proven correct
(`cloud_desktops::tests::invoke_cloud_desktop_reports_lan_route_provenance_in_its_real_http_response`
calls the real `/v1/cloud/desktops/{id}/invoke` handler, the same one
`crates/kanna-cli`/`crates/kanna-mcp` actually talk to, and asserts a real
JSON response of `route: "lan"`) - only the *discovery-populated* case,
as opposed to one seeded in-process by a test, remains unsatisfied.

An earlier draft of this note also listed dropped-reply-exactly-once/
`delivery_uncertain`/no-replay, fake-discovery/pinned-TLS rejection, and
encrypted-proxy-bytes as blocked by the same defect. That was wrong: none
of the three actually need real discovery to resolve anything - they need
only an authenticated candidate *address*, and discovery is only one of
the ways a candidate gets populated, not the only one available to a
test. All three are proven below with real sockets and a real pinned-TLS
identity/handshake, exactly like `invoke_desktop.rs`'s own existing
end-to-end test, just with the candidate seeded explicitly instead of
discovered.

The real blocker, for the two scenarios that remain: `lan_discovery.rs`'s
`mdns-sd`-based advertise/discover never resolves a candidate on this
host, and a matched, same-interface fixture
(`lan_discovery::tests::matched_single_interface_explicit_vs_auto_publish_comparison`,
using `mdns-sd`'s own existing `send_to` trace-log seam, not packet
capture) isolated the exact failure point: every multicast send on this
host's routable interface (`en1`) fails at the OS socket layer with
`EHOSTUNREACH` ("No route to host", errno 65) - identically for IPv4 and
IPv6, for both explicit-address and `enable_addr_auto()` publish, *and*
for `start_discovery`'s own browse-side query send (confirmed by
extending the same fixture with a third, interface-restricted
`daemon.browse()` case using the identical send-trace seam - browse and
publish both funnel through the same `send_dns_outgoing_impl` ->
`multicast_on_intf` code path in `mdns-sd`). This reproduces regardless of
which mdns-sd API is used; the deeper OS reason for the errno remains open
(the routing table for that destination looks ordinary on direct
inspection), and pursuing it further would need packet capture or host
network changes, both out of scope for this task.

A conditionally-authorized native-advertisement adapter (reusing
`bonjour.rs`'s existing libSystem seam) was assessed and **not
implemented**, even though its narrow authorizing criterion (a matched
native control succeeding where correctly-configured mdns-sd fails, same
interface/family/port) is met: since the *browse* side fails identically
and for the same reason, and browsing is not authorized to change, a
publish-side-only native adapter would not achieve real end-to-end
discovery on this host regardless. See `.tmp/lan-revision-checkpoint.md`
(task-local, not committed) for the full evidence chain and reasoning.

Neither remaining scenario can be made testable by seeding a candidate
explicitly the way the three retracted ones were: both are specifically
about a real, separately-spawned server process discovering and reporting
on a candidate *it found itself*, which is exactly the step this host
cannot complete. To make these two testable, the harness needs either a
development host where this OS-level multicast condition does not
reproduce, or a genuine second physical device on the same LAN segment
(never exercised in this environment) to prove or disprove native
delivery independent of mDNSResponder's same-host answering.

Narrower/equivalent coverage added instead, all real production paths:

- `invoke_desktop.rs`'s three new real-socket tests, each a real pinned-TLS
  identity/handshake over a real socket with an explicitly-seeded
  candidate, not a mock:
  `a_dropped_reply_after_a_real_dispatch_is_delivery_uncertain_and_never_replayed_to_relay`
  (a real target genuinely receives the request, then the reply is
  dropped - proves `delivery_uncertain` and exactly-once, no automatic
  replay), `every_byte_a_lan_proxy_observes_is_encrypted_never_plaintext_secrets_or_paths`
  (a transparent byte-capturing proxy in front of the real production
  listener, proving the wire never carries the plaintext bearer secret,
  header name, or path), and
  `a_candidate_presenting_a_different_desktops_real_identity_is_rejected_before_dispatch`
  (a real raw TLS responder presenting a different desktop's real, validly
  -issued identity, proving pinned-TLS rejection before any application
  byte crosses, through the full `attempt_lan_invoke` path), and
  `a_real_lan_invoke_completes_while_relay_is_genuinely_unreachable` (relay
  pointed at a real, actively-refused address rather than merely
  unconfigured, proving an actual LAN dial completes regardless - the
  existing E2E "keeps an already-established outbound grant through a
  relay outage" scenario only reads persisted trust-store state, never
  dials).
- `lan-desktop-routing.e2e.test.ts` › "establishes a real trust grant over
  relay-based bootstrap, independent of LAN candidate discovery" and
  "keeps an already-established outbound grant through a relay outage" -
  the bootstrap-over-relay and lease-persistence halves of the same
  contract, both real, two-server, two-daemon E2E.
- `lan-desktop-routing.e2e.test.ts` › "never routes to, or lists, a
  desktop authenticated under a different account" - real cross-account
  rejection, using a genuine second seeded Firebase identity, not a
  fabricated uid.
- `lan-desktop-routing.e2e.test.ts` › "completes a real mobile pairing
  claim without affecting LAN desktop-to-desktop trust or routing" -
  drives a real pairing claim through production endpoints and proves
  `pairing::PairingStore` and `machine_trust::MachineTrustStore` stay
  independent in both directions.
- `invoke_desktop.rs`'s own real-TLS unit tests
  (`a_real_lan_invoke_completes_over_a_real_tls_socket_end_to_end`, its
  same-address variant, and `a_real_lan_invoke_with_the_wrong_bearer_secret_is_rejected_definitely`)
  prove the pinned-TLS dial and the definite-response-without-fallback
  contract, and (loopback and, separately, the real routable address) that
  the listener/TLS layer itself was never the fault.
- `lan_listener.rs`'s
  `listener_bound_to_all_interfaces_is_reachable_on_a_real_routable_address`
  proves the listener is reachable at this host's real address by plain
  TCP, isolating the remaining fault to discovery/publication specifically.
