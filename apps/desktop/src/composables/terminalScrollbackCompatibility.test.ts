import { Terminal } from "@xterm/xterm"
import { afterEach, describe, expect, it, vi } from "vitest"
import { TerminalScrollbackCompatibilityAddon } from "./terminalScrollbackCompatibility"

const terminals: Terminal[] = []
afterEach(() => {
  vi.restoreAllMocks()
  for (const term of terminals.splice(0)) term.dispose()
})
function terminal() {
  const term = new Terminal({ cols: 12, rows: 6, scrollback: 10 })
  terminals.push(term)
  return term
}
async function write(term: Terminal, bytes: string) {
  await new Promise<void>((resolve) => term.write(bytes, resolve))
}
interface Core {
  _bufferService: { scroll(erase: unknown): void }
  _inputHandler: { _eraseAttrData(): unknown }
}
function core(term: Terminal): Core {
  return (term as unknown as { _core: Core })._core
}

describe("scrollback compatibility safety", () => {
  it.each([
    undefined, null, {},
    { _bufferService: { scroll: 1 }, _inputHandler: { _eraseAttrData() {} } },
    { _bufferService: { scroll() {} }, _inputHandler: { _eraseAttrData: null } },
    { _bufferService: { scroll() {}, buffer: { scrollTop: 0, scrollBottom: NaN } }, _inputHandler: { _eraseAttrData() {} } },
  ])("rejects incompatible private shape %j before parser registration", (value) => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {})
    const registerCsiHandler = vi.fn()
    const term = { _core: value, rows: 6, parser: { registerCsiHandler } } as unknown as Terminal
    const addon = new TerminalScrollbackCompatibilityAddon()
    expect(() => addon.activate(term)).not.toThrow()
    expect(registerCsiHandler).not.toHaveBeenCalled()
    expect(warn).toHaveBeenCalledTimes(1)
    addon.dispose()
  })

  it("contains a throwing internal getter before registration", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {})
    const term = { get _core() { throw new Error("shape changed") } } as unknown as Terminal
    expect(() => new TerminalScrollbackCompatibilityAddon().activate(term)).not.toThrow()
    expect(warn).toHaveBeenCalledTimes(1)
  })

  it("rechecks current margins and disables once before an invalid-region mutation", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {})
    const scroll = vi.fn()
    const service = { scroll, buffer: { scrollTop: 0, scrollBottom: 5 } }
    const registered = vi.fn<Terminal["parser"]["registerCsiHandler"]>(() => ({ dispose() {} }))
    const term = {
      rows: 6, buffer: { active: { type: "normal" } },
      _core: { _bufferService: service, _inputHandler: { _eraseAttrData() { return {} } } },
      parser: { registerCsiHandler: registered },
    } as unknown as Terminal
    const addon = new TerminalScrollbackCompatibilityAddon()
    addon.activate(term)
    service.buffer = { scrollTop: 0, scrollBottom: Infinity }
    const handler = registered.mock.calls[0][1]
    expect(handler([4])).toBe(false)
    expect(handler([4])).toBe(false)
    expect(scroll).not.toHaveBeenCalled()
    expect(warn).toHaveBeenCalledTimes(1)
    addon.dispose()
  })

  it.each(["\x1b[1:2S", "\x1b[?4S", "\x1b[3;6r\x1b[2S"])("falls through without mutation for %j", async (sequence) => {
    const term = terminal()
    const stock = terminal()
    term.loadAddon(new TerminalScrollbackCompatibilityAddon())
    const setup = "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix"
    await write(term, setup + sequence)
    await write(stock, setup + sequence)
    const lines = (t: Terminal) => Array.from({ length: t.buffer.active.length }, (_, n) => t.buffer.active.getLine(n)?.translateToString())
    expect(term.buffer.active.baseY).toBe(stock.buffer.active.baseY)
    expect(lines(term)).toEqual(lines(stock))
  })

  it("falls through on a pre-mutation failure and keeps the write queue alive", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {})
    const term = terminal()
    term.loadAddon(new TerminalScrollbackCompatibilityAddon())
    vi.spyOn(core(term)._inputHandler, "_eraseAttrData").mockImplementationOnce(() => { throw new Error("bad attributes") })
    await write(term, "\x1b[2S")
    await write(term, "\x1b[Sok")
    expect(term.buffer.active.baseY).toBe(0)
    expect(term.buffer.active.getLine(0)?.translateToString(true)).toBe("ok")
    expect(warn).toHaveBeenCalledTimes(1)
  })

  it("does not replay a partially applied SU when the primitive fails", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {})
    const term = terminal()
    term.loadAddon(new TerminalScrollbackCompatibilityAddon())
    const service = core(term)._bufferService
    const scroll = service.scroll.bind(service)
    const spy = vi.spyOn(service, "scroll")
      .mockImplementationOnce(scroll)
      .mockImplementationOnce(() => { throw new Error("scroll failed") })
    await write(term, "one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\x1b[1;1H\x1b[3S")
    expect(term.buffer.active.baseY).toBe(1)
    expect(term.buffer.active.getLine(1)?.translateToString(true)).toBe("two")
    expect(spy).toHaveBeenCalledTimes(2)
    await write(term, "\x1b[Sok") // stock handler after disable; no second warning
    expect(term.buffer.active.baseY).toBe(1)
    expect(warn).toHaveBeenCalledTimes(1)
  })

  it("unregisters with terminal disposal and survives RIS until then", async () => {
    const term = terminal()
    const registered = vi.spyOn(term.parser, "registerCsiHandler")
    term.loadAddon(new TerminalScrollbackCompatibilityAddon())
    const registration = registered.mock.results[0].value
    const dispose = vi.spyOn(registration, "dispose")
    await write(term, "\x1bc\x1b[S")
    expect(term.buffer.active.baseY).toBe(1)
    term.dispose()
    terminals.splice(terminals.indexOf(term), 1)
    expect(dispose).toHaveBeenCalledTimes(1)
  })
})
