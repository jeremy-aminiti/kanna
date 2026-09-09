import { describe, expect, it } from "vitest";
import {
  DEFAULT_MOBILE_TERMINAL_GEOMETRY,
  resolveMobileTerminalGeometry
} from "./mobileTerminalGeometry";

describe("resolveMobileTerminalGeometry", () => {
  it("estimates what the phone can show rather than a desktop-shaped floor", () => {
    // A phone is ~390pt wide. Proposing 80 columns from it asked the daemon
    // for a grid twice as wide as the screen, which is the horizontal
    // scrolling the owner was reading through.
    expect(resolveMobileTerminalGeometry({ width: 390, height: 844 })).toEqual({
      cols: 48,
      rows: 41
    });
  });

  it("expands the grid for an iPad-sized task detail surface", () => {
    expect(resolveMobileTerminalGeometry({ width: 1024, height: 1366 })).toEqual({
      cols: 128,
      rows: 72
    });
  });

  it("floors fractional cells instead of overflowing the viewport", () => {
    expect(resolveMobileTerminalGeometry({ width: 799.9, height: 1000.9 })).toEqual({
      cols: 99,
      rows: 51
    });
  });

  it.each([
    null,
    { width: 0, height: 844 },
    { width: 390, height: Number.NaN },
    { width: Number.POSITIVE_INFINITY, height: 844 },
    // Laid out, but too small to be anyone's terminal: still settling.
    { width: 100, height: 844 },
    { width: 390, height: 180 }
  ])("falls back to the conventional grid for an unusable layout: %o", (layout) => {
    expect(resolveMobileTerminalGeometry(layout)).toEqual({ cols: 80, rows: 24 });
  });

  it("exposes the conventional grid as the frozen fallback", () => {
    expect(DEFAULT_MOBILE_TERMINAL_GEOMETRY).toEqual({ cols: 80, rows: 24 });
    expect(Object.isFrozen(DEFAULT_MOBILE_TERMINAL_GEOMETRY)).toBe(true);
  });
});
