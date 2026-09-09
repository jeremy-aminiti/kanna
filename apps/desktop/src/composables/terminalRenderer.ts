/**
 * Which renderer an xterm view should use, and which one it actually got.
 *
 * Production always asks for WebGL. E2E defaults to the DOM renderer because
 * screenshots have to match xterm's painted rows — WKWebView will report the
 * WebGL-backed logical buffer while capturing a blank native surface with a
 * second desktop window open. That default is deliberate, but it also meant an
 * E2E run could say nothing about the renderer production uses. A run that is
 * specifically about rendering opts back in by setting
 * `window.__KANNA_E2E_TERMINAL_RENDERER__` before the app loads, and reads what
 * the terminal settled on back out of `outcome` — asking for WebGL is not the
 * same as getting it, and a test that cannot tell the difference proves nothing.
 */
export type TerminalRendererChoice = "webgl" | "dom"

export type TerminalRendererOutcome =
  | { renderer: "webgl" }
  | { renderer: "dom"; reason: "requested" | "unavailable" | "context-lost" }

interface RendererWindow {
  __KANNA_E2E__?: unknown
  __KANNA_E2E_TERMINAL_RENDERER__?: unknown
  location?: { search?: string }
}

/**
 * The query parameter is not a second way to say the same thing. A terminal
 * reads its renderer once, when the view is built, so a global set by a driver
 * that is already attached is always too late; the parameter survives the
 * reload that makes it early enough.
 */
const RENDERER_QUERY_PARAM = "kannaTerminalRenderer"

function requestedFromUrl(win: RendererWindow): string | null {
  const search = win.location?.search
  if (!search) return null
  try {
    return new URLSearchParams(search).get(RENDERER_QUERY_PARAM)
  } catch {
    return null
  }
}

export function requestedTerminalRenderer(win: RendererWindow): TerminalRendererChoice {
  if (!win.__KANNA_E2E__) return "webgl"
  const requested = win.__KANNA_E2E_TERMINAL_RENDERER__ ?? requestedFromUrl(win)
  return requested === "webgl" ? "webgl" : "dom"
}

let lastOutcome: TerminalRendererOutcome | null = null

export function recordTerminalRendererOutcome(outcome: TerminalRendererOutcome): void {
  lastOutcome = outcome
}

export function terminalRendererOutcome(): TerminalRendererOutcome | null {
  return lastOutcome
}
