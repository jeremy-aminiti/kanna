import { readFileSync } from "node:fs"
import { createHash } from "node:crypto"
import { resolve } from "node:path"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { ref } from "vue"
import { Terminal, type IDisposable } from "@xterm/xterm"
import { FitAddon } from "@xterm/addon-fit"
import { initializeTerminalView, type InitializedTerminalView } from "./terminalView"
import { createTerminalRuntimeState } from "./terminalRuntimeState"
import { applyTerminalSnapshot } from "./terminalSnapshotApply"
import { shouldResetTerminalForSnapshot } from "./terminalSessionRecovery"
import { TerminalScrollbackCompatibilityAddon } from "./terminalScrollbackCompatibility"

vi.mock("./terminalRenderer", async (importOriginal) => ({
  ...await importOriginal<typeof import("./terminalRenderer")>(),
  requestedTerminalRenderer: () => "dom",
}))

const fixtureRoot = resolve(__dirname, "../../../../tests/tui-fidelity")
const capture = readFileSync(resolve(fixtureRoot, "fixtures/codex-live-20260905.ansi"))
// Actual Codex output: scroll four rows above the fixed composer/footer.
const capturedScroll = Buffer.from("\x1b[1;31r\x1b[4S\x1b[r")
const scrollOffset = capture.indexOf(capturedScroll)
const message = "FIDELITY_94bcfc40_SUBMITTED_MESSAGE"
const response = "FIDELITY_94bcfc40_ASSISTANT_RESPONSE"
const views: InitializedTerminalView[] = []
const adapters: IDisposable[] = []
const retained: { pathSerialized: string; usedVisibleTextFallback: boolean } = JSON.parse(
  readFileSync(resolve(fixtureRoot, "goldens/codex-live-20260905.json"), "utf8"),
)

beforeEach(() => {
  // This lane exercises production initialization and the real xterm parser,
  // buffer, addons and write queue. Only browser painting is absent: native
  // rendered evidence belongs to the separately gated desktop E2E lane.
  vi.spyOn(Terminal.prototype, "open").mockImplementation(() => {})
})

afterEach(() => {
  for (const adapter of adapters.splice(0)) adapter.dispose()
  for (const view of views.splice(0)) {
    view.cleanupContainerEvents?.()
    view.stopThemeWatch()
    view.stopFileLinkAvailabilityWatch()
    view.unregisterE2ETerminalBuffer()
    view.unregisterFileLinkProvider()
    view.term.dispose()
  }
  vi.restoreAllMocks()
})

function desktopTerminal(agentProvider = "codex", stock = false): Terminal {
  // Negative controls use the same initialization, disabling only this addon.
  const disabled = stock ? vi.spyOn(TerminalScrollbackCompatibilityAddon.prototype, "activate").mockImplementationOnce(() => {}) : undefined
  const el = document.createElement("div")
  const view = initializeTerminalView({
    el,
    state: createTerminalRuntimeState(),
    sessionId: "fidelity-test",
    instanceId: `fidelity-${views.length}`,
    options: { agentProvider, agentTerminal: true },
    effectiveCodeTheme: ref("dark"),
    fitAddon: new FitAddon(),
    getContainer: () => el,
    isDisposed: () => false,
    isAttached: () => false,
    getStreamClient: () => null,
    handleLinkActivate: vi.fn(),
    sendInputBytes: vi.fn(async () => {}),
    maybeReadClipboardImage: vi.fn(async () => {}),
    sendDroppedPaths: vi.fn(),
    onNativeDropCleanupReady: vi.fn(),
    setTerminal: vi.fn(),
  })
  disabled?.mockRestore()
  views.push(view)
  view.term.resize(120, 36)
  return view.term
}

async function write(term: Terminal, bytes: string | Uint8Array): Promise<void> {
  await new Promise<void>((resolve) => term.write(bytes, resolve))
}

function lines(term: Terminal, start = 0, end = term.buffer.active.length): string[] {
  return Array.from({ length: end - start }, (_, row) =>
    term.buffer.active.getLine(start + row)?.translateToString(true) ?? "",
  )
}

function screen(term: Terminal): string[] {
  return lines(term, term.buffer.active.baseY, term.buffer.active.baseY + term.rows)
}

describe("desktop Codex live scrollback", () => {
  it("retains distinct message/response rows when the captured top-origin region scrolls", async () => {
    expect(scrollOffset).toBe(21233)
    const term = desktopTerminal()
    await write(term, capture.subarray(0, scrollOffset))
    expect(term.buffer.active.type).toBe("normal")
    // Mark the two rows about to leave the region. These are PTY output cells,
    // not an input replay or a separate text-history representation.
    await write(term, `\x1b[1;1H\x1b[2K${message}\x1b[2;1H\x1b[2K${response}`)
    const before = screen(term)
    const baseBefore = term.buffer.active.baseY
    expect(before.slice(0, 2)).toEqual([message, response])

    // Split the escape sequence across writes, as a PTY/KSP stream may do.
    await write(term, capturedScroll.subarray(0, 10))
    await write(term, capturedScroll.subarray(10))

    // Live screen painting looks correct even on the broken parser: the
    // missing rows are specifically a scrollback-retention failure.
    expect(screen(term)).toEqual([
      ...before.slice(4, 31), "", "", "", "", ...before.slice(31),
    ])
    expect.soft(lines(term)).toContain(message)
    expect.soft(lines(term)).toContain(response)
    expect(term.buffer.active.baseY).toBe(baseBefore + 4)
  })

  it("compares the unmodified capture with its retained fixture output", async () => {
    expect(retained.usedVisibleTextFallback).toBe(false)
    const live = desktopTerminal()
    const restored = desktopTerminal()
    await write(live, capture)
    // This is the committed fixture's serialized result of the daemon/mobile
    // fidelity path, NOT a new daemon execution or the owner's current .16
    // snapshot. Both daemon cutovers (13000 / 21200) PRECEDE the first SU
    // (21240): this golden does not establish daemon SU retention. Reuse it
    // without regenerating or changing it.
    await write(restored, retained.pathSerialized)
    for (const marker of ["Fidelity probe turn one:", "Fidelity probe turn two:", "Fidelity probe turn three."]) {
      expect(lines(restored).join("\n")).toContain(marker)
      expect(lines(live).join("\n")).toContain(marker)
    }
    expect(screen(live)).toEqual(screen(restored))
  })

  it.each(["codex", "claude"])("keeps inset-region scrolling out of history for %s", async (provider) => {
    const term = desktopTerminal(provider)
    await write(term, `\x1b[1;1HHEADER\x1b[2;1H${message}\x1b[36;1HFOOTER`)
    await write(term, "\x1b[2;31r\x1b[4S\x1b[r")
    expect(term.buffer.active.baseY).toBe(0)
    expect(lines(term)).not.toContain(message)
    expect(screen(term)[0]).toBe("HEADER")
    expect(screen(term)[35]).toBe("FOOTER")
  })

  it("does not turn alternate-screen output into primary scrollback", async () => {
    const term = desktopTerminal()
    await write(term, "PRIMARY\x1b[?1049h")
    await write(term, `\x1b[1;1H${message}\x1b[36;1HFOOTER`)
    await write(term, capturedScroll)
    expect(term.buffer.active.type).toBe("alternate")
    expect(term.buffer.active.baseY).toBe(0)
    expect(lines(term)).not.toContain(message)
    expect(screen(term)[35]).toBe("FOOTER")
    await write(term, "\x1b[?1049l")
    expect(term.buffer.active.type).toBe("normal")
    expect(lines(term)[0]).toBe("PRIMARY")
    expect(lines(term)).not.toContain(message)
  })
})

// Explicit private shape for the dependency-version contract only.
interface PinnedXtermCore {
  _bufferService: {
    buffer: { scrollTop: number; scrollBottom: number }
    scroll(eraseAttributes: unknown, isWrapped?: boolean): void
  }
  _inputHandler: { _eraseAttrData(): unknown }
}

function installCompatibilityAdapter(term: Terminal): IDisposable {
  const adapter = new TerminalScrollbackCompatibilityAddon()
  term.loadAddon(adapter)
  adapters.push(adapter)
  return adapter
}

function cells(term: Terminal, start = 0, end = term.buffer.active.length) {
  return Array.from({ length: end - start }, (_, index) => {
    const line = term.buffer.active.getLine(start + index)
    return {
      wrapped: line?.isWrapped,
      cells: Array.from({ length: term.cols }, (_, column) => {
        const cell = line?.getCell(column)
        return cell && {
          text: cell.getChars(), width: cell.getWidth(),
          fg: cell.getFgColor(), fgMode: cell.getFgColorMode(),
          bg: cell.getBgColor(), bgMode: cell.getBgColorMode(),
          bold: cell.isBold(), dim: cell.isDim(), italic: cell.isItalic(),
          underline: cell.isUnderline(), inverse: cell.isInverse(),
        }
      }),
    }
  })
}

function cursor(term: Terminal) {
  return { x: term.buffer.active.cursorX, y: term.buffer.active.cursorY }
}

function rowFixture(): string {
  return Array.from({ length: 36 }, (_, row) => `\x1b[${row + 1};1HROW_${row + 1}`).join("")
}

function semanticLines(term: Terminal): string[] {
  return lines(term).filter((line) => line.trim() !== "")
}

async function snapshot(term: Terminal, data: string, replaceBuffer: boolean): Promise<void> {
  await new Promise<void>((resolve) => applyTerminalSnapshot({
    terminal: term, cols: 120, rows: 36, data, replaceBuffer, onParsed: resolve,
  }))
}

describe("instance adapter contract", () => {
  it("pins the private adapter shape to the installed xterm version", () => {
    const installed: { version: string } = JSON.parse(readFileSync(
      resolve(__dirname, "../../node_modules/@xterm/xterm/package.json"), "utf8",
    ))
    expect(installed.version).toBe("6.1.0-beta.195")
    const term = desktopTerminal()
    const core = (term as unknown as { _core: PinnedXtermCore })._core
    expect(typeof core._bufferService.scroll).toBe("function")
    expect(typeof core._inputHandler._eraseAttrData).toBe("function")
    expect(core._bufferService.buffer.scrollTop).toBe(0)
    expect(core._bufferService.buffer.scrollBottom).toBe(term.rows - 1)
  })

  it("retains the captured marker rows without changing the live screen", async () => {
    const term = desktopTerminal()
    await write(term, capture.subarray(0, scrollOffset))
    await write(term, `\x1b[1;1H\x1b[2K${message}\x1b[2;1H\x1b[2K${response}`)
    const before = screen(term)
    const baseBefore = term.buffer.active.baseY
    await write(term, capturedScroll.subarray(0, 10))
    await write(term, capturedScroll.subarray(10))
    expect(screen(term)).toEqual([...before.slice(4, 31), "", "", "", "", ...before.slice(31)])
    expect(lines(term).filter((line) => line === message)).toHaveLength(1)
    expect(lines(term).filter((line) => line === response)).toHaveLength(1)
    expect(term.buffer.active.baseY).toBe(baseBefore + 4)
  })

  it("keeps cell attributes, cursor, footer, erase background and scroll events", async () => {
    const term = desktopTerminal()
    const reference = desktopTerminal("codex", true)
    const setup = rowFixture() +
      "\x1b[1;1H\x1b[1;3;4;38;2;10;20;30;48;2;40;50;60m彩色🙂\x1b[0m" +
      "\x1b[36;1H\x1b[2;7mFOOTER\x1b[0m\x1b[1;31r\x1b[12;9H\x1b[42m"
    await write(term, setup)
    await write(reference, setup)
    const before = cells(term)
    const beforeCursor = cursor(term)
    const events = vi.fn()
    const outbound = vi.fn()
    const onScroll = term.onScroll(events)
    const onData = term.onData(outbound)
    await write(term, "\x1b[4S")
    await write(reference, "\x1b[4S")
    expect(cells(term, 0, 4)).toEqual(before.slice(0, 4))
    expect(cells(term, term.buffer.active.baseY)).toEqual(cells(reference))
    expect(cursor(term)).toEqual(beforeCursor)
    expect(events).toHaveBeenCalled()
    expect(outbound).not.toHaveBeenCalled()
    await write(term, "NEXT")
    await write(reference, "NEXT")
    expect(cells(term, term.buffer.active.baseY)).toEqual(cells(reference))
    onScroll.dispose()
    onData.dispose()
  })

  it.each(["codex", "claude", "copilot", "opencode", ""])("retains top-origin SU rows independently of provider %s", async (provider) => {
    const term = desktopTerminal(provider)
    const reference = desktopTerminal(provider)
    const bytes = rowFixture() + capturedScroll.toString()
    await write(term, bytes)
    await write(reference, bytes)
    expect(cells(term)).toEqual(cells(reference))
    expect(term.buffer.active.baseY).toBe(4)
    expect(cursor(term)).toEqual(cursor(reference))
  })

  it.each(["inset", "footer", "alternate"])("falls through for the %s buffer/region", async (kind) => {
    const term = desktopTerminal()
    const reference = desktopTerminal("codex", true)
    const bytes = (kind === "alternate" ? "PRIMARY\x1b[?1049h" : "") + rowFixture() +
      (kind === "inset" ? "\x1b[2;31r\x1b[4S\x1b[r" : kind === "footer" ? "\x1b[30;36r\x1b[4S\x1b[r" : capturedScroll.toString())
    await write(term, bytes)
    await write(reference, bytes)
    expect(cells(term)).toEqual(cells(reference))
    expect(term.buffer.active.baseY).toBe(0)
    if (kind === "alternate") {
      await write(term, "\x1b[?1049l")
      expect(semanticLines(term)).toEqual(["PRIMARY"])
    }
  })

  it.each(["\x1b[S", "\x1b[0S", "\x1b[999S"])("bounds count and keeps the footer for %j", async (scroll) => {
    const term = desktopTerminal()
    await write(term, rowFixture() + "\x1b[1;31r")
    await write(term, scroll)
    expect(term.buffer.active.baseY).toBe(scroll === "\x1b[999S" ? 31 : 1)
    expect(screen(term).slice(31)).toEqual(["ROW_32", "ROW_33", "ROW_34", "ROW_35", "ROW_36"])
  })

  it("preserves scroll position while reading history and obeys capacity trimming", async () => {
    const term = desktopTerminal()
    term.options.scrollback = 6
    await write(term, rowFixture() + "\x1b[1;31r\x1b[4S")
    term.scrollToLine(3)
    expect(term.buffer.active.viewportY).toBe(3)
    const reading = lines(term, 3, 6)
    await write(term, "\x1b[S")
    expect(term.buffer.active.viewportY).toBe(3)
    expect(lines(term, 3, 6)).toEqual(reading)
    await write(term, "\x1b[3S")
    expect(term.buffer.active.baseY).toBe(6)
    expect(term.buffer.active.length).toBe(term.rows + 6)
    expect(term.buffer.active.viewportY).toBe(1)
    expect(lines(term, 1, 4)).toEqual(reading)
    expect(lines(term)[0]).toBe("ROW_3")
  })

  it("preserves wrapped cells through subsequent width reflow", async () => {
    const term = desktopTerminal()
    const text = "wrapped_".repeat(20)
    await write(term, text + "\x1b[1;31r\x1b[2S")
    expect(term.buffer.active.getLine(1)?.isWrapped).toBe(true)
    expect(lines(term, 0, 2).join("")).toBe(text)
    term.resize(80, 36)
    expect(lines(term, 0, 2).join("")).toBe(text)
  })

  it("is disposable and does not modify another terminal instance", async () => {
    const term = desktopTerminal("codex", true)
    const other = desktopTerminal("codex", true)
    const adapter = installCompatibilityAdapter(term)
    await write(term, rowFixture() + capturedScroll.toString())
    await write(other, rowFixture() + capturedScroll.toString())
    expect(term.buffer.active.baseY).toBe(4)
    expect(other.buffer.active.baseY).toBe(0)
    adapter.dispose()
    await write(term, capturedScroll)
    expect(term.buffer.active.baseY).toBe(4)
  })

  it("leaves ordinary linefeed scrolling and explicit scrollback erase unchanged", async () => {
    const term = desktopTerminal()
    const reference = desktopTerminal("codex", true)
    const bytes = Array.from({ length: 50 }, (_, row) => `LINE_${row}\r\n`).join("")
    await write(term, bytes)
    await write(reference, bytes)
    expect(cells(term)).toEqual(cells(reference))
    await write(term, "\x1b[3J")
    await write(reference, "\x1b[3J")
    expect(cells(term)).toEqual(cells(reference))
    expect(term.buffer.active.baseY).toBe(0)
  })
})

describe("snapshot continuity at the existing desktop application helpers", () => {
  it.each([false, true])("ordinary Codex snapshot must preserve semantic history (adapter=%s)", async (adapted) => {
    const term = desktopTerminal("codex", !adapted)
    await snapshot(term, retained.pathSerialized, true)
    const before = semanticLines(term)
    const replaceBuffer = shouldResetTerminalForSnapshot({
      agentProvider: "codex", preserveRecoveredScrollback: false, sessionRespawned: false,
    })
    // The retained fixture has the same full-state, relative-paint contract
    // as daemon snapshots. This proves helper behavior, not a new daemon
    // capture or an actual tab switch. The production policy remains intact.
    await snapshot(term, retained.pathSerialized, replaceBuffer)
    expect(semanticLines(term)).toEqual(before)
  })

  it("a replacing snapshot is idempotent and keeps the adapter after RIS", async () => {
    const term = desktopTerminal()
    await snapshot(term, retained.pathSerialized, true)
    const before = cells(term)
    const beforeCursor = cursor(term)
    // Deliberately leave a write queued: the existing helper's RIS must
    // follow it in the parser queue instead of resetting synchronously.
    term.write("queued stale output\r\n")
    await snapshot(term, retained.pathSerialized, true)
    expect(cells(term)).toEqual(before)
    expect(cursor(term)).toEqual(beforeCursor)
    const base = term.buffer.active.baseY
    await write(term, capturedScroll)
    expect(term.buffer.active.baseY).toBe(base + 4)
  })
})

// This opt-in comparison requires real output from
// the existing tui-fidelity-emit binary. It never invokes Cargo/native code.
// .tmp/prepare-codex-post-su.py prepares two bounded inputs and exact commands;
// execute those commands only after the native lane is explicitly released.
// Missing emission files fail an opted-in run rather than fabricating a golden.
describe.runIf(process.env.KANNA_CODEX_POST_SU_PROOF === "1")("authoritative post-SU daemon comparison", () => {
  it.each(["capture", "marked"])("compares %s outgoing rows with an actual post-SU daemon snapshot", async (name) => {
    const evidenceRoot = resolve(__dirname, "../../../../.tmp/codex-post-su")
    const manifest: {
      captureSha256: string
      cases: Array<{
        name: string; fixture: string; sha256: string
        snapshotAt: number; resnapshotAt: number
      }>
    } = JSON.parse(readFileSync(resolve(evidenceRoot, "manifest.json"), "utf8"))
    const entry = manifest.cases.find((entry) => entry.name === name)
    if (!entry) throw new Error(`Missing prepared daemon comparison case: ${name}`)
    const input = readFileSync(entry.fixture)
    expect(createHash("sha256").update(capture).digest("hex")).toBe(manifest.captureSha256)
    expect(createHash("sha256").update(input).digest("hex")).toBe(entry.sha256)
    expect(input.subarray(0, scrollOffset)).toEqual(capture.subarray(0, scrollOffset))
    expect(input.subarray(entry.snapshotAt)).toEqual(capturedScroll)
    expect(entry.resnapshotAt).toBe(input.length)
    expect(entry.resnapshotAt).toBeGreaterThan(21244)

    const emission: {
      fixture: string; cols: number; rows: number
      snapshot_at: number; resnapshot_at: number
      used_visible_text_fallback: boolean
      frames: Array<{ type: string; cols?: number; rows?: number; data_b64: string }>
    } = JSON.parse(readFileSync(resolve(evidenceRoot, `${name}.emission.json`), "utf8"))
    expect(emission.fixture).toBe(entry.fixture)
    expect([emission.cols, emission.rows]).toEqual([120, 36])
    expect(emission.snapshot_at).toBe(entry.snapshotAt)
    expect(emission.resnapshot_at).toBe(entry.resnapshotAt)
    expect(emission.used_visible_text_fallback).toBe(false)
    // No tail after resnapshot: the second snapshot MUST contain the SU's
    // result. This avoids the old golden's live-xterm-on-both-sides mistake.
    expect(emission.frames.map((frame) => frame.type)).toEqual([
      "term_snapshot", "term_output", "term_output", "term_snapshot",
    ])
    for (const index of [0, 3]) {
      expect([emission.frames[index].cols, emission.frames[index].rows]).toEqual([120, 36])
    }
    const chunks = emission.frames.slice(1, 3).map((frame) => Buffer.from(frame.data_b64, "base64"))
    expect(Buffer.concat(chunks)).toEqual(capturedScroll)

    const daemonBefore = desktopTerminal()
    const daemonAfter = desktopTerminal()
    await write(daemonBefore, Buffer.from(emission.frames[0].data_b64, "base64"))
    await write(daemonAfter, Buffer.from(emission.frames[3].data_b64, "base64"))
    expect(daemonBefore.buffer.active.type).toBe("normal")
    expect(daemonAfter.buffer.active.type).toBe("normal")
    const beforeBase = daemonBefore.buffer.active.baseY
    const beforeLines = lines(daemonBefore)
    const beforeScreen = screen(daemonBefore)
    // The authoritative snapshots, not bare xterm or a provider transcript,
    // decide whether the outgoing rows survived. Include history, not merely
    // visible-grid equality. Four inserted blanks sit above the fixed footer.
    expect(daemonAfter.buffer.active.baseY).toBe(beforeBase + 4)
    expect(lines(daemonAfter)).toEqual([
      ...beforeLines.slice(0, beforeBase + 31), "", "", "", "",
      ...beforeLines.slice(beforeBase + 31),
    ])
    expect(screen(daemonAfter)).toEqual([
      ...beforeScreen.slice(4, 31), "", "", "", "", ...beforeScreen.slice(31),
    ])

    const baseline = desktopTerminal("codex", true)
    const candidate = desktopTerminal()
    await write(baseline, input.subarray(0, entry.snapshotAt))
    await write(candidate, input.subarray(0, entry.snapshotAt))
    // A pre-existing emulator difference must be diagnosed separately instead
    // of being silently attributed to the SU under investigation.
    expect(cells(baseline)).toEqual(cells(daemonBefore))
    expect(cells(candidate)).toEqual(cells(daemonBefore))
    const baselineBase = baseline.buffer.active.baseY
    for (const chunk of chunks) {
      await write(baseline, chunk)
      await write(candidate, chunk)
    }
    expect(baseline.buffer.active.baseY).toBe(baselineBase)
    expect(screen(baseline)).toEqual(screen(daemonAfter))
    expect(cells(candidate)).toEqual(cells(daemonAfter))
    if (name === "marked") {
      for (const marker of [message, response]) {
        expect(beforeScreen).toContain(marker)
        expect(lines(baseline)).not.toContain(marker) // negative control
        expect(lines(daemonAfter, 0, daemonAfter.buffer.active.baseY)).toContain(marker)
        expect(lines(candidate, 0, candidate.buffer.active.baseY)).toContain(marker)
        expect(screen(daemonAfter)).not.toContain(marker)
      }
    }
  })
})
