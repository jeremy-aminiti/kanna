import { defineConfig } from "vitest/config";
import { sharedTestOptions } from "../../vitest.shared";

export default defineConfig({
  test: {
    ...sharedTestOptions,
    // apt, a systemd user unit, a real daemon and a real agent session. The
    // upgrade lane installs two packages in one test.
    testTimeout: 480_000,
    hookTimeout: 480_000,
  },
});
