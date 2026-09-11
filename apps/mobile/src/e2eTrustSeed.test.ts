import { describe, expect, it, vi } from "vitest";
import {
  claimPairingPayloadFromUrl,
  seedTrustedDesktopFromUrl
} from "./e2eTrustSeed";
import { createSessionPersistence, type StorageAdapter } from "./state/sessionPersistence";

function createMemoryStorage(): StorageAdapter {
  const values = new Map<string, string>();
  return {
    async getItem(key) {
      return values.get(key) ?? null;
    },
    async setItem(key, value) {
      values.set(key, value);
    }
  };
}

describe("seedTrustedDesktopFromUrl", () => {
  it("persists the E2E deep-link trust seed so reload can enable trusted Bonjour", async () => {
    // Full native Bonjour/Appium coverage requires a running signed iOS app,
    // native service discovery permissions, and a desktop LAN server. This
    // boundary test proves the deep-link seed survives the same persistence
    // reload that app bootstrap uses before trusted Bonjour resolution runs.
    const persistence = createSessionPersistence(createMemoryStorage());
    const reload = vi.fn(async () => {
      const reloaded = await persistence.load();
      expect(reloaded?.trustedDesktops).toEqual([
        {
          desktopId: "desktop-e2e",
          displayName: "E2E Mac",
          lanEndpoints: [],
          lastSeenAt: expect.any(String)
        }
      ]);
    });

    await seedTrustedDesktopFromUrl(
      "kanna://e2e-trust?desktopId=desktop-e2e&displayName=E2E%20Mac",
      {
        getPersistence: async () => persistence,
        reload
      }
    );

    expect(reload).toHaveBeenCalledTimes(1);
  });

  it("persists a trusted LAN endpoint and unresolved task selection for hybrid startup", async () => {
    const persistence = createSessionPersistence(createMemoryStorage());
    const reload = vi.fn(async () => {
      const reloaded = await persistence.load();
      expect(reloaded).toMatchObject({
        selectedDesktopId: "desktop-hybrid",
        selectedRepoId: "repo-restored",
        selectedTaskId: "task-unresolved",
        activeView: "tasks",
        trustedDesktops: [
          {
            desktopId: "desktop-hybrid",
            displayName: "Hybrid LAN Desktop",
            lanEndpoints: [
              {
                baseUrl: "http://127.0.0.1:48120",
                lastSeenAt: expect.any(String)
              }
            ],
            lastSeenAt: expect.any(String)
          }
        ]
      });
    });

    await seedTrustedDesktopFromUrl(
      "kanna://e2e-trust?desktopId=desktop-hybrid" +
        "&displayName=Hybrid%20LAN%20Desktop" +
        "&lanBaseUrl=http%3A%2F%2F127.0.0.1%3A48120" +
        "&selectedRepoId=repo-restored" +
        "&selectedTaskId=task-unresolved",
      {
        getPersistence: async () => persistence,
        reload
      }
    );

    expect(reload).toHaveBeenCalledTimes(1);
  });

  it("accepts the current build's own registered scheme, not only the legacy literal", async () => {
    // A real OS-level open (xcrun simctl openurl, Safari, another app) can
    // only route to this app via a scheme it actually registered in
    // Info.plist -- kanna-dev for a dev build, never bare "kanna" (see
    // mobileEnvironments.json). The Appium-driven E2E harness bypasses that
    // resolution entirely (`mobile: deepLink` with an explicit bundle id),
    // which is why it could send literal "kanna://" and still work; a plain
    // URL open cannot.
    const persistence = createSessionPersistence(createMemoryStorage());
    const reload = vi.fn(async () => undefined);

    await seedTrustedDesktopFromUrl(
      "kanna-dev://e2e-trust?desktopId=desktop-dev&displayName=Dev%20Desktop",
      { getPersistence: async () => persistence, reload },
      "kanna-dev"
    );

    expect(reload).toHaveBeenCalledTimes(1);
    const persisted = await persistence.load();
    expect(persisted?.selectedDesktopId).toBe("desktop-dev");
  });

  it("still accepts the legacy literal scheme when a build scheme is supplied", async () => {
    const persistence = createSessionPersistence(createMemoryStorage());
    const reload = vi.fn(async () => undefined);

    await seedTrustedDesktopFromUrl(
      "kanna://e2e-trust?desktopId=desktop-legacy&displayName=Legacy",
      { getPersistence: async () => persistence, reload },
      "kanna-dev"
    );

    expect(reload).toHaveBeenCalledTimes(1);
  });

  it("rejects a scheme that matches neither the legacy literal nor the build's own", async () => {
    const persistence = createSessionPersistence(createMemoryStorage());
    const reload = vi.fn(async () => undefined);

    await seedTrustedDesktopFromUrl(
      "other-app://e2e-trust?desktopId=desktop-dev&displayName=Dev%20Desktop",
      { getPersistence: async () => persistence, reload },
      "kanna-dev"
    );

    expect(reload).not.toHaveBeenCalled();
    expect(await persistence.load()).toBeNull();
  });
});

describe("claimPairingPayloadFromUrl", () => {
  it("sends a simulator QR payload through the controller pairing contract", async () => {
    const pairPayload = vi.fn().mockResolvedValue("desktop-e2e");
    const payload = JSON.stringify({
      type: "kanna.machine-pairing",
      version: 1,
      desktopId: "desktop-e2e",
      code: "ABC123"
    });

    await claimPairingPayloadFromUrl(
      `kanna://e2e-pair?payload=${encodeURIComponent(payload)}`,
      pairPayload
    );

    expect(pairPayload).toHaveBeenCalledWith(payload);
  });

  it("ignores unrelated and incomplete deep links", async () => {
    const pairPayload = vi.fn().mockResolvedValue("desktop-e2e");

    await claimPairingPayloadFromUrl("kanna://e2e-trust", pairPayload);
    await claimPairingPayloadFromUrl("kanna://e2e-pair", pairPayload);

    expect(pairPayload).not.toHaveBeenCalled();
  });

  it("accepts the current build's own registered scheme for a real OS-level open", async () => {
    const pairPayload = vi.fn().mockResolvedValue("desktop-e2e");
    const payload = JSON.stringify({
      type: "kanna.machine-pairing",
      version: 1,
      desktopId: "desktop-e2e",
      code: "ABC123"
    });

    await claimPairingPayloadFromUrl(
      `kanna-dev://e2e-pair?payload=${encodeURIComponent(payload)}`,
      pairPayload,
      "kanna-dev"
    );

    expect(pairPayload).toHaveBeenCalledWith(payload);
  });
});
