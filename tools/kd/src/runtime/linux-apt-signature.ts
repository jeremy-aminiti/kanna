/** Supplied-key OpenPGP adapter. No key discovery, provisioning, filesystem,
 *  subprocesses or publication. Ubuntu apt/GPG interoperability is a separate
 *  acceptance gate; verification here uses the pinned OpenPGP.js Node build. */
import * as openpgp from "openpgp";
import type { AptPublicationSigner } from "./linux-apt-publication";

export interface AptVerificationKey {
  publicKey: string;
  /** Full v4 primary-key fingerprint, never a short key ID or user ID. */
  fingerprint: string;
}

export interface AptSigningKey extends AptVerificationKey {
  privateKey: string;
  passphrase?: string;
  /** Re-evaluated on every sign; a long-lived signer must not freeze validity. */
  now: () => Date;
}

export interface AptReleaseVerification extends AptVerificationKey {
  signedRelease: Uint8Array;
  expectedRelease: Uint8Array;
  now: Date;
}

const MAX_BYTES = 1024 * 1024;
const config: openpgp.Config = {
  ...openpgp.config,
  minRSABits: 3072,
  v6Keys: false,
  preferredHashAlgorithm: openpgp.enums.hash.sha512,
};

class AptSignatureError extends Error {}

function fail(message: string): never {
  throw new AptSignatureError(message);
}

function safeError(error: unknown): Error {
  // Library errors must not expose supplied key material or passphrases.
  return error instanceof AptSignatureError ? error : new AptSignatureError("OpenPGP apt signature operation failed.");
}

function text(bytes: Uint8Array): string {
  if (bytes.byteLength === 0 || bytes.byteLength > MAX_BYTES) fail("Invalid apt signature input size.");
  return new TextDecoder("utf-8", { fatal: true }).decode(bytes).replace(/\r\n/g, "\n");
}

function clock(value: Date): Date {
  const result = new Date(value.getTime());
  if (!Number.isFinite(result.getTime())) fail("Invalid apt verification clock.");
  return result;
}

function fingerprint(value: string): string {
  if (!/^[0-9a-f]{40}$/i.test(value)) fail("A full 40-digit apt key fingerprint is required.");
  return value.toLowerCase();
}

function profile(key: openpgp.Key | openpgp.Subkey): void {
  const algorithm = key.getAlgorithmInfo();
  if (key.keyPacket.version !== 4 || !["rsaEncryptSign", "rsaSign"].includes(algorithm.algorithm) ||
      (algorithm.bits ?? 0) < 3072) {
    fail("Apt signing requires a v4 RSA key of at least 3072 bits.");
  }
}

async function readPinnedKey(input: AptVerificationKey): Promise<openpgp.Key> {
  const expected = fingerprint(input.fingerprint);
  if (Buffer.byteLength(input.publicKey) > MAX_BYTES) fail("Apt public key is too large.");
  const keys = await openpgp.readKeys({ armoredKeys: input.publicKey, config });
  const key = keys[0];
  if (keys.length !== 1 || !key || key.isPrivate()) fail("Exactly one apt public key is required.");
  if (key.getFingerprint() !== expected) fail("Apt public key fingerprint mismatch.");
  profile(key);
  return key;
}

/** Validate the single authenticated Release stanza, never fields from armor.
 *  Require the UTC date form emitted by buildReleaseIndex; Date.parse alone
 *  silently normalizes impossible dates. Reject expiry at equality. */
function validateRelease(release: string, now: Date): void {
  if (!release.endsWith("\n") || /[\r\0]/.test(release) || /[ \t]+$/m.test(release)) {
    fail("Apt Release must use canonical lines without trailing whitespace.");
  }
  const fields = new Map<string, string>();
  let previous = "";
  for (const line of release.slice(0, -1).split("\n")) {
    if (/^[ \t]/.test(line)) {
      if (!previous || ["date", "valid-until"].includes(previous)) fail("Invalid apt Release date continuation.");
      continue;
    }
    const match = /^([A-Za-z0-9][A-Za-z0-9-]*):(?: (.*))?$/.exec(line);
    if (!match) fail("Invalid apt Release field or extra stanza.");
    previous = match[1].toLowerCase();
    if (fields.has(previous)) fail("Duplicate apt Release field.");
    fields.set(previous, match[2] ?? "");
  }
  const dates = ["date", "valid-until"].map((name) => {
    const value = fields.get(name);
    const date = new Date(value ?? "");
    if (!value || !Number.isFinite(date.getTime()) || date.toUTCString() !== value) {
      fail("Apt Release requires valid Date and Valid-Until fields.");
    }
    return date.getTime();
  });
  if (dates[0] > now.getTime()) fail("Apt Release is dated in the future.");
  if (dates[1] <= dates[0] || now.getTime() >= dates[1]) fail("Apt Release metadata has expired or has an invalid validity interval.");
}

async function verify(
  signed: string, expected: string, key: openpgp.Key, now: Date,
): Promise<Uint8Array> {
  await key.verifyPrimaryKey(now, undefined, config);
  const message = await openpgp.readCleartextMessage({ cleartextMessage: signed, config });
  // Refuse multiple signatures before starting verification promises.
  if (message.getSigningKeyIDs().length !== 1) fail("Exactly one apt Release signature is required.");
  const result = await openpgp.verify({ message, verificationKeys: key, date: now, config });
  const verification = result.signatures[0];
  if (!verification || result.signatures.length !== 1) fail("Exactly one apt Release signature is required.");
  await verification.verified;
  const signature = await verification.signature;
  const packet = signature.packets[0];
  if (signature.packets.length !== 1 || packet?.version !== 4 ||
      packet.hashAlgorithm !== openpgp.enums.hash.sha512 || packet.signatureType !== openpgp.enums.signature.text) {
    fail("Apt Release requires one v4 SHA512 cleartext signature.");
  }
  profile(await key.getSigningKey(verification.keyID, now, undefined, config));
  // Only the library's verified data is authoritative. No trim: it would hide
  // unexpected signed content. LF/CRLF are the sole normalization permitted.
  const authenticated = result.data.replace(/\r\n/g, "\n");
  if (authenticated !== expected) fail("Authenticated apt Release differs from the intended content.");
  validateRelease(authenticated, now);
  return Buffer.from(authenticated, "utf8");
}

export async function verifyAptRelease(input: AptReleaseVerification): Promise<Uint8Array> {
  try {
    const signed = text(input.signedRelease);
    const expected = text(input.expectedRelease);
    const now = clock(input.now);
    const key = await readPinnedKey(input);
    return await verify(signed, expected, key, now);
  } catch (error) {
    throw safeError(error);
  }
}

export async function createAptPublicationSigner(input: AptSigningKey): Promise<AptPublicationSigner> {
  try {
    const { privateKey: armored, passphrase, now: getNow } = input;
    if (Buffer.byteLength(armored) > MAX_BYTES) fail("Apt private key is too large.");
    const publicKey = await readPinnedKey(input);
    const keys = await openpgp.readPrivateKeys({ armoredKeys: armored, config });
    let privateKey = keys[0];
    if (keys.length !== 1 || !privateKey) fail("Exactly one apt private key is required.");
    if (privateKey.getFingerprint() !== publicKey.getFingerprint()) fail("Apt private/public key fingerprint mismatch.");
    profile(privateKey);
    if (!privateKey.isDecrypted()) {
      if (passphrase === undefined) fail("The supplied apt private key requires a passphrase.");
      privateKey = await openpgp.decryptKey({ privateKey, passphrase, config });
    }
    const signingKey = privateKey;
    const now = clock(getNow());
    await publicKey.verifyPrimaryKey(now, undefined, config);
    profile(await signingKey.getSigningKey(undefined, now, undefined, config));
    return {
      async sign(release) {
        try {
          const expected = text(release);
          const now = clock(getNow());
          validateRelease(expected, now);
          await publicKey.verifyPrimaryKey(now, undefined, config);
          profile(await signingKey.getSigningKey(undefined, now, undefined, config));
          const signed = await openpgp.sign({
            message: await openpgp.createCleartextMessage({ text: expected }),
            signingKeys: signingKey, date: now, config,
          });
          await verify(signed, expected, publicKey, now);
          return Buffer.from(signed, "utf8");
        } catch (error) {
          throw safeError(error);
        }
      },
    };
  } catch (error) {
    throw safeError(error);
  }
}
