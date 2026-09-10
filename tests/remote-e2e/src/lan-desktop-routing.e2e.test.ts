import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { localProcessFetch } from "@kanna/local-process-fetch";
import { BUFFY_UID } from "./firebaseAuth";
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
});

interface MachineInvokeResult {
  status: number;
  body: unknown;
  error: string | null;
  route: "local" | "lan" | "relay";
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

/** Publishes a real `desktopCredentials` Firestore document for
 * `desktopId`, uid Buffy - the same shape and same emulator write
 * `associateDesktopCloudCredential` performs in production, verified
 * against the existing proven pattern in
 * cloud-pairing-auth-discovery.e2e.test.ts's `publishDesktopCredentialAsBuffy`. */
async function publishDesktopCredentialAsBuffy(
  harness: RemoteHarness,
  input: { desktopId: string; desktopSecret: string; displayName: string }
): Promise<void> {
  const idToken = await harness.getIdToken();
  const body = {
    fields: {
      desktopId: { stringValue: input.desktopId },
      displayName: { stringValue: input.displayName },
      desktopSecretHash: { stringValue: sha256Hex(input.desktopSecret) },
      revokedAt: { nullValue: null },
      uid: { stringValue: BUFFY_UID },
      updatedAt: { stringValue: new Date().toISOString() }
    }
  };
  const response = await fetch(
    `${firestoreBaseUrl(harness)}/desktopCredentials/${desktopDocId(input.desktopId)}`,
    {
      method: "PATCH",
      headers: {
        Authorization: `Bearer ${idToken}`,
        "Content-Type": "application/json"
      },
      body: JSON.stringify(body)
    }
  );
  if (!response.ok) {
    throw new Error(
      `failed to publish desktop credential as Buffy: ${response.status} ${await response.text()}`
    );
  }
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
