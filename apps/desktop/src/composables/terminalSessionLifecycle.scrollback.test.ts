import { readFileSync } from "node:fs"
import { resolve } from "node:path"
import { Terminal } from "@xterm/xterm"
import type { StreamClient } from "@kanna/stream-client"
import { mount } from "@vue/test-utils"
import { defineComponent, h, ref, shallowRef, vShow, withDirectives } from "vue"
import { afterEach, describe, expect, it, vi } from "vitest"
import { createTerminalSessionLifecycle } from "./terminalSessionLifecycle"
import * as recovery from "./terminalSessionRecovery"
import { createTerminalRuntimeState } from "./terminalRuntimeState"
import { TerminalScrollbackCompatibilityAddon } from "./terminalScrollbackCompatibility"

// Only external listeners/transport and geometry waits are stubbed. The actual
// lifecycle owns pause/startListening/onSnapshot/onOutput and snapshot policy;
// the real xterm queue, parser, retention and addon run at the fixture's grid.
vi.mock("../listen", () => ({ listen: vi.fn(async () => vi.fn()) }))
vi.mock("./desktopStreamClient", () => ({ onSharedStreamConnectionChange: vi.fn(() => vi.fn()) }))

const fixture: {
  cols: number; rows: number; usedVisibleTextFallback: boolean
  frames: Array<{ type: string; data_b64: string }>
} = JSON.parse(readFileSync(resolve(__dirname, "../../../../tests/tui-fidelity/fixtures/codex-post-su-daemon.json"), "utf8"))
type Handlers = Parameters<StreamClient["attachTerminal"]>[1]
const cleanups: Array<() => void> = []
afterEach(() => {
  for (const cleanup of cleanups.splice(0).reverse()) cleanup()
  vi.restoreAllMocks()
})
async function parsed(term: Terminal) {
  await new Promise<void>((resolve) => term.write("", resolve))
}
function lines(term: Terminal) {
  return Array.from({ length: term.buffer.active.length }, (_, n) => term.buffer.active.getLine(n)?.translateToString(true) ?? "")
}
function semanticLines(term: Terminal) {
  return lines(term).filter((line) => line.trim() !== "")
}

function harness() {
  const term = new Terminal({ cols: fixture.cols, rows: fixture.rows, scrollback: 10000 })
  term.loadAddon(new TerminalScrollbackCompatibilityAddon())
  const state = createTerminalRuntimeState()
  let handlers: Handlers | undefined
  let attaches = 0
  const client = {
    attachTerminal: vi.fn((_id: string, next: Handlers) => {
      handlers = next
      next.onSnapshot?.(fixture.cols, fixture.rows, fixture.frames[attaches++ === 0 ? 0 : 3].data_b64, "codex")
    }),
    detach: vi.fn(), registerTerminalViewer: vi.fn(), sendTermResize: vi.fn(), sendTermInput: vi.fn(),
  } satisfies Partial<StreamClient>
  // A bounded in-memory transport implements only the methods this lifecycle
  // uses; no real socket or native terminal is opened.
  const stream = client as unknown as StreamClient
  state.streamClient = stream
  const sendInputBytes = vi.fn(async (_bytes: Uint8Array) => {})
  const lifecycle = createTerminalSessionLifecycle({
    sessionId: "fidelity-lifecycle", instanceId: "fidelity-lifecycle-instance",
    state, terminal: shallowRef(term), options: { agentProvider: "codex", agentTerminal: true },
    getTerminalStreamClient: async () => stream,
    inputQueue: { sendInputBytes, flushQueuedInput: vi.fn(async () => {}), clearPendingInputFlushTimer: vi.fn() },
    clipboardBridge: {
      maybeReadClipboardImage: vi.fn(async () => {}), handleTerminalOutputControlSequences: vi.fn(),
      restoreTerminalModesFromSnapshot: vi.fn(), sendDroppedPaths: vi.fn(), reset: vi.fn(),
    },
    layout: {
      ensureFitted: vi.fn(async () => {}), waitForReconnectRedrawSettle: vi.fn(async () => {}),
      waitForReconnectResizeDelay: vi.fn(async () => {}), resizeLiveSession: vi.fn(async () => {}),
      fit: vi.fn(), fitDeferred: vi.fn(), cancelPendingFit: vi.fn(),
    },
    toast: { warning: vi.fn() },
  })
  term.onData((data) => void sendInputBytes(new TextEncoder().encode(data)))
  cleanups.push(() => lifecycle.dispose())
  return {
    term, state, client, lifecycle, sendInputBytes,
    async attachAndScroll() {
      expect(fixture.usedVisibleTextFallback).toBe(false)
      await lifecycle.startListening()
      await parsed(term)
      if (!handlers) throw new Error("lifecycle did not register terminal stream handlers")
      const repliesBefore = sendInputBytes.mock.calls.length
      for (const frame of fixture.frames.slice(1, 3)) handlers.onOutput(frame.data_b64)
      await parsed(term)
      expect(sendInputBytes).toHaveBeenCalledTimes(repliesBefore)
      expect(client.attachTerminal).toHaveBeenCalledTimes(1)
      expect(state.attached).toBe(true)
      expect(semanticLines(term)).toContain("FIDELITY_94bcfc40_SUBMITTED_MESSAGE")
      expect(semanticLines(term)).toContain("FIDELITY_94bcfc40_ASSISTANT_RESPONSE")
    },
  }
}

describe("authoritative scrollback through the actual terminal lifecycle", () => {
  it.each([false, true])("reattachment history and anchor (obsolete exemption negative control=%s)", async (obsoleteExemption) => {
    if (obsoleteExemption) {
      vi.spyOn(recovery, "shouldResetTerminalForSnapshot").mockImplementation((params) =>
        !params.preserveRecoveredScrollback && (params.sessionRespawned || params.agentProvider !== "codex"))
    }
    const { term, state, client, lifecycle, sendInputBytes, attachAndScroll } = harness()
    await attachAndScroll()
    term.scrollToLine(2)
    const before = semanticLines(term)
    const viewportBefore = term.buffer.active.viewportY
    const anchorBefore = lines(term)[viewportBefore]
    lifecycle.pause()
    expect(client.detach).toHaveBeenCalledExactlyOnceWith("fidelity-lifecycle", "terminal")
    expect(state.paused).toBe(true)
    await lifecycle.startListening()
    await parsed(term)
    expect(client.attachTerminal).toHaveBeenCalledTimes(2)
    expect(state.attached).toBe(true)
    expect(state.resetTerminalOnNextSnapshot).toBe(false)
    // The real snapshot enables focus reporting; an unopened xterm emits its
    // existing focus-out reply. These are terminal replies, never replayed
    // message text. The SU itself produces no input (asserted above).
    expect(sendInputBytes.mock.calls.map(([bytes]) => new TextDecoder().decode(bytes))).toEqual(["\x1b[O", "\x1b[O"])
    const after = semanticLines(term)
    const count = (all: string[], marker: string) => all.filter((line) => line.includes(marker)).length
    console.info("fidelity lifecycle result", {
      beforeRows: before.length, afterRows: after.length,
      messageBefore: count(before, "FIDELITY_94bcfc40_SUBMITTED_MESSAGE"),
      messageAfter: count(after, "FIDELITY_94bcfc40_SUBMITTED_MESSAGE"),
      responseBefore: count(before, "FIDELITY_94bcfc40_ASSISTANT_RESPONSE"),
      responseAfter: count(after, "FIDELITY_94bcfc40_ASSISTANT_RESPONSE"),
      viewportBefore, viewportAfter: term.buffer.active.viewportY,
    })
    if (obsoleteExemption) {
      expect(before).toHaveLength(26)
      expect(after).toHaveLength(39)
      expect(count(after, "FIDELITY_94bcfc40_SUBMITTED_MESSAGE")).toBe(2)
      expect(count(after, "FIDELITY_94bcfc40_ASSISTANT_RESPONSE")).toBe(2)
      expect(after).not.toEqual(before)
    } else {
      expect(after).toEqual(before)
      expect(count(after, "FIDELITY_94bcfc40_SUBMITTED_MESSAGE")).toBe(1)
      expect(count(after, "FIDELITY_94bcfc40_ASSISTANT_RESPONSE")).toBe(1)
    }
    expect.soft(term.buffer.active.viewportY).toBe(viewportBefore)
    expect(lines(term)[term.buffer.active.viewportY]).toBe(anchorBefore)
  })

  it("retains a v-show-hidden viewer without detaching or replaying a snapshot", async () => {
    const { term, client, lifecycle, sendInputBytes, attachAndScroll } = harness()
    await attachAndScroll()
    const active = ref(true)
    // MainPanel's retained-v-show boundary, not a KeepAlive deactivation.
    // This control does not claim to mount the entire MainPanel/TerminalView.
    const wrapper = mount(defineComponent({
      setup: () => () => withDirectives(h("div", { class: "retained-terminal" }), [[vShow, active.value]]),
    }))
    cleanups.push(() => wrapper.unmount())
    term.scrollToLine(2)
    const before = lines(term)
    const viewportBefore = term.buffer.active.viewportY
    active.value = false
    await wrapper.vm.$nextTick()
    expect(wrapper.attributes("style")).toContain("display: none")
    active.value = true
    await wrapper.vm.$nextTick()
    await lifecycle.startListening() // already attached: cannot attach twice
    await parsed(term)
    expect(client.detach).not.toHaveBeenCalled()
    expect(client.attachTerminal).toHaveBeenCalledTimes(1)
    expect(lines(term)).toEqual(before)
    expect(term.buffer.active.viewportY).toBe(viewportBefore)
    expect(sendInputBytes.mock.calls.map(([bytes]) => new TextDecoder().decode(bytes))).toEqual(["\x1b[O"])
  })
})
