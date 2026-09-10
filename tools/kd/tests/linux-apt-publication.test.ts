import { createHash } from "node:crypto";
import { describe, expect, it } from "vitest";
import { inReleasePath, packagesIndexPath, poolPath } from "../src/runtime/linux-apt";
import {
  publishAptArchive,
  type AptPublicationInput,
  type AptPublicationSigner,
  type AptPublicationStorage,
} from "../src/runtime/linux-apt-publication";

const channel = "desktop-linux-staging";
const commitPath = inReleasePath(channel);
const sha256 = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");

function publication(version = "1.2.3-1"): AptPublicationInput {
  return {
    channel,
    date: new Date("2026-09-09T00:00:00Z"),
    validForHours: 168,
    artifacts: ["amd64", "arm64"].map((architecture) => {
      // Synthetic package bytes. These tests do not build or install a deb.
      const bytes = Buffer.from(`fixture ${architecture} ${version}`);
      return {
        bytes,
        artifact: {
          architecture,
          fileName: `kanna_${version}_${architecture}.deb`,
          sizeBytes: bytes.byteLength,
          sha256: sha256(bytes),
          controlFields: { Package: "kanna", Version: version, Architecture: architecture },
        },
      };
    }),
  };
}

class MemoryStorage implements AptPublicationStorage {
  readonly objects = new Map<string, Buffer>();
  readonly operations: string[] = [];
  writes = 0;
  failAt = 0;
  failAfterWrite = false;
  corruptRead?: (path: string, bytes: Buffer | null) => Buffer | null;
  private tail = Promise.resolve();

  async withExclusivePublication<T>(work: () => Promise<T>): Promise<T> {
    const previous = this.tail;
    let unlock = () => {};
    this.tail = new Promise<void>((resolve) => { unlock = resolve; });
    await previous;
    try {
      return await work();
    } finally {
      unlock();
    }
  }

  async read(path: string): Promise<Uint8Array | null> {
    this.operations.push(`read ${path}`);
    const bytes = this.objects.get(path);
    const copy = bytes === undefined ? null : Buffer.from(bytes);
    return this.corruptRead ? this.corruptRead(path, copy) : copy;
  }

  private write(path: string, bytes: Uint8Array): void {
    this.writes += 1;
    if (this.writes === this.failAt && !this.failAfterWrite) throw new Error("interrupted upload");
    this.objects.set(path, Buffer.from(bytes));
    if (this.writes === this.failAt && this.failAfterWrite) throw new Error("lost acknowledgement");
  }

  async create(path: string, bytes: Uint8Array): Promise<boolean> {
    this.operations.push(`create ${path}`);
    if (this.objects.has(path)) return false;
    this.write(path, bytes);
    return true;
  }

  async replace(path: string, bytes: Uint8Array): Promise<void> {
    this.operations.push(`replace ${path}`);
    this.write(path, bytes);
  }
}

class TestSigner implements AptPublicationSigner {
  readonly releases: Buffer[] = [];
  failure = false;
  empty = false;

  constructor(private storage: MemoryStorage) {}

  async sign(release: Uint8Array): Promise<Uint8Array> {
    this.storage.operations.push("sign");
    this.releases.push(Buffer.from(release));
    if (this.failure) throw new Error("test signer failed");
    // Deliberately NOT an OpenPGP signature; trust/expiry rejection needs apt
    // and a real test key in the later Linux acceptance checkpoint.
    return this.empty ? Buffer.alloc(0) : Buffer.concat([Buffer.from("TEST-ONLY\n"), release]);
  }
}

/** Follow the advertised checksum chain independently of the plan/path helper.
 *  Can start at a retained InRelease while canonical aliases name a newer one. */
function expectReadable(store: MemoryStorage, signed: Buffer | undefined, version: string): void {
  expect(signed).toBeDefined();
  const release = signed?.toString() ?? "";
  expect(release).toContain("TEST-ONLY\n");
  expect(release).toContain("Acquire-By-Hash: yes");
  expect(release).toContain("Architectures: amd64 arm64");
  const checksums = release.split("SHA256:\n")[1]?.trim().split("\n") ?? [];
  expect(checksums).toHaveLength(2);
  for (const row of checksums) {
    const [digest, size, relative] = row.trim().split(/\s+/);
    const path = `dists/staging/${relative?.replace(/Packages$/, `by-hash/SHA256/${digest}`)}`;
    const index = store.objects.get(path);
    expect(index, path).toBeDefined();
    if (!index) throw new Error(`Missing ${path}`);
    expect(index.byteLength).toBe(Number(size));
    expect(sha256(index)).toBe(digest);
    expect(index.toString()).toContain(`Version: ${version}\n`);
    expect(index.toString()).toContain(`Architecture: ${relative?.includes("amd64") ? "amd64" : "arm64"}\n`);
    const fields = Object.fromEntries(index.toString().trim().split("\n").map((line) => {
      const split = line.indexOf(": ");
      return [line.slice(0, split), line.slice(split + 2)];
    }));
    const bytes = store.objects.get(fields.Filename);
    expect(bytes, fields.Filename).toBeDefined();
    if (!bytes) throw new Error(`Missing ${fields.Filename}`);
    expect(bytes.byteLength).toBe(Number(fields.Size));
    expect(sha256(bytes)).toBe(fields.SHA256);
  }
}

describe("apt publication with in-memory storage and test signing", () => {
  it("reads back both architectures and every index before signing, then commits last", async () => {
    const store = new MemoryStorage();
    const signer = new TestSigner(store);
    await publishAptArchive(publication(), store, signer);
    expectReadable(store, store.objects.get(commitPath), "1.2.3-1");
    expect(store.operations.slice(-2)).toEqual(["sign", `replace ${commitPath}`]);
    const signIndex = store.operations.indexOf("sign");
    for (const path of store.objects.keys()) {
      if (path === commitPath) continue;
      expect(store.operations.indexOf(`read ${path}`)).toBeGreaterThan(-1);
      expect(store.operations.indexOf(`read ${path}`)).toBeLessThan(signIndex);
      if (path.startsWith("pool/") || path.includes("/by-hash/")) {
        expect(store.operations).toContain(`create ${path}`);
        expect(store.operations).not.toContain(`replace ${path}`);
      }
    }
  });

  it("retains the previous checksum chain after publishing a new version", async () => {
    const store = new MemoryStorage();
    const signer = new TestSigner(store);
    await publishAptArchive(publication(), store, signer);
    const previous = store.objects.get(commitPath);
    await publishAptArchive(publication("1.2.4-1"), store, signer);
    expectReadable(store, previous, "1.2.3-1");
    expectReadable(store, store.objects.get(commitPath), "1.2.4-1");
    expect(store.objects.get(packagesIndexPath(channel, "amd64"))?.toString()).toContain("Version: 1.2.4-1");
  });

  for (const failAfterWrite of [false, true]) {
    it.each([1, 2, 3, 4, 5, 6, 7, 8])(`recovers from ${failAfterWrite ? "lost acknowledgement after" : "interruption before"} write %i`, async (failAt) => {
      const store = new MemoryStorage();
      const signer = new TestSigner(store);
      await publishAptArchive(publication(), store, signer);
      const previous = store.objects.get(commitPath);
      store.writes = 0;
      store.failAt = failAt;
      store.failAfterWrite = failAfterWrite;
      const next = publication("1.2.4-1");
      await expect(publishAptArchive(next, store, signer)).rejects.toThrow(/interrupted|acknowledgement/);
      expectReadable(store, previous, "1.2.3-1");
      const committed = failAfterWrite && failAt === 8;
      expectReadable(store, store.objects.get(commitPath), committed ? "1.2.4-1" : "1.2.3-1");
      if (!committed) expect(store.objects.get(commitPath)).toEqual(previous);
      store.failAt = 0;
      await publishAptArchive(next, store, signer);
      expectReadable(store, store.objects.get(commitPath), "1.2.4-1");
      // Also repeat a fully acknowledged publication.
      const objects = new Map(store.objects);
      await publishAptArchive(next, store, signer);
      expect(store.objects).toEqual(objects);
    });
  }

  it.each(["pool", "by-hash"])("refuses conflicting existing %s bytes without replacing them", async (kind) => {
    const store = new MemoryStorage();
    const signer = new TestSigner(store);
    await publishAptArchive(publication(), store, signer);
    const conflict = [...store.objects.keys()].find((path) => kind === "pool" ? path.startsWith("pool/") : path.includes("/by-hash/"));
    if (!conflict) throw new Error("Missing conflict fixture");
    store.objects.set(conflict, Buffer.from("conflicting bytes"));
    const before = new Map(store.objects);
    await expect(publishAptArchive(publication(), store, signer)).rejects.toThrow(/Stored bytes differ/);
    expect(store.objects).toEqual(before);
    expect(signer.releases).toHaveLength(1);
  });

  it("refuses a changed rebuild at an already published filename, preserving the live archive", async () => {
    const store = new MemoryStorage();
    const signer = new TestSigner(store);
    await publishAptArchive(publication(), store, signer);
    const changed = publication();
    const item = changed.artifacts[0];
    if (!item) throw new Error("Missing fixture package");
    item.bytes = Buffer.from("different build, same version and filename");
    item.artifact.sha256 = sha256(item.bytes);
    item.artifact.sizeBytes = item.bytes.byteLength;
    const originalBytes = store.objects.get(poolPath(item.artifact));
    await expect(publishAptArchive(changed, store, signer)).rejects.toThrow(/Stored bytes differ/);
    expect(store.objects.get(poolPath(item.artifact))).toEqual(originalBytes);
    expectReadable(store, store.objects.get(commitPath), "1.2.3-1");
    expect(signer.releases).toHaveLength(1);
  });

  it("snapshots caller metadata, bytes and dates before waiting on storage", async () => {
    const store = new MemoryStorage();
    const signer = new TestSigner(store);
    const input = publication();
    const pending = publishAptArchive(input, store, signer);
    input.date.setFullYear(2030);
    input.validForHours = 1;
    for (const item of input.artifacts) {
      item.bytes.fill(0);
      item.artifact.controlFields.Version = "9.9.9-1";
    }
    await pending;
    expectReadable(store, store.objects.get(commitPath), "1.2.3-1");
    expect(signer.releases[0]?.toString()).toContain("Valid-Until: Wed, 16 Sep 2026 00:00:00 GMT");
  });

  it.each(["pool", "by-hash", "Packages", "Release"])("refuses corrupted %s read-back before signing or committing", async (kind) => {
    const store = new MemoryStorage();
    const signer = new TestSigner(store);
    store.corruptRead = (path, bytes) => {
      const matches = kind === "pool" ? path.startsWith("pool/") : kind === "by-hash" ? path.includes("/by-hash/") : path.endsWith(`/${kind}`);
      if (!matches || !bytes) return bytes;
      const corrupted = Buffer.from(bytes);
      corrupted[0] ^= 1; // Same size, different digest.
      return corrupted;
    };
    await expect(publishAptArchive(publication(), store, signer)).rejects.toThrow(/Stored bytes differ/);
    expect(signer.releases).toHaveLength(0);
    expect(store.objects.has(commitPath)).toBe(false);
  });

  it.each(["missing", "truncated"])("refuses %s stored bytes", async (kind) => {
    const store = new MemoryStorage();
    const signer = new TestSigner(store);
    store.corruptRead = (_path, bytes) => kind === "missing" ? null : bytes?.subarray(1) ?? null;
    await expect(publishAptArchive(publication(), store, signer)).rejects.toThrow(/Stored bytes differ/);
    expect(signer.releases).toHaveLength(0);
  });

  it.each(["throw", "empty"])("preserves the old commit when signing returns %s, and allows retry", async (mode) => {
    const store = new MemoryStorage();
    const signer = new TestSigner(store);
    await publishAptArchive(publication(), store, signer);
    const previous = store.objects.get(commitPath);
    signer.failure = mode === "throw";
    signer.empty = mode === "empty";
    await expect(publishAptArchive(publication("1.2.4-1"), store, signer)).rejects.toThrow(/signer/);
    expect(store.objects.get(commitPath)).toEqual(previous);
    expectReadable(store, previous, "1.2.3-1");
    signer.failure = false;
    signer.empty = false;
    await publishAptArchive(publication("1.2.4-1"), store, signer);
    expectReadable(store, store.objects.get(commitPath), "1.2.4-1");
  });

  it("serializes overlapping publications and keeps both versions readable", async () => {
    const store = new MemoryStorage();
    const signer = new TestSigner(store);
    await Promise.all([
      publishAptArchive(publication(), store, signer),
      publishAptArchive(publication("1.2.4-1"), store, signer),
    ]);
    expectReadable(store, store.objects.get(commitPath), "1.2.4-1");
    expect(signer.releases).toHaveLength(2);
    const first = signer.releases[0];
    if (!first) throw new Error("Missing first signed release");
    expectReadable(store, Buffer.concat([Buffer.from("TEST-ONLY\n"), first]), "1.2.3-1");
  });

  it.each(["missing arm64", "missing amd64", "wrong architecture", "duplicate architecture", "different versions", "different packages", "wrong digest", "wrong size", "path traversal", "field injection", "invalid date", "invalid validity"])("rejects %s before touching storage", async (fault) => {
    const input = publication();
    const first = input.artifacts[0];
    const second = input.artifacts[1];
    if (!first || !second) throw new Error("Missing fixture architectures");
    switch (fault) {
      case "missing arm64": input.artifacts.pop(); break;
      case "missing amd64": input.artifacts.shift(); break;
      case "wrong architecture": first.artifact.controlFields.Architecture = "arm64"; break;
      case "duplicate architecture": input.artifacts.push(first); break;
      case "different versions": second.artifact.controlFields.Version = "1.2.4-1"; break;
      case "different packages": second.artifact.controlFields.Package = "kanna-staging"; break;
      case "wrong digest": first.artifact.sha256 = "0".repeat(64); break;
      case "wrong size": first.artifact.sizeBytes += 1; break;
      case "path traversal": first.artifact.fileName = "../overwrite.deb"; break;
      case "field injection": first.artifact.controlFields.Description = "text\nSHA256: forged"; break;
      case "invalid date": input.date = new Date("invalid"); break;
      case "invalid validity": input.validForHours = 0; break;
    }
    const store = new MemoryStorage();
    const signer = new TestSigner(store);
    await expect(publishAptArchive(input, store, signer)).rejects.toThrow();
    expect(store.operations).toEqual([]);
    expect(signer.releases).toHaveLength(0);
  });
});
