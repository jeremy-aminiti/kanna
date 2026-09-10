import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { localProcessFetch } from "@kanna/local-process-fetch";
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
 * `attempt_lan_invoke` are what have to do all the work here, exactly as
 * production does - this suite seeds nothing.
 */
describe("LAN-first desktop-to-desktop routing E2E", () => {
  let harness: RemoteHarness;

  beforeAll(async () => {
    harness = await startRemoteHarness();
  }, 180_000);

  afterAll(async () => {
    await harness?.stop();
  });

  it(
    "bootstraps trust from an empty store through real discovery, then dials the peer over LAN instead of relay",
    async () => {
      // Both desktops start with genuinely empty machine-trust stores: no
      // seeded grant, no seeded LAN candidate.
      const peer = await harness.startAdditionalDesktop();
      try {
        // The very first call has nothing to work with yet - no candidate,
        // no grant - so it must still succeed, by definite fallback to
        // relay, while that same fallback kicks off a background bootstrap
        // attempt (`invoke_desktop::maybe_trigger_lan_bootstrap`).
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
