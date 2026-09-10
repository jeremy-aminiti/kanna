import type { SessionPersistence } from "./state/sessionPersistence";

interface LinkingLike {
  addEventListener(
    eventName: "url",
    listener: (event: { url: string }) => void
  ): { remove(): void };
  getInitialURL(): Promise<string | null>;
}

interface ReactNativeModule {
  Linking: LinkingLike;
}

declare const require: ((id: string) => ReactNativeModule) | undefined;

export function installE2eTrustSeedHandler(input: {
  getPersistence(): Promise<SessionPersistence>;
  pairPayload(payload: string): Promise<string>;
  reload(): Promise<void>;
  /**
   * This build's own registered URL scheme (e.g. "kanna-dev" for dev,
   * "kanna-staging" for staging, bare "kanna" for prod — see
   * mobileEnvironments.json). Only this scheme, or the exact bundle id
   * Appium's `mobile: deepLink` targets, ever reaches an app's Linking
   * listener without the OS being able to resolve a URL scheme at all: a
   * real OS-level open (`xcrun simctl openurl`, Safari, another app) needs
   * this app to have genuinely registered the scheme, which happens for
   * this value and never for a bare "kanna" on dev/staging. Omit it to
   * accept only the legacy literal below (matches prior behavior exactly).
   */
  scheme?: string;
}): () => void {
  const linking = loadLinking();
  if (!linking) {
    return () => undefined;
  }

  const handleUrl = (url: string) => {
    void Promise.all([
      seedTrustedDesktopFromUrl(url, input, input.scheme),
      claimPairingPayloadFromUrl(url, input.pairPayload, input.scheme)
    ]);
  };
  const subscription = linking.addEventListener("url", (event) => handleUrl(event.url));
  void linking.getInitialURL().then((url) => {
    if (url) {
      handleUrl(url);
    }
  });
  return () => subscription.remove();
}

/**
 * Whether a parsed URL's protocol is one this E2E-only handling may act on.
 *
 * Always accepts the legacy literal "kanna:" the Appium-driven E2E harness
 * has sent since this handler was written
 * (apps/mobile/e2e/helpers/trust-seed.ts, `mobile: deepLink` with an
 * explicit bundle id) — Appium hands the URL straight to the named app and
 * never asks the OS to resolve its scheme, so "kanna:" works there
 * regardless of what is actually registered, and nothing about that harness
 * needs to change. Also accepts `scheme`, the current build's own
 * registered scheme, when the caller supplies one — this is what makes the
 * same deep link reachable through a real OS-level open, which the literal
 * alone cannot be for a dev or staging build (only kanna-dev/kanna-staging/
 * kanna are ever registered in Info.plist, never a bare "kanna" except for
 * prod, where the two already coincide).
 */
function matchesE2eProtocol(protocol: string, scheme: string | undefined): boolean {
  return protocol === "kanna:" || (scheme !== undefined && protocol === `${scheme}:`);
}

export async function claimPairingPayloadFromUrl(
  url: string,
  pairPayload: (payload: string) => Promise<string>,
  scheme?: string
): Promise<void> {
  const parsed = new URL(url);
  if (!matchesE2eProtocol(parsed.protocol, scheme) || parsed.hostname !== "e2e-pair") {
    return;
  }
  const payload = parsed.searchParams.get("payload");
  if (!payload) return;
  await pairPayload(payload);
}

export async function seedTrustedDesktopFromUrl(
  url: string,
  input: {
    getPersistence(): Promise<SessionPersistence>;
    reload(): Promise<void>;
  },
  scheme?: string
): Promise<void> {
  const parsed = new URL(url);
  if (!matchesE2eProtocol(parsed.protocol, scheme) || parsed.hostname !== "e2e-trust") {
    return;
  }
  const desktopId = parsed.searchParams.get("desktopId");
  const displayName = parsed.searchParams.get("displayName");
  if (!desktopId || !displayName) {
    return;
  }

  const persistence = await input.getPersistence();
  const seededAt = new Date().toISOString();
  const lanBaseUrl = parsed.searchParams.get("lanBaseUrl")?.trim() || null;
  await persistence.save({
    mobileDeviceId: null,
    selectedDesktopId: desktopId,
    selectedRepoId: parsed.searchParams.get("selectedRepoId")?.trim() || null,
    selectedTaskId: parsed.searchParams.get("selectedTaskId")?.trim() || null,
    activeView: "tasks",
    trustedDesktops: [
      {
        desktopId,
        displayName,
        lanEndpoints: lanBaseUrl
          ? [{ baseUrl: lanBaseUrl, lastSeenAt: seededAt }]
          : [],
        lastSeenAt: seededAt
      }
    ]
  });
  await input.reload();
}

function loadLinking(): LinkingLike | null {
  try {
    return typeof require === "function" ? require("react-native").Linking : null;
  } catch {
    return null;
  }
}
