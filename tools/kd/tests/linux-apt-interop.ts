/** Test-only Ubuntu apt/GnuPG interoperability. No installs, sudo or remote
 * publication. Run with tsx after building kd into .tmp/apt-adapter-bundle.
 * --prepare-only exercises fixture construction on the Studio without apt. */
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import * as pgp from "openpgp";
import { publishAptArchive, type AptPublicationStorage } from "../src/runtime/linux-apt-publication";
import type * as SignatureAdapter from "../src/runtime/linux-apt-signature";

const prepareOnly = process.argv.includes("--prepare-only");
assert(prepareOnly || process.platform === "linux", "Interop requires Ubuntu; use --prepare-only for fixture checks elsewhere.");
const repoRoot = resolve(import.meta.dirname, "../../..");
const scratch = join(repoRoot, ".tmp");
await mkdir(scratch, { recursive: true });
const root = await mkdtemp(join(scratch, "apt-interop-"));
const adapter: typeof SignatureAdapter = await import(pathToFileURL(join(scratch, "apt-adapter-bundle/runtime/linux-apt-signature.js")).href);
const now = new Date(Math.floor(Date.now() / 1000) * 1000);
const artifacts = ["amd64", "arm64"].map((architecture) => {
  // Metadata acceptance only: these are synthetic payloads, not installable
  // Kanna packages or release-graph artifact evidence.
  const bytes = Buffer.from(`TEST ONLY ${architecture}\n`);
  return { bytes, artifact: {
    architecture, fileName: `kanna-apt-interop_1.0_${architecture}.deb`, sizeBytes: bytes.length,
    sha256: createHash("sha256").update(bytes).digest("hex"),
    controlFields: { Package: "kanna-apt-interop", Version: "1.0", Architecture: architecture, Description: "Test metadata only" },
  } };
});

async function command(name: string, args: string[], env = process.env) {
  return new Promise<{ code: number | null; output: string }>((accept, reject) => {
    const child = spawn(name, args, { cwd: root, env: { ...env, LC_ALL: "C" }, stdio: ["ignore", "pipe", "pipe"] });
    let output = "";
    child.stdout.on("data", (data) => { output += String(data); });
    child.stderr.on("data", (data) => { output += String(data); });
    child.once("error", reject);
    child.once("close", (code) => accept({ code, output }));
  });
}
async function key() {
  return pgp.generateKey({ type: "rsa", rsaBits: 3072, subkeys: [], format: "object",
    date: new Date(now.getTime() - 10 * 86400_000), config: { v6Keys: false },
    userIDs: [{ name: "Disposable apt interoperability test ONLY", email: "test@example.invalid" }],
  });
}
const server = createServer();
try {
  const signing = await key();
  const other = await key();
  const keys = { privateKey: signing.privateKey.armor(), publicKey: signing.publicKey.armor(), fingerprint: signing.publicKey.getFingerprint() };
  const archives = new Map<string, Map<string, Uint8Array>>();
  for (const [name, age] of [["valid", 0], ["expired", 3 * 86400_000]] as const) {
    const date = new Date(now.getTime() - age);
    const objects = new Map<string, Uint8Array>();
    const storage: AptPublicationStorage = {
      withExclusivePublication: (work) => work(),
      read: async (path) => objects.get(path) ?? null,
      create: async (path, bytes) => { if (objects.has(path)) return false; objects.set(path, Uint8Array.from(bytes)); return true; },
      replace: async (path, bytes) => { objects.set(path, Uint8Array.from(bytes)); },
    };
    const signer = await adapter.createAptPublicationSigner({ ...keys, now: () => date });
    await publishAptArchive({ channel: "desktop-linux-staging", date, validForHours: 24, artifacts }, storage, signer);
    const signedRelease = objects.get("dists/staging/InRelease")!;
    const expectedRelease = objects.get("dists/staging/Release")!;
    const verified = adapter.verifyAptRelease({ ...keys, signedRelease, expectedRelease, now });
    if (name === "expired") await assert.rejects(verified, /expired/);
    else assert.deepEqual(await verified, Buffer.from(expectedRelease));
    archives.set(name, objects);
    await writeFile(join(root, `${name}.InRelease`), signedRelease);
  }
  const valid = archives.get("valid")!;
  const tampered = new Map(valid);
  tampered.set("dists/staging/InRelease", Buffer.from(Buffer.from(valid.get("dists/staging/InRelease")!).toString().replace("Suite: staging", "Suite: stable")));
  archives.set("tampered", tampered);
  archives.set("wrong-key", valid);
  await assert.rejects(adapter.verifyAptRelease({ ...keys, now, signedRelease: tampered.get("dists/staging/InRelease")!, expectedRelease: valid.get("dists/staging/Release")! }));
  await assert.rejects(adapter.verifyAptRelease({ now, publicKey: other.publicKey.armor(), fingerprint: other.publicKey.getFingerprint(), signedRelease: valid.get("dists/staging/InRelease")!, expectedRelease: valid.get("dists/staging/Release")! }));
  await writeFile(join(root, "tampered.InRelease"), tampered.get("dists/staging/InRelease")!);
  await writeFile(join(root, "trusted.gpg"), signing.publicKey.write());
  await writeFile(join(root, "wrong.gpg"), other.publicKey.write());
  await mkdir(join(root, "gnupg"), { mode: 0o700 });
  console.log(`Fixture generated on ${process.platform}/${process.arch} ${process.version}; both architecture indexes signed by emitted adapter.`);
  if (!prepareOnly) {
    const distribution = await readFile("/etc/os-release", "utf8");
    assert.match(distribution, /^ID=ubuntu$/m);
    assert.match(distribution, /^VERSION_ID="24\.04"$/m);
    console.log(distribution);
    for (const tool of ["apt-get", "gpgv"]) {
      const version = await command(tool, ["--version"]);
      assert.equal(version.code, 0, version.output);
      console.log(version.output.split("\n")[0]);
    }
    for (const name of ["valid", "expired", "tampered", "wrong-key"]) {
      const result = await command("gpgv", ["--homedir", join(root, "gnupg"), "--status-fd", "1", "--keyring",
        join(root, name === "wrong-key" ? "wrong.gpg" : "trusted.gpg"), join(root, `${name === "wrong-key" ? "valid" : name}.InRelease`)]);
      if (name === "valid" || name === "expired") {
        assert.equal(result.code, 0, result.output);
        assert(result.output.includes(`VALIDSIG ${keys.fingerprint.toUpperCase()} `), result.output);
      } else {
        assert.notEqual(result.code, 0, result.output);
        assert.match(result.output, name === "tampered" ? /BADSIG/ : /NO_PUBKEY/);
      }
      console.log(`GnuPG ${name}: expected result, exit ${result.code}`);
    }
    const requests: string[] = [];
    server.on("request", (request, response) => {
      const path = new URL(request.url ?? "/", "http://127.0.0.1").pathname.slice(1);
      requests.push(path);
      const separator = path.indexOf("/");
      const bytes = archives.get(path.slice(0, separator))?.get(path.slice(separator + 1));
      response.writeHead(bytes ? 200 : 404, { "Content-Type": "application/octet-stream" });
      response.end(bytes ? Buffer.from(bytes) : "not found");
    });
    await new Promise<void>((accept) => server.listen(0, "127.0.0.1", accept));
    const address = server.address();
    assert(address && typeof address !== "string");
    for (const name of ["valid", "tampered", "wrong-key", "expired"]) {
      // Independent empty apt state prevents a rejected index from falling
      // back to cached data, and APT_CONFIG prevents host hooks/sources loading.
      const state = join(root, name);
      await mkdir(join(state, "lists/partial"), { recursive: true });
      await mkdir(join(state, "cache/archives/partial"), { recursive: true });
      await writeFile(join(state, "status"), "");
      const trusted = join(root, name === "wrong-key" ? "wrong.gpg" : "trusted.gpg");
      await writeFile(join(state, "sources.list"), `deb [arch=amd64,arm64 signed-by=${trusted}] http://127.0.0.1:${address.port}/${name} staging main\n`);
      const config = join(state, "apt.conf");
      await writeFile(config, `Dir::Etc::Parts "-";\nDir::Etc::main "-";\nDir::Etc::sourcelist "${state}/sources.list";\nDir::Etc::sourceparts "-";\nDir::Etc::trusted "-";\nDir::Etc::trustedparts "-";\nDir::State "${state}";\nDir::State::status "${state}/status";\nDir::Cache "${state}/cache";\nDir::Log "${state}";\nAPT::Architectures { "amd64"; "arm64"; };\nAcquire::Languages "none";\nAcquire::http::Proxy "DIRECT";\nAcquire::Retries "0";\nAcquire::Check-Valid-Until "true";\nAPT::Update::Error-Mode "any";\n`);
      const env = { ...process.env, APT_CONFIG: config };
      const result = await command("apt-get", ["update"], env);
      console.log(result.output);
      if (name === "valid") {
        assert.equal(result.code, 0, result.output);
        for (const architecture of ["amd64", "arm64"]) {
          assert(requests.some((path) => path.startsWith(`valid/dists/staging/main/binary-${architecture}/by-hash/SHA256/`)), `No by-hash request for ${architecture}: ${requests}`);
          const candidate = await command("apt-cache", ["policy", `kanna-apt-interop:${architecture}`], env);
          assert.equal(candidate.code, 0, candidate.output);
          assert.match(candidate.output, /Candidate: 1\.0/);
        }
      } else {
        assert.notEqual(result.code, 0, result.output);
        assert.match(result.output, name === "expired" ? /expired/i : name === "wrong-key" ? /NO_PUBKEY/ : /BADSIG/);
      }
      console.log(`apt ${name}: expected result, exit ${result.code}`);
    }
    console.log("Ubuntu apt/GnuPG interoperability passed; no packages installed.");
  }
} finally {
  if (server.listening) await new Promise<void>((accept, reject) => server.close((error) => error ? reject(error) : accept()));
  await rm(root, { recursive: true, force: true });
}
