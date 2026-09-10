// Run after tsup: node tools/kd/tests/apt-bundle-smoke.mjs <bundle-directory>
// Disposable keys stay in memory and travel to the isolated worker over stdin.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { readFileSync, realpathSync } from "node:fs";
import { createRequire, isBuiltin, registerHooks } from "node:module";
import { dirname, isAbsolute, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const worker = process.argv[2] === "--worker";
const directory = realpathSync(resolve(process.argv[worker ? 3 : 2]));
const entry = resolve(directory, "runtime/linux-apt-signature.js");
const now = new Date("2026-09-10T01:00:00Z");
const release = "Suite: staging\nArchitectures: amd64 arm64\nDate: Thu, 10 Sep 2026 00:00:00 GMT\nValid-Until: Fri, 11 Sep 2026 00:00:00 GMT\n";
const encode = (value) => Buffer.from(value);
function contained(path) {
  const suffix = relative(directory, realpathSync(path));
  assert(!isAbsolute(suffix) && suffix !== ".." && !suffix.startsWith("../"), `Outside bundle: ${path}`);
}

if (worker) {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  const keys = JSON.parse(input);
  registerHooks({
    resolve(specifier, context, nextResolve) {
      if (isBuiltin(specifier)) return nextResolve(specifier, context);
      assert(specifier.startsWith("file:") || specifier.startsWith("./") || specifier.startsWith("../"), `External dependency: ${specifier}`);
      const result = nextResolve(specifier, context);
      assert(result.url.startsWith("file:"), `Non-file dependency: ${result.url}`);
      contained(fileURLToPath(result.url));
      return result;
    },
  });
  // Prove both ESM and the library's createRequire path are fenced.
  await assert.rejects(import("openpgp"), /External dependency/);
  assert.throws(() => createRequire(import.meta.url)("openpgp"), /External dependency/);
  await assert.rejects(import(import.meta.url + "?outside-bundle"), /Outside bundle/);
  const { createAptPublicationSigner, verifyAptRelease } = await import(pathToFileURL(entry).href);
  const signer = await createAptPublicationSigner({ ...keys, now: () => now });
  const signedRelease = await signer.sign(encode(release));
  const verify = (overrides = {}) => verifyAptRelease({ ...keys, now, signedRelease, expectedRelease: encode(release), ...overrides });
  assert.deepEqual(await verify(), encode(release));
  await assert.rejects(signer.sign(encode(`\uFEFF${release}`)), /Invalid apt Release field/);
  await assert.rejects(verify({ expectedRelease: encode(`\uFEFF${release}`) }), /intended content/);
  await assert.rejects(verify({ fingerprint: "0".repeat(40) }), /fingerprint mismatch/);
  await assert.rejects(verify({ signedRelease: encode(Buffer.from(signedRelease).toString().replace("Suite: staging", "Suite: stable")) }), /OpenPGP/);
  await assert.rejects(verify({ now: new Date("2026-09-11T00:00:00Z") }), /expired/);
  console.log(`Bundled RSA sign/verify, tamper, fingerprint and expiry checks passed on ${process.platform}/${process.arch} ${process.version}; only bundle files and Node built-ins allowed.`);
} else {
  const kdRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
  const metadata = JSON.parse(readFileSync(resolve(directory, "metafile-esm.json"), "utf8"));
  const outputs = new Map(Object.entries(metadata.outputs).map(([path, value]) => [resolve(kdRoot, path), value]));
  const visited = new Set();
  function checkClosure(path) {
    if (visited.has(path)) return;
    visited.add(path);
    contained(path);
    const output = outputs.get(path);
    assert(output, `Missing emitted module: ${path}`);
    for (const dependency of output.imports) {
      if (dependency.external) assert(isBuiltin(dependency.path), `External package: ${dependency.path}`);
      else checkClosure(resolve(kdRoot, dependency.path));
    }
  }
  checkClosure(entry);
  assert(readFileSync(resolve(directory, "licenses/openpgp/LICENSE"), "utf8").includes("GNU LESSER GENERAL PUBLIC LICENSE"));
  assert(readFileSync(resolve(directory, "licenses/openpgp/GPL-3.0.txt"), "utf8").includes("GNU GENERAL PUBLIC LICENSE"));
  assert(readFileSync(resolve(directory, "licenses/openpgp/NOTICE.md"), "utf8").includes("6.3.1"));
  const installedSource = resolve(dirname(createRequire(import.meta.url).resolve("openpgp")), "openpgp.mjs");
  assert.deepEqual(readFileSync(resolve(directory, "licenses/openpgp/openpgp-6.3.1.mjs")), readFileSync(installedSource));
  assert(readFileSync(`${entry}.LEGAL.txt`, "utf8").includes("OpenPGP.js"));
  const map = JSON.parse(readFileSync(`${entry}.map`, "utf8"));
  assert(map.sourcesContent.some((source) => source?.includes("OpenPGP.js")));
  const pgp = await import("openpgp");
  const passphrase = "Disposable bundle test ONLY";
  const { privateKey, publicKey } = await pgp.generateKey({
    type: "rsa", rsaBits: 3072, subkeys: [], format: "object", passphrase,
    userIDs: [{ name: "Bundle smoke test ONLY", email: "test@example.invalid" }],
    date: new Date("2026-09-09T00:00:00Z"), config: { v6Keys: false },
  });
  const child = spawn(process.execPath, [fileURLToPath(import.meta.url), "--worker", directory], { stdio: ["pipe", "inherit", "inherit"] });
  child.stdin.end(JSON.stringify({ privateKey: privateKey.armor(), publicKey: publicKey.armor(), fingerprint: publicKey.getFingerprint(), passphrase }));
  const code = await new Promise((accept, reject) => { child.once("error", reject); child.once("exit", accept); });
  assert.equal(code, 0, "Isolated bundle worker failed");
  console.log(`Dependency closure (${visited.size} emitted modules), source map, license and exact copied source checks passed.`);
}
