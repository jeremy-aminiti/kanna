import { localProcessFetch, type LocalProcessFetch } from "@kanna/local-process-fetch";

export interface DesktopPairingSession {
  code: string;
  desktopId: string;
  desktopName: string;
  pairingPayload: string;
  lanHost: string;
  lanPort: number;
  expiresAtUnixMs: number;
}

export async function createDesktopPairingSession(
  baseUrl: string,
  fetchImpl: LocalProcessFetch = localProcessFetch,
): Promise<DesktopPairingSession> {
  const response = await fetchImpl(`${baseUrl}/v1/pairing/sessions`, {
    method: "POST",
  });
  const body = await response.text();
  if (!response.ok) {
    throw new Error(`desktop pairing failed: ${response.status}${body ? ` ${body}` : ""}`);
  }
  const pairing = JSON.parse(body) as Partial<DesktopPairingSession>;
  if (!pairing.code || !pairing.desktopId || !pairing.desktopName) {
    throw new Error("desktop pairing returned an incomplete response");
  }
  return pairing as DesktopPairingSession;
}

export interface DesktopPushIdentity {
  publicKey: string;
  relayUrl: string;
  environment: string;
}

export interface PushPairingCertificate {
  deviceId: string;
  [key: string]: unknown;
}

export interface DesktopPairingClaim {
  desktopId: string;
  desktopName: string;
  deviceSecret: string;
  desktopPushIdentity: DesktopPushIdentity;
  pushPairingCert: PushPairingCertificate;
}

/** Completes a pairing session as the mobile side would: `pairing.rs`'s
 * `claim_pairing_session` is the only production path that turns a
 * created-but-unclaimed `PairingSession` into a persisted `TrustedDevice` in
 * `PairingStore` - no existing e2e test calls it, every prior test stopped
 * at session creation. Single-use: the server clears its in-memory active
 * session on a successful claim, so calling this twice for the same code
 * fails on the second call. */
export async function claimDesktopPairingSession(
  baseUrl: string,
  input: { code: string; deviceId: string; deviceName: string },
  fetchImpl: LocalProcessFetch = localProcessFetch,
): Promise<DesktopPairingClaim> {
  const response = await fetchImpl(`${baseUrl}/v1/pairing/sessions/claim`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      code: input.code,
      deviceId: input.deviceId,
      deviceName: input.deviceName,
    }),
  });
  const body = await response.text();
  if (!response.ok) {
    throw new Error(`desktop pairing claim failed: ${response.status}${body ? ` ${body}` : ""}`);
  }
  const claim = JSON.parse(body) as Partial<DesktopPairingClaim>;
  if (!claim.desktopId || !claim.deviceSecret) {
    throw new Error("desktop pairing claim returned an incomplete response");
  }
  return claim as DesktopPairingClaim;
}
