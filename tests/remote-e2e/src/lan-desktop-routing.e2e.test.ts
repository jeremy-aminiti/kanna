import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { localProcessFetch } from "@kanna/local-process-fetch";
import {
  BUFFY_UID,
  OTHER_ACCOUNT_EMAIL,
  OTHER_ACCOUNT_PASSWORD,
  OTHER_ACCOUNT_UID,
  signInWithPassword
} from "./firebaseAuth";
import { startRemoteHarness, type RemoteDesktop, type RemoteHarness } from "./harness";
import { waitForCondition } from "./terminalFlowTestUtils";

/**
 * Production-path coverage for LAN-first same-account desktop-to-desktop
 * routing (`invoke_desktop::invoke_desktop`), through two real, isolated
 * `kanna-server` listeners sharing one real relay and Firebase emulator -
 * not the manually-seeded trust/candidate fixtures the unit tests in
 * `invoke_desktop.rs` use to exercise the same fallback/uncertainty
 * contract in isolation. Real Bonjour advertise/discover
 * (`lan_discovery.rs`) and the on-demand bootstrap trigger inside
 * `attempt_lan_invoke` are what have to do all the work here.
 *
 * This suite never seeds a machine_trust grant or LAN candidate directly -
 * that is exactly the thing under test. It does provision each desktop with
 * a real, same-account signed-in identity the same way a genuine desktop
 * gets one: a real `desktopCredentials` Firestore document (mirroring
 * `associateDesktopCloudCredential`'s own write, verified against the
 * existing proven pattern in cloud-pairing-auth-discovery.e2e.test.ts) plus
 * a matching `desktop_secret` in that desktop's own config. Without this,
 * every desktop here is `desktop_secret: None`, which is not how any real
 * signed-in desktop is ever configured (`generate_desktop_secret()` runs
 * unconditionally on first launch) and materially changes account-auth
 * behavior in `relay.rs`'s reconnection loop.
 */
describe("LAN-first desktop-to-desktop routing E2E", () => {
  let harness: RemoteHarness;

  beforeAll(async () => {
    // Real discovery (`lan_discovery::routable_lan_addresses`) advertises
    // this host's actual routable interface address, never loopback - the
    // same address a genuine second machine on the LAN would need. A real
    // desktop's own config generator binds its LAN listener to "0.0.0.0"
    // unconditionally (apps/desktop/src-tauri/src/commands/mobile/config.rs),
    // never the harness's own "127.0.0.1" default, so this suite must match
    // that or the discovered candidate is structurally unreachable no
    // matter how correct bootstrap/trust is.
    harness = await startRemoteHarness({ lanHost: "0.0.0.0" });
    await signInDesktopAsBuffy(harness, harness.desktopId);
    await harness.waitForDesktop(harness.desktopId);
  }, 180_000);

  afterAll(async () => {
    await harness?.stop();
  });

  it(
    "bootstraps trust from an empty store through real discovery, then dials the peer over LAN instead of relay",
    async () => {
      const peer = await startSameAccountPeer(harness, "empty-store-bootstrap");
      try {
        // Both desktops start with genuinely empty machine-trust stores: no
        // seeded grant, no seeded LAN candidate. The very first call has
        // nothing to work with yet, so it must still succeed, by definite
        // fallback to relay, while that same fallback kicks off a
        // background bootstrap attempt
        // (`invoke_desktop::maybe_trigger_lan_bootstrap`).
        const first = await invokeMachine(harness, peer.desktopId, "/v1/status");
        expect(first.status).toBe(200);
        expect(first.route).toBe("relay");

        // Poll the identical operation until real discovery has found a
        // candidate and the background bootstrap has produced an attested
        // trust grant - at which point the call must switch to a real,
        // pinned-TLS LAN dial.
        await waitForCondition(
          async () => {
            const response = await invokeMachine(harness, peer.desktopId, "/v1/status");
            expect(response.status).toBe(200);
            return response.route === "lan";
          },
          60_000,
          `${harness.desktopId} never reached ${peer.desktopId} over LAN`
        );

        // The same production path from the other direction: the peer
        // desktop reaching back through its own store, discovery and
        // bootstrap - LAN eligibility is symmetric, not a one-way fixture.
        await waitForCondition(
          async () => {
            const response = await invokeMachine(peer, harness.desktopId, "/v1/status");
            expect(response.status).toBe(200);
            return response.route === "lan";
          },
          60_000,
          `${peer.desktopId} never reached ${harness.desktopId} over LAN`
        );
      } finally {
        await peer.stop();
      }
    },
    120_000
  );

  /**
   * `request_bootstrap`/`bootstrap_lan_trust` (lan_bootstrap.rs) establish
   * trust entirely over the already-authenticated relay round trip -
   * `state.invoke_relay_desktop`, not `lan_candidate_for` or any Bonjour
   * state. This is deliberately a separate, narrower test from the one
   * above: it proves the real bootstrap-over-relay production path
   * (currently held: LAN discovery in this environment - see the
   * checkpoint/commit history) independent of whether real mDNS discovery
   * ever resolves a candidate. It reads the actual on-disk
   * machine-trust.json each side wrote, never seeding it.
   */
  it(
    "establishes a real trust grant over relay-based bootstrap, independent of LAN candidate discovery",
    async () => {
      const peer = await startSameAccountPeer(harness, "relay-bootstrap");
      try {
        // Same trigger as the empty-store test: any invoke with no existing
        // grant kicks off `maybe_trigger_lan_bootstrap` in the background,
        // regardless of whether a LAN candidate has ever been discovered.
        await invokeMachine(harness, peer.desktopId, "/v1/status");

        await waitForCondition(
          async () => {
            const store = await readMachineTrustStore(harness);
            return store?.outbound.some(
              (grant) =>
                grant.targetDesktopId === peer.desktopId && grant.accountUid === BUFFY_UID
            ) ?? false;
          },
          30_000,
          `${harness.desktopId} never recorded a real outbound bootstrap grant for ${peer.desktopId}`
        );

        await waitForCondition(
          async () => {
            const store = await readMachineTrustStore(peer);
            return store?.inbound.some(
              (grant) =>
                grant.sourceDesktopId === harness.desktopId && grant.accountUid === BUFFY_UID
            ) ?? false;
          },
          30_000,
          `${peer.desktopId} never recorded a real inbound bootstrap grant from ${harness.desktopId}`
        );
      } finally {
        await peer.stop();
      }
    },
    60_000
  );

  /**
   * The invariant `reconcile_machine_trust_for_account`'s own doc comment
   * names: "Preserve eligible leases on ordinary outage." An outbound
   * grant already established over relay must survive relay going away -
   * `eligible_lan_desktop_ids`/`attempt_lan_invoke` read the trust store
   * directly, never relay presence, for this decision. Provisioning both
   * desktops with a real `desktop_secret` (see `startSameAccountPeer`) is
   * what makes this provable at all: without it, `signed_out_or_rejected`
   * in `relay.rs`'s reconnection loop is unconditionally true, wiping trust
   * on every reconnect regardless of whether it was a real sign-out - see
   * the checkpoint history for that trace. Not gated by the LAN-discovery
   * defect: this only checks the trust store's own persisted state, never
   * an actual LAN dial.
   */
  it(
    "keeps an already-established outbound grant through a relay outage",
    async () => {
      const peer = await startSameAccountPeer(harness, "relay-outage-lease");
      try {
        await invokeMachine(harness, peer.desktopId, "/v1/status");
        await waitForCondition(
          async () => {
            const store = await readMachineTrustStore(harness);
            return store?.outbound.some(
              (grant) =>
                grant.targetDesktopId === peer.desktopId && grant.accountUid === BUFFY_UID
            ) ?? false;
          },
          30_000,
          `${harness.desktopId} never recorded a real outbound bootstrap grant for ${peer.desktopId}`
        );

        await harness.stopRelay();
        try {
          // Long enough for the reconnection loop to notice the drop and
          // retry at least once (RELAY_RECONNECT_DELAY is 5s) - exactly the
          // window where an incorrectly-forced signed-out reconciliation
          // would wipe the grant.
          await sleep(8_000);
          const store = await readMachineTrustStore(harness);
          const stillHasGrant =
            store?.outbound.some(
              (grant) =>
                grant.targetDesktopId === peer.desktopId && grant.accountUid === BUFFY_UID
            ) ?? false;
          expect(stillHasGrant).toBe(true);
        } finally {
          await harness.startRelay();
          // Leave the harness's own server actually reconnected before this
          // test returns - the reconnection loop backs off for several
          // seconds after `stopRelay()`, and a later test dialing through
          // `harness.lanBaseUrl` during that window gets a raw transport
          // failure ("Connection refused") instead of the logical relay
          // response it expects, which is a test-ordering artifact of this
          // outage simulation, not anything the next test is actually
          // proving.
          const logOffset = harness.serverLogs().length;
          await waitForCondition(
            async () => harness.serverLogs().slice(logOffset).includes("Authenticated with relay"),
            30_000,
            `${harness.desktopId} never reconnected to relay after this test's simulated outage`
          );
        }
      } finally {
        await peer.stop();
      }
    },
    60_000
  );

  /**
   * Wrong-account rejection. `services/relay/src/router.ts`'s `routeMessage`
   * looks up `connections.get(userId)` before doing anything else - a
   * desktop authenticated as a different account lives in a completely
   * separate `pair.desktops` map, structurally invisible to a same-account
   * caller's routing regardless of desktop_id. No Firebase Auth signup is
   * needed to prove this: `services/relay/src/auth.ts`'s
   * `readDesktopCredentials` resolves account identity purely from the
   * `desktopCredentials` Firestore document's own `uid` field, matched
   * against the presented `desktop_secret` hash - so a real, differently-
   * uid'd credential document is what actually determines which account a
   * desktop authenticates as, independent of whether that uid has ever
   * signed into anything.
   */
  it(
    "never routes to, or lists, a desktop authenticated under a different account",
    async () => {
      const peer = await startPeerForAccount(harness, "wrong-account");
      try {
        // Real relay routing, not a fabricated assertion: an actual invoke
        // attempt against a real desktop_id relay itself will never find
        // in the caller's own account pair.
        const response = await localProcessFetch(
          `${harness.lanBaseUrl}/v1/cloud/desktops/${encodeURIComponent(peer.desktopId)}/invoke`,
          {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ method: "GET", path: "/v1/status", body: null })
          }
        );
        // `invoke_desktop` returns `Err(String)` (a plain-text, non-2xx HTTP
        // response) for a transport-level relay failure, and `Ok` (200,
        // JSON body carrying its own embedded `status`) for a relay-level
        // rejection like "Desktop offline" - both shapes are "did not
        // succeed," so only parse JSON when the outer response claims 2xx.
        if (response.ok) {
          const body = await response.json() as { status: number; error: string | null };
          // invoke_relay_desktop surfaces relay's "Desktop offline" as a
          // definite, non-2xx result - the same shape a genuinely offline
          // desktop would produce, which is exactly the point: this account
          // has no way to distinguish "wrong account" from "not connected,"
          // because the desktop is never in its own routing table at all.
          expect(body.status).not.toBe(200);
        }

        // Confirm the negative from the other observable direction too:
        // the wrong-account desktop must never appear in this account's own
        // active-desktop listing, the same enumeration eligible_lan_desktop_ids
        // and every fan-out consumer in finding #1 reads.
        const activeIds = await harness.client.listActiveDesktopIds();
        expect(Array.from(activeIds)).not.toContain(peer.desktopId);
      } finally {
        await peer.stop();
      }
    },
    60_000
  );
});

interface MachineInvokeResult {
  status: number;
  body: unknown;
  error: string | null;
  route: "local" | "lan" | "relay";
}

interface MachineTrustGrant {
  targetDesktopId?: string;
  sourceDesktopId?: string;
  accountUid: string;
}

interface MachineTrustStoreSnapshot {
  inbound: MachineTrustGrant[];
  outbound: MachineTrustGrant[];
}

/** Reads the real, on-disk `machine-trust.json` `request_bootstrap`/
 * `bootstrap_lan_trust` themselves write - never seeded by this suite.
 * `Config::machine_trust_store_path` derives it as a `machine-trust.json`
 * sibling of `pairing_store_path`, which the harness always places at
 * `<daemonDir>/pairings.json`. Absent (not yet written) reads as an empty
 * store rather than an error - a genuine race with the background
 * bootstrap task, not a failure. */
async function readMachineTrustStore(
  desktop: Pick<RemoteHarness, "paths"> | Pick<RemoteDesktop, "paths">
): Promise<MachineTrustStoreSnapshot | null> {
  const daemonDir = "daemonDir" in desktop.paths ? desktop.paths.daemonDir : join(desktop.paths.root, "daemon");
  try {
    const raw = await readFile(join(daemonDir, "machine-trust.json"), "utf8");
    return JSON.parse(raw) as MachineTrustStoreSnapshot;
  } catch {
    return null;
  }
}

async function invokeMachine(
  from: Pick<RemoteHarness, "lanBaseUrl"> | Pick<RemoteDesktop, "lanBaseUrl">,
  targetDesktopId: string,
  path: string
): Promise<MachineInvokeResult> {
  const response = await localProcessFetch(
    `${from.lanBaseUrl}/v1/cloud/desktops/${encodeURIComponent(targetDesktopId)}/invoke`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ method: "GET", path, body: null })
    }
  );
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`machine invoke HTTP ${response.status}: ${text}`);
  }
  return JSON.parse(text) as MachineInvokeResult;
}

/** Deterministic per-desktop secret - a test fixture value, not a real
 * cryptographic device secret, matching the existing established pattern
 * in cloud-pairing-auth-discovery.e2e.test.ts. Its exact bytes do not
 * matter to kanna-server: `desktop_secret`'s only role at that layer is
 * presence (gates whether account-auth is attempted at all) and identity
 * (must match what a `desktopCredentials` document's hash was computed
 * from), not any particular derivation. */
function desktopSecretFor(desktopId: string): string {
  return sha256Hex(`${desktopId}:secret`);
}

function sha256Hex(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

function desktopDocId(desktopId: string): string {
  return desktopId.replace(/\//g, "_");
}

function firestoreBaseUrl(harness: RemoteHarness): string {
  return `http://127.0.0.1:${harness.ports.firestore}/v1/projects/kanna-local/databases/(default)/documents`;
}

/** Publishes a real `desktopCredentials` Firestore document for `desktopId`
 * under `uid` - the same shape and same emulator write
 * `associateDesktopCloudCredential` performs in production, verified against
 * the existing proven pattern in
 * cloud-pairing-auth-discovery.e2e.test.ts's `publishDesktopCredentialAsBuffy`.
 * `idToken` must belong to `uid`: the emulator's own Firestore rules require
 * `request.resource.data.uid == request.auth.uid`, so a real credential
 * document's account is determined by whoever signs the write, not by
 * whatever `uid` value is asked for here. */
async function publishDesktopCredential(
  harness: RemoteHarness,
  input: { desktopId: string; desktopSecret: string; displayName: string; uid: string; idToken: string }
): Promise<void> {
  const body = {
    fields: {
      desktopId: { stringValue: input.desktopId },
      displayName: { stringValue: input.displayName },
      desktopSecretHash: { stringValue: sha256Hex(input.desktopSecret) },
      revokedAt: { nullValue: null },
      uid: { stringValue: input.uid },
      updatedAt: { stringValue: new Date().toISOString() }
    }
  };
  const response = await fetch(
    `${firestoreBaseUrl(harness)}/desktopCredentials/${desktopDocId(input.desktopId)}`,
    {
      method: "PATCH",
      headers: {
        Authorization: `Bearer ${input.idToken}`,
        "Content-Type": "application/json"
      },
      body: JSON.stringify(body)
    }
  );
  if (!response.ok) {
    throw new Error(
      `failed to publish desktop credential for uid ${input.uid}: ${response.status} ${await response.text()}`
    );
  }
}

async function publishDesktopCredentialAsBuffy(
  harness: RemoteHarness,
  input: { desktopId: string; desktopSecret: string; displayName: string }
): Promise<void> {
  const idToken = await harness.getIdToken();
  await publishDesktopCredential(harness, { ...input, uid: BUFFY_UID, idToken });
}

/** Signs the harness's own (already-running) desktop into the shared Buffy
 * account for real: publishes its Firestore credential, then restarts it
 * with a matching `desktop_secret` - the same sequence
 * cloud-pairing-auth-discovery.e2e.test.ts already proves works. */
async function signInDesktopAsBuffy(harness: RemoteHarness, desktopId: string): Promise<void> {
  const desktopSecret = desktopSecretFor(desktopId);
  await publishDesktopCredentialAsBuffy(harness, {
    desktopId,
    desktopSecret,
    displayName: "LAN Routing E2E Desktop"
  });
  await harness.restartServerWithIdentity({ desktopId, desktopSecret });
}

/** Starts a second, genuinely separate `kanna-server`/daemon pair already
 * signed into the same Buffy account as `harness` - the peer's Firestore
 * credential and `desktop_secret` are established *before* the peer's
 * process ever starts, exactly as a real desktop would already be signed
 * in before this suite's own on-demand LAN bootstrap ever gets a chance to
 * run. Its machine_trust store and LAN candidate list start genuinely
 * empty - only account identity is provisioned here, never LAN trust
 * itself. */
async function startSameAccountPeer(harness: RemoteHarness, label: string): Promise<RemoteDesktop> {
  const desktopId = `desktop-lan-peer-${label}-${Date.now()}`;
  const desktopSecret = desktopSecretFor(desktopId);
  await publishDesktopCredentialAsBuffy(harness, {
    desktopId,
    desktopSecret,
    displayName: `LAN Routing E2E Peer (${label})`
  });
  return await harness.startAdditionalDesktop({ desktopId, desktopSecret });
}

/** Starts a second, genuinely separate `kanna-server`/daemon pair signed into
 * the real, seeded `OTHER_ACCOUNT_UID` account - a completely different
 * account from `harness`'s Buffy, not a fabricated uid: Firestore's own
 * security rules require `request.resource.data.uid == request.auth.uid` on
 * a `desktopCredentials` write, so proving cross-account isolation for real
 * requires a real second signed-in identity to author that document, not
 * just an arbitrary string.
 *
 * Cannot reuse `startAdditionalDesktop`'s default relay-visibility wait,
 * which polls through `harness`'s own (Buffy) relay client -
 * `waitForRelayVisibility: false` skips it, and this instead waits for the
 * peer's own server log to report a real, successful relay authentication
 * under its own account, independent of Buffy's client ever observing it. */
async function startPeerForAccount(harness: RemoteHarness, label: string): Promise<RemoteDesktop> {
  const desktopId = `desktop-lan-peer-${label}-${Date.now()}`;
  const desktopSecret = desktopSecretFor(desktopId);
  const signIn = await signInWithPassword({
    authPort: harness.ports.auth,
    email: OTHER_ACCOUNT_EMAIL,
    password: OTHER_ACCOUNT_PASSWORD
  });
  if (!signIn.idToken) {
    throw new Error(`failed to sign in as the other seeded account: ${signIn.failure ?? "no idToken"}`);
  }
  if (signIn.localId && signIn.localId !== OTHER_ACCOUNT_UID) {
    throw new Error(`other-account auth seed resolved unexpected uid ${signIn.localId}`);
  }
  await publishDesktopCredential(harness, {
    desktopId,
    desktopSecret,
    displayName: `LAN Routing E2E Peer (${label})`,
    uid: OTHER_ACCOUNT_UID,
    idToken: signIn.idToken
  });
  const peer = await harness.startAdditionalDesktop({
    desktopId,
    desktopSecret,
    waitForRelayVisibility: false
  });
  await waitForCondition(
    async () => peer.serverLogs().includes("Authenticated with relay"),
    30_000,
    `${desktopId} never authenticated with relay under the other account`
  );
  return peer;
}
