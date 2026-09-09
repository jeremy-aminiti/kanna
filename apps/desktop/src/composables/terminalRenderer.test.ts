import { describe, expect, it } from "vitest"
import {
  recordTerminalRendererOutcome,
  requestedTerminalRenderer,
  terminalRendererOutcome,
} from "./terminalRenderer"

describe("requestedTerminalRenderer", () => {
  it("asks for WebGL outside E2E", () => {
    expect(requestedTerminalRenderer({})).toBe("webgl")
  })

  it("defaults an E2E run to the DOM renderer", () => {
    expect(requestedTerminalRenderer({ __KANNA_E2E__: { ready: true } })).toBe("dom")
  })

  it("lets a rendering run opt back into production's renderer", () => {
    expect(
      requestedTerminalRenderer({
        __KANNA_E2E__: { ready: true },
        __KANNA_E2E_TERMINAL_RENDERER__: "webgl",
      }),
    ).toBe("webgl")
  })

  it("takes the request from the URL, which survives the reload a global cannot", () => {
    expect(
      requestedTerminalRenderer({
        __KANNA_E2E__: { ready: true },
        location: { search: "?kannaTerminalRenderer=webgl" },
      }),
    ).toBe("webgl")
  })

  it("keeps the DOM default for an unrelated query string", () => {
    expect(
      requestedTerminalRenderer({
        __KANNA_E2E__: { ready: true },
        location: { search: "?foo=webgl" },
      }),
    ).toBe("dom")
  })

  it("treats an unrecognized request as the DOM default rather than guessing", () => {
    expect(
      requestedTerminalRenderer({
        __KANNA_E2E__: { ready: true },
        __KANNA_E2E_TERMINAL_RENDERER__: "metal",
      }),
    ).toBe("dom")
  })
})

describe("terminalRendererOutcome", () => {
  it("reports what the terminal settled on, not what was asked for", () => {
    recordTerminalRendererOutcome({ renderer: "dom", reason: "unavailable" })
    expect(terminalRendererOutcome()).toEqual({ renderer: "dom", reason: "unavailable" })
    recordTerminalRendererOutcome({ renderer: "webgl" })
    expect(terminalRendererOutcome()).toEqual({ renderer: "webgl" })
  })
})
