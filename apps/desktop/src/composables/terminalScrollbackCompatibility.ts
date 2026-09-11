import type { IDisposable, ITerminalAddon, Terminal } from "@xterm/xterm"

// Private shape of @xterm/xterm 6.1.0-beta.195. The contract test pins this
// version: reassess these internals when upgrading, rather than widening casts.
interface XtermScrollCore {
  _bufferService: {
    buffer: { scrollTop: number; scrollBottom: number }
    scroll(eraseAttributes: unknown): void
  }
  _inputHandler: { _eraseAttrData(): unknown }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null
}

function hasScrollCore(value: unknown): value is XtermScrollCore {
  return isRecord(value)
    && isRecord(value._bufferService)
    && typeof value._bufferService.scroll === "function"
    && isRecord(value._inputHandler)
    && typeof value._inputHandler._eraseAttrData === "function"
}

function validRegion(value: unknown, rows: number): value is XtermScrollCore["_bufferService"]["buffer"] {
  return isRecord(value)
    && typeof value.scrollTop === "number" && Number.isInteger(value.scrollTop)
    && typeof value.scrollBottom === "number" && Number.isInteger(value.scrollBottom)
    && value.scrollTop >= 0 && value.scrollTop <= value.scrollBottom
    && value.scrollBottom < rows
}

/** Temporary compatibility for xterm InputHandler.scrollUp's own FIXME:
 * top-origin SU should retain outgoing rows in scrollback. The daemon's
 * Ghostty does so; dropping these rows only in the live viewer makes attach
 * history disagree. Remove this addon when upstream implements that behavior.
 *
 * VT semantics, independent of provider: inset regions and alternate buffers
 * retain stock behavior. xterm's existing scroll primitive owns cells, erase
 * attributes, footer placement, dirty ranges, capacity and the reading anchor.
 * No synthesized terminal bytes, input, provider metadata or history copy. */
export class TerminalScrollbackCompatibilityAddon implements ITerminalAddon {
  private registration: IDisposable | undefined
  private disabled = false

  private disable(): void {
    if (this.disabled) return
    this.disabled = true
    console.warn("[terminal] xterm scrollback compatibility disabled: unsupported internal state")
  }

  activate(term: Terminal): void {
    try {
      const core: unknown = (term as unknown as { _core?: unknown })._core
      if (!hasScrollCore(core) || !validRegion(core._bufferService.buffer, term.rows)) {
        this.disable()
        return
      }
      this.registration = term.parser.registerCsiHandler({ final: "S" }, (params) => {
        if (this.disabled) return false
        let mutationStarted = false
        try {
          if (term.buffer.active.type !== "normal") return false
          // The active buffer/margins may change on RIS, resize or DECSTBM.
          const region = core._bufferService.buffer
          if (!validRegion(region, term.rows)) {
            this.disable()
            return false
          }
          if (region.scrollTop !== 0 || params.some(Array.isArray)) return false
          const requested = params[0] ?? 1
          if (typeof requested !== "number" || !Number.isInteger(requested) || requested < 0) return false
          const count = Math.min(requested || 1, region.scrollBottom + 1)
          const erase = core._inputHandler._eraseAttrData()
          for (let row = 0; row < count; row++) {
            mutationStarted = true
            core._bufferService.scroll(erase)
          }
          return true
        } catch {
          this.disable()
          // Never throw into the parser/write queue. If a primitive failed
          // after mutation began, stock SU would scroll the same rows again;
          // consume this command and disable the shim rather than replay it.
          return mutationStarted
        }
      })
    } catch {
      this.disable()
    }
  }

  dispose(): void {
    this.registration?.dispose()
    this.registration = undefined
  }
}
