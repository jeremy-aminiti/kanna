import { describe, expect, it } from "vitest";
import {
  isTerminalTransportGap,
  resolveTerminalPresentationStatus,
  TERMINAL_RECONNECT_GRACE_MS
} from "./terminalReconnectPresentation";

describe("isTerminalTransportGap", () => {
  it("names only the states the transport passes through between attachments", () => {
    expect(isTerminalTransportGap("connecting")).toBe(true);
    expect(isTerminalTransportGap("restarting")).toBe(true);
    expect(isTerminalTransportGap("live")).toBe(false);
    expect(isTerminalTransportGap("idle")).toBe(false);
    expect(isTerminalTransportGap("closed")).toBe(false);
    expect(isTerminalTransportGap("error")).toBe(false);
  });
});

describe("resolveTerminalPresentationStatus", () => {
  it("keeps a rendered grid live across a sub-threshold gap", () => {
    for (const status of ["connecting", "restarting"] as const) {
      expect(
        resolveTerminalPresentationStatus({
          status,
          hasRenderedGrid: true,
          gapExceededGrace: false
        })
      ).toBe("live");
    }
  });

  it("reports the gap once it outlasts the grace window", () => {
    expect(
      resolveTerminalPresentationStatus({
        status: "restarting",
        hasRenderedGrid: true,
        gapExceededGrace: true
      })
    ).toBe("restarting");
  });

  it("reports a gap with nothing rendered to keep", () => {
    // A first connect has no grid to hold: the reader is looking at an empty
    // surface either way, so the connecting state is the honest answer.
    expect(
      resolveTerminalPresentationStatus({
        status: "connecting",
        hasRenderedGrid: false,
        gapExceededGrace: false
      })
    ).toBe("connecting");
  });

  it("never softens a state the transport will not leave on its own", () => {
    for (const status of ["closed", "error", "idle"] as const) {
      expect(
        resolveTerminalPresentationStatus({
          status,
          hasRenderedGrid: true,
          gapExceededGrace: false
        })
      ).toBe(status);
    }
  });

  it("outlasts the stream client's whole reconnect ladder", () => {
    // 250 + 500 + 1000 + 2000 is the ladder in relayClient; the grace has to
    // clear at least a single redial or every drop is still news.
    expect(TERMINAL_RECONNECT_GRACE_MS).toBeGreaterThanOrEqual(2_000);
  });
});
