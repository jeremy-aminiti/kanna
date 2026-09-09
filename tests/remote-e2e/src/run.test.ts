import { describe, expect, it, vi } from "vitest";
import { dispatchRemoteE2e } from "./run";

describe("remote E2E dispatcher", () => {
  it("dispatches only the focused terminal-control mobile command", async () => {
    const run = vi.fn().mockResolvedValue(undefined);
    await dispatchRemoteE2e(["--dev", "--mobile-relay-terminal-control"], { run, env: { NODE_ENV: "test" } });
    expect(run).toHaveBeenCalledTimes(1);
    expect(run).toHaveBeenCalledWith(
      "pnpm",
      ["--dir", "apps/mobile", "run", "test:e2e:relay-terminal-control"],
      expect.any(Object),
    );
  });

  it("refuses staging focused modes before spawning", async () => {
    const run = vi.fn();
    await expect(dispatchRemoteE2e(["--staging", "--mobile-relay-terminal-control"], { run, env: { NODE_ENV: "test" } }))
      .rejects.toThrow("human-gated");
    expect(run).not.toHaveBeenCalled();
  });

  it("preserves the ordinary mobile relay dispatch", async () => {
    const run = vi.fn().mockResolvedValue(undefined);
    await dispatchRemoteE2e(["--dev", "--mobile-relay"], { run, env: { NODE_ENV: "test" } });
    expect(run).toHaveBeenCalledWith(
      "pnpm", ["--dir", "apps/mobile", "run", "test:e2e:relay"], expect.any(Object),
    );
  });
});
