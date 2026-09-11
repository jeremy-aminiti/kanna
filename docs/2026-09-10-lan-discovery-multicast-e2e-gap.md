# LAN Discovery Cross-Host E2E Gap

Same-host production discovery is now covered on macOS through the native
DNS-SD adapter: `bonjour_multi_process.rs` starts separate `kanna-server`
processes, verifies the exact `_kanna-lan._tcp.local.` SRV/TXT contract,
starts a browser after the advertisement is already live, observes a
non-loopback candidate on the listener's actual bound port, and verifies
withdrawal without disturbing mobile Bonjour. The remote E2E then starts
two isolated servers with empty automatic-trust stores, bootstraps through
the real relay, proves bidirectional authenticated LAN calls, and drives
`kanna_info` through the real CLI with `route: "lan"` provenance.

The remaining gap is narrower: this environment has not exercised two
physical Macs on the same LAN segment. Same-host mDNSResponder coverage
proves the production producer, browser, resolution, address observation,
and cleanup contracts, but cannot by itself prove multicast interoperability
across a particular network's AP isolation, firewall, or multicast policy.

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

The investigation history remains useful context: the prior `mdns-sd`
producer and browser both hit `EHOSTUNREACH` on this host's `en1` interface,
and passive cache browsing did not yield a usable candidate. Raw evidence is
retained under `.tmp/lan-discovery-evidence/`. Those failures motivated the
bounded macOS adapter; they are no longer production assertions. Non-macOS
continues to use `mdns-sd`, whose behavior is unchanged.

Related coverage, all through real production paths:

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
- `lan-desktop-routing.e2e.test.ts` › "bootstraps trust from an empty store
  through real discovery..." proves automatic discovery, relay bootstrap,
  bidirectional LAN routing, and real CLI provenance as one capstone.
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
