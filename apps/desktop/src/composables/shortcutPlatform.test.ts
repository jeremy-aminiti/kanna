import { describe, expect, it } from "vitest"
import { bindingsFor, shortcuts } from "./useKeyboardShortcuts"
import {
  metaOrControlHint,
  resolveShortcutPlatform,
  shortcutModifierTokens,
  terminalClipboardAction,
} from "./shortcutPlatform"

const mac = bindingsFor("mac")
const linux = bindingsFor("linux")

function chord(binding: { ctrl?: boolean; shift?: boolean; alt?: boolean; meta?: boolean }): string {
  return [
    binding.ctrl ? "Ctrl" : "",
    binding.alt ? "Alt" : "",
    binding.shift ? "Shift" : "",
    binding.meta ? "Super" : "",
  ]
    .filter(Boolean)
    .join("+")
}

describe("resolveShortcutPlatform", () => {
  it("reads a Mac as a Mac and everything else as Linux", () => {
    expect(resolveShortcutPlatform("MacIntel")).toBe("mac")
    expect(resolveShortcutPlatform("iPhone")).toBe("mac")
    expect(resolveShortcutPlatform("Linux aarch64")).toBe("linux")
    expect(resolveShortcutPlatform("")).toBe("linux")
    // Omitting the argument reads `navigator.platform`, which this suite
    // declares as a Mac in its setup file.
    expect(resolveShortcutPlatform()).toBe("mac")
  })
})

describe("macOS bindings", () => {
  it("dispatches and displays exactly what the table authored", () => {
    for (const def of shortcuts) {
      const binding = mac.get(def.action)
      expect(binding, def.action).toBeDefined()
      expect(binding!.display, def.action).toBe(def.display)
      expect(binding!.meta ?? false, def.action).toBe(def.meta ?? false)
      expect(binding!.shift ?? false, def.action).toBe(def.shift ?? false)
      expect(binding!.alt ?? false, def.action).toBe(def.alt ?? false)
      expect(binding!.ctrl ?? false, def.action).toBe(def.ctrl ?? false)
    }
  })
})

describe("Linux bindings", () => {
  it("never asks for a Command key nobody has", () => {
    for (const [action, binding] of linux) {
      expect(binding.meta ?? false, action).toBe(false)
      expect(binding.display, action).not.toContain("⌘")
    }
  })

  it("never takes a plain Ctrl+letter, because the terminal owns those", () => {
    // Ctrl+C, Ctrl+D, Ctrl+Z, Ctrl+W and the rest of readline belong to the
    // agent session running inside the app, not to the app's chrome.
    for (const [action, binding] of linux) {
      const keys = Array.isArray(binding.key) ? binding.key : [binding.key]
      const isSingleLetter = keys.some((k) => /^[A-Za-z]$/.test(k))
      if (!isSingleLetter) continue
      const plainCtrl = binding.ctrl && !binding.shift && !binding.alt
      expect(plainCtrl, `${action} would steal Ctrl+${keys[0]?.toUpperCase()} from the terminal`).toBe(false)
    }
  })

  it("binds no two actions to the same chord", () => {
    const seen = new Map<string, string>()
    for (const [action, binding] of linux) {
      const keys = Array.isArray(binding.key) ? binding.key : [binding.key]
      // `["I", "i"]` is one binding written twice, not two.
      for (const id of new Set(keys.map((key) => `${chord(binding)}+${key.toLowerCase()}`))) {
        const previous = seen.get(id)
        expect(previous, `${action} collides with ${previous} on ${id}`).toBeUndefined()
        seen.set(id, action)
      }
    }
  })

  it("stays off the chords GNOME has already claimed", () => {
    // Ctrl+Alt+Arrow switches workspaces; an app cannot win that.
    for (const [action, binding] of linux) {
      const keys = Array.isArray(binding.key) ? binding.key : [binding.key]
      const isArrow = keys.some((k) => k.startsWith("Arrow"))
      if (!isArrow) continue
      expect(binding.ctrl && binding.alt, `${action} is on a GNOME workspace chord`).toBeFalsy()
    }
    // Ctrl+Alt+Backspace is the historical "kill the X server" chord.
    for (const [action, binding] of linux) {
      const keys = Array.isArray(binding.key) ? binding.key : [binding.key]
      if (!keys.includes("Backspace")) continue
      expect(binding.ctrl && binding.alt, `${action} is on the X-server-zap chord`).toBeFalsy()
    }
  })

  it("moves an unshifted Command binding to Ctrl+Shift and a shifted one to Ctrl+Alt", () => {
    expect(linux.get("createRepo")).toMatchObject({ ctrl: true, shift: true, alt: false, display: "Ctrl+Shift+I" })
    expect(linux.get("importRepo")).toMatchObject({ ctrl: true, shift: false, alt: true, display: "Ctrl+Alt+I" })
    expect(linux.get("closeTabOrWindow")).toMatchObject({ ctrl: true, shift: true, display: "Ctrl+Shift+W" })
    expect(linux.get("closeWindow")).toMatchObject({ ctrl: true, alt: true, display: "Ctrl+Alt+W" })
  })

  it("leaves a binding that was already Ctrl-only alone", () => {
    expect(linux.get("goBack")).toMatchObject({ ctrl: true, shift: false, alt: false, meta: false })
    expect(linux.get("goForward")).toMatchObject({ ctrl: true, shift: true, alt: false, meta: false })
  })

  it("keeps Escape bare", () => {
    expect(linux.get("dismiss")).toMatchObject({ ctrl: false, shift: false, alt: false, meta: false, display: "Escape" })
  })

  it("spells arrows and Backspace so a hint reads as a key", () => {
    expect(linux.get("navigateUp")?.display).toBe("Alt+↑")
    expect(linux.get("navigateRepoDown")?.display).toBe("Ctrl+Shift+↓")
    expect(linux.get("closeTask")?.display).toBe("Ctrl+Shift+Backspace")
  })
})

describe("shortcutModifierTokens", () => {
  it("gives each platform the vocabulary its hints are written in", () => {
    expect(shortcutModifierTokens("mac")).toEqual(["⇧", "⌃", "⌥", "⌘"])
    expect(shortcutModifierTokens("linux")).toEqual(["Shift", "Ctrl", "Alt", "Super"])
  })
})

describe("terminalClipboardAction", () => {
  const key = (over: Partial<KeyboardEvent>) =>
    ({ key: "c", metaKey: false, ctrlKey: false, shiftKey: false, altKey: false, ...over }) as KeyboardEvent

  it("acts on the Command chord on macOS", () => {
    expect(terminalClipboardAction(key({ key: "c", metaKey: true }), "mac")).toBe("copy")
    expect(terminalClipboardAction(key({ key: "v", metaKey: true }), "mac")).toBe("paste")
  })

  it("leaves plain Ctrl+C to the PTY on Linux, because it is SIGINT", () => {
    expect(terminalClipboardAction(key({ key: "c", ctrlKey: true }), "linux")).toBeNull()
    // Ctrl+V is readline's quoted-insert; also not ours.
    expect(terminalClipboardAction(key({ key: "v", ctrlKey: true }), "linux")).toBeNull()
  })

  it("acts on Ctrl+Shift+C/V on Linux", () => {
    expect(terminalClipboardAction(key({ key: "C", ctrlKey: true, shiftKey: true }), "linux")).toBe("copy")
    expect(terminalClipboardAction(key({ key: "V", ctrlKey: true, shiftKey: true }), "linux")).toBe("paste")
  })

  it("does not claim the Command chord on Linux, where it is the Super key", () => {
    expect(terminalClipboardAction(key({ key: "c", metaKey: true }), "linux")).toBeNull()
  })

  it("ignores every other key", () => {
    expect(terminalClipboardAction(key({ key: "d", metaKey: true }), "mac")).toBeNull()
    expect(terminalClipboardAction(key({ key: "c", metaKey: true, altKey: true }), "mac")).toBeNull()
  })
})

describe("hints for keys a modifier rewrites", () => {
  it("names the key you press, not the character Shift makes", () => {
    // `["_", "-"]` is one key; "Ctrl+Shift+_" is not something to press.
    expect(linux.get("goForward")?.display).toBe("Ctrl+Shift+-")
    expect(linux.get("prevTab")?.display).toBe("Ctrl+Alt+[")
    expect(linux.get("nextTab")?.display).toBe("Ctrl+Alt+]")
  })
})

describe("metaOrControlHint", () => {
  it("labels a binding the view already dispatches on either modifier", () => {
    expect(metaOrControlHint("f", "mac")).toBe("⌘F")
    expect(metaOrControlHint("f", "linux")).toBe("Ctrl+F")
  })
})
