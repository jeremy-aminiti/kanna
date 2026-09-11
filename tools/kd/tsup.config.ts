import { defineConfig } from "tsup";
import { copyFile, mkdir } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";

const packageRoot = import.meta.dirname;
const openpgpRoot = resolve(dirname(createRequire(import.meta.url).resolve("openpgp")), "../..");

export default defineConfig((options) => ({
  entry: {
    "bin/kd": "src/bin/kd.ts",
    "bin/kd-mcp": "src/bin/kd-mcp.ts",
    // Independently testable distribution entry; release commands are not yet wired.
    "runtime/linux-apt-signature": "src/runtime/linux-apt-signature.ts",
  },
  format: ["esm"],
  target: "node22",
  clean: true,
  sourcemap: true,
  dts: false,
  metafile: true,
  noExternal: ["@modelcontextprotocol/sdk", "smol-toml", "zod", "openpgp"],
  esbuildOptions(options) {
    options.legalComments = "linked";
  },
  async onSuccess() {
    const directory = resolve(packageRoot, options.outDir ?? "dist", "licenses/openpgp");
    await mkdir(directory, { recursive: true });
    await copyFile(resolve(openpgpRoot, "LICENSE"), resolve(directory, "LICENSE"));
    await copyFile(resolve(packageRoot, "licenses/GPL-3.0.txt"), resolve(directory, "GPL-3.0.txt"));
    await copyFile(resolve(packageRoot, "OPENPGP-NOTICE.md"), resolve(directory, "NOTICE.md"));
    // Keep the exact distributed library source, including embedded dependency
    // notices, beside its license. Sourcemaps also retain sourcesContent.
    await copyFile(resolve(openpgpRoot, "dist/node/openpgp.mjs"), resolve(directory, "openpgp-6.3.1.mjs"));
  },
}));
