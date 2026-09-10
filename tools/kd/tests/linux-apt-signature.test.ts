import { createHash } from "node:crypto";
import { beforeAll, describe, expect, it } from "vitest";
import * as pgp from "openpgp";
import { buildReleaseIndex, inReleasePath } from "../src/runtime/linux-apt";
import { publishAptArchive, type AptPublicationStorage } from "../src/runtime/linux-apt-publication";
import { createAptPublicationSigner, verifyAptRelease } from "../src/runtime/linux-apt-signature";

const created = new Date("2026-09-09T00:00:00Z");
const now = new Date("2026-09-10T01:00:00Z");
const passphrase = "disposable test ONLY";
let privateKey: pgp.PrivateKey;
let encrypted: string;
let publicKey: string;
let fingerprint: string;
let wrong: pgp.PrivateKey;
const encode = (value: string) => Buffer.from(value);

function release(date = new Date("2026-09-10T00:00:00Z"), validForHours = 24): string {
  return buildReleaseIndex({ channel: "desktop-linux-staging", architectures: ["amd64", "arm64"], date, validForHours, indexes: {} });
}

function keys() {
  return { privateKey: encrypted, publicKey, fingerprint, passphrase, now: () => now };
}

async function rawSign(value: string, overrides: Partial<Pick<pgp.SignOptions, "signingKeys" | "config">> = {}): Promise<string> {
  return pgp.sign({
    message: await pgp.createCleartextMessage({ text: value }), signingKeys: privateKey, date: now,
    config: { preferredHashAlgorithm: pgp.enums.hash.sha512 }, ...overrides,
  });
}

function verify(signedRelease: string, expectedRelease = release(), overrides = {}) {
  return verifyAptRelease({ signedRelease: encode(signedRelease), expectedRelease: encode(expectedRelease), publicKey, fingerprint, now, ...overrides });
}

beforeAll(async () => {
  const generated = await pgp.generateKey({
    type: "rsa", rsaBits: 3072, subkeys: [], passphrase, format: "object",
    userIDs: [{ name: "Kanna apt test ONLY", email: "test@example.invalid" }],
    date: created, keyExpirationTime: 7 * 86400, config: { v6Keys: false },
  });
  encrypted = generated.privateKey.armor();
  privateKey = await pgp.decryptKey({ privateKey: generated.privateKey, passphrase });
  publicKey = generated.publicKey.armor();
  fingerprint = generated.publicKey.getFingerprint();
  wrong = (await pgp.generateKey({
    type: "rsa", rsaBits: 3072, subkeys: [], format: "object",
    userIDs: [{ name: "Other test ONLY", email: "wrong@example.invalid" }], date: created,
  })).privateKey;
});

describe("supplied-key OpenPGP apt signatures", () => {
  it("signs with the explicitly pinned key and returns library-verified plaintext", async () => {
    const signer = await createAptPublicationSigner(keys());
    const signed = await signer.sign(encode(release()));
    expect(Buffer.from(signed).toString()).toContain("Hash: SHA512");
    expect(await verifyAptRelease({ signedRelease: signed, expectedRelease: encode(release()), publicKey, fingerprint, now })).toEqual(encode(release()));
  });

  it("permits only LF/CRLF normalization", async () => {
    const signer = await createAptPublicationSigner(keys());
    const signed = await signer.sign(encode(release().replace(/\n/g, "\r\n")));
    expect(await verify(Buffer.from(signed).toString(), release().replace(/\n/g, "\r\n"))).toEqual(encode(release()));
    await expect(signer.sign(encode(release().replace("Suite: staging", "Suite: staging ")))).rejects.toThrow(/canonical/);
  });

  it("rejects a bad signature", async () => {
    const signed = await rawSign(release());
    const tampered = signed.replace("Suite: staging", "Suite: stable");
    await expect(verify(tampered, release().replace("Suite: staging", "Suite: stable"))).rejects.toThrow(/OpenPGP/);
  });

  it("rejects wrong-key signatures even when the supplied verification key matches its own pin", async () => {
    const signed = await rawSign(release(), { signingKeys: wrong });
    await expect(verify(signed)).rejects.toThrow(/OpenPGP/);
  });

  it.each(["", "DEADBEEF", "0".repeat(40)])("refuses a missing, short or mismatched fingerprint (%s)", async (pin) => {
    await expect(createAptPublicationSigner({ ...keys(), fingerprint: pin })).rejects.toThrow(/fingerprint/);
    await expect(verify(await rawSign(release()), release(), { fingerprint: pin })).rejects.toThrow(/fingerprint/);
  });

  it("accepts uppercase full fingerprints without weakening identity", async () => {
    const signer = await createAptPublicationSigner({ ...keys(), fingerprint: fingerprint.toUpperCase() });
    expect(await signer.sign(encode(release()))).not.toHaveLength(0);
  });

  it("refuses a different private key", async () => {
    await expect(createAptPublicationSigner({ ...keys(), privateKey: wrong.armor() })).rejects.toThrow(/fingerprint mismatch/);
  });

  it("does not expose a wrong passphrase or supplied private key in errors", async () => {
    await expect(createAptPublicationSigner({ ...keys(), passphrase: "DO NOT EXPOSE THIS" })).rejects.toThrow("OpenPGP apt signature operation failed.");
    await expect(createAptPublicationSigner({ ...keys(), passphrase: undefined })).rejects.toThrow(/requires a passphrase/);
    await expect(createAptPublicationSigner({ ...keys(), privateKey: "DO NOT EXPOSE PRIVATE INPUT" })).rejects.toThrow("OpenPGP apt signature operation failed.");
  });

  it("accepts supplied unencrypted test keys without consulting a keyring", async () => {
    const signer = await createAptPublicationSigner({ ...keys(), privateKey: privateKey.armor(), passphrase: undefined });
    expect(await signer.sign(encode(release()))).not.toHaveLength(0);
  });

  it("rejects signature-free, extra-signature and malformed armor", async () => {
    await expect(verify(release())).rejects.toThrow();
    const multiple = await rawSign(release(), { signingKeys: [privateKey, wrong] });
    await expect(verify(multiple)).rejects.toThrow(/Exactly one/);
    const injected = (await rawSign(release())).replace("Hash: SHA512", "Suite: stable\nHash: SHA512");
    await expect(verify(injected)).rejects.toThrow();
  });

  it("rejects valid signatures over unexpected plaintext", async () => {
    await expect(verify(await rawSign(release()), release().replace("Suite: staging", "Suite: stable"))).rejects.toThrow(/intended content/);
  });

  it("rejects SHA256 signatures without disabling the library's signature checks", async () => {
    const signed = await rawSign(release(), { config: { preferredHashAlgorithm: pgp.enums.hash.sha256 } });
    expect(signed).toContain("Hash: SHA256");
    await expect(verify(signed)).rejects.toThrow(/SHA512/);
  });

  it("rejects revoked and expired pinned keys", async () => {
    const signed = await rawSign(release());
    const revoked = await privateKey.revoke({ flag: pgp.enums.reasonForRevocation.keyCompromised }, now);
    await expect(verify(signed, release(), { publicKey: revoked.toPublic().armor() })).rejects.toThrow(/OpenPGP/);
    const expired = new Date("2026-09-17T00:00:00Z");
    await expect(verify(signed, release(), { now: expired })).rejects.toThrow(/OpenPGP/);
    await expect(createAptPublicationSigner({ ...keys(), now: () => expired })).rejects.toThrow(/OpenPGP/);
  });

  it("rechecks the clock on each use of an existing signer", async () => {
    let time = now;
    const signer = await createAptPublicationSigner({ ...keys(), now: () => time });
    await signer.sign(encode(release()));
    time = new Date("2026-09-11T00:00:00Z");
    await expect(signer.sign(encode(release()))).rejects.toThrow(/expired/);
  });

  it("supports a bound RSA signing subkey under the pinned primary fingerprint", async () => {
    const subkey = await privateKey.addSubkey({ type: "rsa", rsaBits: 3072, sign: true, date: created });
    const signer = await createAptPublicationSigner({ ...keys(), privateKey: subkey.armor(), publicKey: subkey.toPublic().armor(), passphrase: undefined });
    expect(await signer.sign(encode(release()))).not.toHaveLength(0);
  });

  it("rejects unsupported key profiles", async () => {
    const ecc = (await pgp.generateKey({ type: "ecc", curve: "ed25519Legacy", format: "object", date: created, userIDs: [{ name: "Unsupported test" }] })).privateKey;
    await expect(createAptPublicationSigner({ ...keys(), privateKey: ecc.armor(), publicKey: ecc.toPublic().armor(), fingerprint: ecc.getFingerprint() })).rejects.toThrow(/v4 RSA/);
  });
});

describe("authenticated apt Release validity", () => {
  const cases = [
    ["expired", () => release(created, 1)],
    ["expiry equality", () => release(new Date("2026-09-10T00:00:00Z"), 1)],
    ["future Date", () => release(new Date("2026-09-11T00:00:00Z"), 24)],
    ["missing expiry", () => release().replace(/^Valid-Until:.*\n/m, "")],
    ["missing Date", () => release().replace(/^Date:.*\n/m, "")],
    ["invalid date", () => release().replace(/^Date:.*$/m, "Date: nonsense")],
    ["impossible date", () => release().replace(/^Date:.*$/m, "Date: Mon, 31 Feb 2026 00:00:00 GMT")],
    ["duplicate expiry", () => `${release()}valid-until: Fri, 11 Sep 2026 00:00:00 GMT\n`],
    ["duplicate Date", () => `${release()}Date: Thu, 10 Sep 2026 00:00:00 GMT\n`],
    ["date continuation", () => release().replace("Date: Thu,", "Date:\n Thu,")],
    ["extra stanza", () => `${release()}\nSuite: stable\n`],
    ["reversed interval", () => release(new Date("2026-09-10T00:00:00Z"), -1)],
  ] as const;
  it.each(cases)("rejects %s even when cryptographically signed", async (_label, invalid) => {
    const value = invalid();
    // The library accepts the actual signature. Our metadata policy must still reject it.
    const signed = await rawSign(value);
    const result = await pgp.verify({ message: await pgp.readCleartextMessage({ cleartextMessage: signed }), verificationKeys: await pgp.readKey({ armoredKey: publicKey }), date: now });
    await result.signatures[0].verified;
    await expect(verify(signed, value)).rejects.toThrow();
    const signer = await createAptPublicationSigner(keys());
    await expect(signer.sign(encode(value))).rejects.toThrow();
  });

  it("rejects invalid clocks, malformed UTF-8 and oversized input", async () => {
    const signed = await rawSign(release());
    await expect(verify(signed, release(), { now: new Date("invalid") })).rejects.toThrow(/clock/);
    await expect(verify(signed, release(), { signedRelease: Uint8Array.from([0xff]) })).rejects.toThrow(/OpenPGP/);
    await expect(verify(signed, release(), { signedRelease: new Uint8Array(1024 * 1024 + 1) })).rejects.toThrow(/size/);
  });
});

it("uses the real signer at the final transaction boundary and preserves the old commit on expiry", async () => {
  const objects = new Map<string, Uint8Array>();
  const operations: string[] = [];
  const storage: AptPublicationStorage = {
    withExclusivePublication: (work) => work(),
    read: async (path) => { operations.push(`read ${path}`); return objects.get(path) ?? null; },
    create: async (path, bytes) => { if (objects.has(path)) return false; objects.set(path, Uint8Array.from(bytes)); return true; },
    replace: async (path, bytes) => { operations.push(`replace ${path}`); objects.set(path, Uint8Array.from(bytes)); },
  };
  const input = {
    channel: "desktop-linux-staging" as const, date: new Date("2026-09-10T00:00:00Z"), validForHours: 24,
    artifacts: ["amd64", "arm64"].map((architecture) => {
      const bytes = encode(`synthetic package ${architecture}`);
      return { bytes, artifact: { architecture, fileName: `kanna_1.2.3_${architecture}.deb`, sizeBytes: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex"), controlFields: { Package: "kanna", Version: "1.2.3", Architecture: architecture } } };
    }),
  };
  let time = now;
  const signer = await createAptPublicationSigner({ ...keys(), now: () => time });
  await publishAptArchive(input, storage, signer);
  const path = inReleasePath(input.channel);
  const signed = objects.get(path);
  const expected = objects.get("dists/staging/Release");
  if (!signed || !expected) throw new Error("Missing signed archive fixture");
  await verifyAptRelease({ signedRelease: signed, expectedRelease: expected, publicKey, fingerprint, now });
  expect(operations.at(-1)).toBe(`replace ${path}`);
  const commits = operations.filter((op) => op === `replace ${path}`).length;
  time = new Date("2026-09-11T00:00:00Z");
  await expect(publishAptArchive(input, storage, signer)).rejects.toThrow(/expired/);
  expect(objects.get(path)).toEqual(signed);
  expect(operations.filter((op) => op === `replace ${path}`)).toHaveLength(commits);
});
