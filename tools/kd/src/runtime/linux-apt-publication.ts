/**
 * Archive transaction only: no command wiring, credentials, network or GPG.
 * Real storage must implement the atomicity/locking contract below; the signer
 * must return a clearsigned Release verified against its explicitly selected
 * key. In-memory tests prove ordering and byte integrity, not OpenPGP trust,
 * apt acceptance, release lineage or eligibility of a built package.
 */
import { createHash } from "node:crypto";
import {
  buildPackagesIndex,
  buildReleaseIndex,
  packagesByHashPath,
  packagesIndexPath,
  planPublish,
  poolPath,
  releaseIndexPath,
  verifyBeforeCommit,
  type AptArtifact,
  type AptChannel,
} from "./linux-apt";

export interface AptPublicationStorage {
  /** Serialize publishers of this archive, including other channels sharing
   *  its pool. Hold ownership until work settles, release on failure, and fail
   *  closed if ownership is lost. No read-then-write emulation of this lock. */
  withExclusivePublication<T>(work: () => Promise<T>): Promise<T>;
  /** Return complete stored bytes, not the upload's claimed digest. */
  read(path: string): Promise<Uint8Array | null>;
  /** Atomic create-if-absent; false leaves the existing object untouched. */
  create(path: string, bytes: Uint8Array): Promise<boolean>;
  /** Atomic replacement: readers see either the complete old or new object.
   *  An error may mean its acknowledgement was lost after replacement. */
  replace(path: string, bytes: Uint8Array): Promise<void>;
}

export interface AptPublicationSigner {
  sign(release: Uint8Array): Promise<Uint8Array>;
}

export interface AptPublicationArtifact {
  artifact: AptArtifact;
  bytes: Uint8Array;
}

export interface AptPublicationInput {
  channel: AptChannel;
  artifacts: AptPublicationArtifact[];
  /** Reuse the publication timestamp on retry; never silently refresh expiry. */
  date: Date;
  validForHours: number;
}

const ARCHITECTURES = ["amd64", "arm64"];

function digest(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}

function validateArtifacts(artifacts: AptPublicationArtifact[]): void {
  const paths = new Set<string>();
  const architectures = new Set<string>();
  const packages = new Set<string>();
  const uploaded: Record<string, { sha256: string; sizeBytes: number }> = {};
  for (const { artifact, bytes } of artifacts) {
    const fields = artifact.controlFields;
    if (!/^[a-z0-9][a-z0-9+.-]+$/.test(fields.Package ?? "") ||
        !/^[0-9][A-Za-z0-9.+:~_-]*$/.test(fields.Version ?? "") ||
        !/^[A-Za-z0-9][A-Za-z0-9.+:~_-]*\.deb$/.test(artifact.fileName)) {
      throw new Error("Invalid apt package name, version or archive filename.");
    }
    // Permit Debian continuation lines, but never a new field/stanza injected
    // through a value. Authoritative identity fields must use canonical names.
    for (const [key, value] of Object.entries(fields)) {
      if (!/^[A-Za-z0-9][A-Za-z0-9-]*$/.test(key) || /[\r\0]/.test(value) || /\n(?![ \t])/.test(value)) {
        throw new Error(`Invalid apt control field ${JSON.stringify(key)}.`);
      }
      if (["package", "version", "architecture"].includes(key.toLowerCase()) &&
          !["Package", "Version", "Architecture"].includes(key)) {
        throw new Error(`Noncanonical apt identity field ${key}.`);
      }
    }
    if (!ARCHITECTURES.includes(artifact.architecture) || fields.Architecture !== artifact.architecture ||
        architectures.has(artifact.architecture)) {
      throw new Error("Expected exactly one matching package per apt architecture: amd64 and arm64.");
    }
    const path = poolPath(artifact);
    if (paths.has(path)) throw new Error(`Duplicate apt pool path: ${path}.`);
    paths.add(path);
    architectures.add(artifact.architecture);
    packages.add(fields.Package);
    uploaded[path] = { sha256: digest(bytes), sizeBytes: bytes.byteLength };
  }
  const checked = verifyBeforeCommit({
    artifacts: artifacts.map(({ artifact }) => artifact),
    uploaded,
    requiredArchitectures: ARCHITECTURES,
  });
  if (!checked.ok) throw new Error(checked.problems.join("\n"));
  if (packages.size !== 1) throw new Error("The two apt architectures must name the same package.");
}

async function verifyStored(storage: AptPublicationStorage, path: string, expected: Uint8Array): Promise<void> {
  const stored = await storage.read(path);
  if (stored === null || stored.byteLength !== expected.byteLength || digest(stored) !== digest(expected)) {
    throw new Error(`Stored bytes differ or are missing at ${path}; refusing apt publication.`);
  }
}

/**
 * Writes every dependency before signing and replacing InRelease last. Errors
 * propagate; there is no implicit retry. Repeating the same input is safe even
 * after a lost commit acknowledgement. Different bytes at an existing pool or
 * by-hash path are refused. Nothing deletes historical immutable objects.
 *
 * Availability across a publish is guaranteed for by-hash readers. A reader
 * explicitly disabling by-hash can race a canonical Packages replacement and
 * fail checksum validation; it must never be described as an atomic snapshot.
 */
export async function publishAptArchive(
  input: AptPublicationInput,
  storage: AptPublicationStorage,
  signer: AptPublicationSigner,
): Promise<void> {
  // Own a snapshot before the first await, so callers cannot change indexed
  // metadata or bytes while storage/signing is in flight.
  const artifacts = input.artifacts.map(({ artifact, bytes }) => ({
    artifact: { ...artifact, controlFields: { ...artifact.controlFields } },
    bytes: Buffer.from(bytes),
  })).sort((left, right) => left.artifact.architecture.localeCompare(right.artifact.architecture));
  const channel = input.channel;
  const date = new Date(input.date.getTime());
  const validForHours = input.validForHours;
  if (!["desktop-linux-staging", "desktop-linux"].includes(channel) ||
      !Number.isFinite(date.getTime()) || !Number.isFinite(validForHours) || validForHours <= 0 ||
      !Number.isFinite(new Date(date.getTime() + validForHours * 3_600_000).getTime())) {
    throw new Error("Invalid apt channel, publication date or validity interval.");
  }
  validateArtifacts(artifacts);
  const metadata = artifacts.map(({ artifact }) => artifact);
  const payloads = new Map<string, Uint8Array>(artifacts.map(({ artifact, bytes }) => [poolPath(artifact), bytes]));
  const indexes: Record<string, string> = {};
  for (const architecture of ARCHITECTURES) {
    const contents = buildPackagesIndex(metadata.filter((artifact) => artifact.architecture === architecture));
    const path = packagesIndexPath(channel, architecture);
    indexes[path] = contents;
    payloads.set(path, Buffer.from(contents));
    payloads.set(packagesByHashPath(channel, architecture, contents), Buffer.from(contents));
  }
  const release = Buffer.from(buildReleaseIndex({ channel, architectures: ARCHITECTURES, date, validForHours, indexes }));
  payloads.set(releaseIndexPath(channel), release);
  const steps = planPublish({ channel, artifacts: metadata, architectures: ARCHITECTURES });

  await storage.withExclusivePublication(async () => {
    for (const step of steps) {
      if (step.kind === "commit") {
        // Read back the whole closure after all uploads, including immutable
        // indexes and canonical aliases, before the signer sees the Release.
        for (const [path, bytes] of payloads) await verifyStored(storage, path, bytes);
        const signed = await signer.sign(Buffer.from(release));
        if (signed.byteLength === 0) throw new Error("The apt signer returned an empty InRelease.");
        await storage.replace(step.path, signed);
      } else {
        const bytes = payloads.get(step.path);
        if (!bytes) throw new Error(`Missing apt publication payload: ${step.path}.`);
        if (step.write === "immutable") {
          if (!await storage.create(step.path, bytes)) await verifyStored(storage, step.path, bytes);
        } else {
          await storage.replace(step.path, bytes);
        }
      }
    }
  });
}
