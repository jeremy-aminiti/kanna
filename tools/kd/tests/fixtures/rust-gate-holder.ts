import { withRustGate } from "../../src/runtime/rust-gate";

const [homeDir, milliseconds] = process.argv.slice(2);
if (!homeDir || !milliseconds) throw new Error("usage: rust-gate-holder <home-dir> <milliseconds>");

const result = await withRustGate({
  homeDir,
  env: process.env,
  run: async () => {
    const startedAt = Date.now();
    await new Promise((resolve) => setTimeout(resolve, Number(milliseconds)));
    return { startedAt, finishedAt: Date.now() };
  }
});
process.stdout.write(`${JSON.stringify(result)}\n`);
