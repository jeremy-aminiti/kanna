import { describe, expect, it, vi } from "vitest";
import { createStaticBonjourBrowser } from "./bonjour";
import {
  createExplicitDevelopmentServerBrowser,
  resolveExplicitDevelopmentServerUrl
} from "./explicitDevelopmentServer";

function response(status: number, body: unknown) {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body
  };
}

describe("explicit development server discovery", () => {
  it("is absent outside development even when an endpoint is configured", () => {
    expect(
      resolveExplicitDevelopmentServerUrl("http://10.0.2.2:48120", false)
    ).toBeNull();
  });

  it("accepts only a plain HTTP origin in development", () => {
    expect(
      resolveExplicitDevelopmentServerUrl(" http://10.0.2.2:48120/ ", true)
    ).toBe("http://10.0.2.2:48120");
    expect(() =>
      resolveExplicitDevelopmentServerUrl("https://example.test/path", true)
    ).toThrow(/must be an http origin/);
  });

  it("synthesizes a candidate from the server's real status identity", async () => {
    const fetchImpl = vi.fn(async () => response(200, {
      desktopId: "desktop-real",
      desktopName: "Studio Mac"
    }));
    const browser = createExplicitDevelopmentServerBrowser({
      baseUrl: "http://10.0.2.2:48120",
      browser: createStaticBonjourBrowser([]),
      fetchImpl
    });

    await browser.refresh?.();

    expect(fetchImpl).toHaveBeenCalledWith(
      "http://10.0.2.2:48120/v1/status",
      expect.objectContaining({ signal: expect.any(Object) })
    );
    expect(browser.getServices()).toEqual([{
      name: "Studio Mac",
      type: "_kanna-mobile._tcp.",
      host: "10.0.2.2",
      port: 48120,
      txt: { desktopId: "desktop-real" }
    }]);
  });

  it("does not invent an identity when status is unreachable or malformed", async () => {
    const unreachable = createExplicitDevelopmentServerBrowser({
      baseUrl: "http://10.0.2.2:48120",
      browser: createStaticBonjourBrowser([]),
      fetchImpl: vi.fn(async () => response(503, {}))
    });
    await expect(unreachable.refresh?.()).rejects.toThrow(/valid desktop identity/);
    expect(unreachable.getServices()).toEqual([]);

    const malformed = createExplicitDevelopmentServerBrowser({
      baseUrl: "http://10.0.2.2:48120",
      browser: createStaticBonjourBrowser([]),
      fetchImpl: vi.fn(async () => response(200, { desktopName: "Studio Mac" }))
    });
    await expect(malformed.refresh?.()).rejects.toThrow(/valid desktop identity/);
    expect(malformed.getServices()).toEqual([]);
  });
});
