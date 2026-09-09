/**
 * How a shortcut is spelled on this platform, for dispatch and for display.
 *
 * The shortcut table is authored in macOS terms — `meta` is ⌘, and `display`
 * is the glyph string. On Linux both halves were simply wrong: nothing has a
 * Command key, so every ⌘ binding was unreachable by a person, while the hints
 * still said "⌘I". Dispatch and display have to move together or the app tells
 * you to press something that does nothing, which is why one module answers
 * both questions.
 *
 * The Linux mapping is not a glyph substitution. A terminal owns plain
 * `Ctrl+<letter>`: `Ctrl+C`, `Ctrl+D`, `Ctrl+Z`, `Ctrl+W`, and the rest of
 * readline. Mapping ⌘ to Ctrl the obvious way would have taken all of them
 * away from every agent session in the app. So:
 *
 * - `⌘X`  becomes `Ctrl+Shift+X` — the GNOME Terminal convention for exactly
 *   this reason.
 * - `⇧⌘X` becomes `Ctrl+Alt+X`, because its `Ctrl+Shift` form is taken by the
 *   unshifted binding above.
 * - A shortcut that was already Ctrl-only on macOS (`⌃-`, `⌃⇧-`) is unchanged:
 *   punctuation, not readline.
 * - Anything the desktop environment has already claimed gets a named
 *   exception below rather than a silent collision.
 */
export type ShortcutPlatform = "mac" | "linux"

export interface ShortcutModifiers {
  meta?: boolean
  shift?: boolean
  alt?: boolean
  ctrl?: boolean
}

export interface ShortcutBinding extends ShortcutModifiers {
  /** Keys matched against `KeyboardEvent.key`. */
  key: string | string[]
  /** Physical codes, for keys whose `key` a modifier rewrites. */
  code?: string | string[]
  /** What the hints say. */
  display: string
}

export function resolveShortcutPlatform(
  platform: string | undefined = typeof navigator === "undefined" ? undefined : navigator.platform,
): ShortcutPlatform {
  return /mac|iphone|ipad/i.test(platform ?? "") ? "mac" : "linux"
}

const MODIFIER_LABELS: Record<ShortcutPlatform, { ctrl: string; shift: string; alt: string; meta: string }> = {
  mac: { ctrl: "⌃", shift: "⇧", alt: "⌥", meta: "⌘" },
  linux: { ctrl: "Ctrl", shift: "Shift", alt: "Alt", meta: "Super" },
}

/** Modifier tokens a display string can start with, for splitting hints into keys. */
export function shortcutModifierTokens(platform: ShortcutPlatform): string[] {
  const labels = MODIFIER_LABELS[platform]
  return [labels.shift, labels.ctrl, labels.alt, labels.meta]
}

/**
 * US-layout shifted punctuation, so a hint can name the key you press rather
 * than the character Shift produces. `["_", "-"]` is one key, and "Ctrl+Shift+-"
 * is what a person can act on; "Ctrl+Shift+_" is not.
 */
const UNSHIFTED_PUNCTUATION: Record<string, string> = {
  _: "-",
  "{": "[",
  "}": "]",
  ":": ";",
  '"': "'",
  "<": ",",
  ">": ".",
  "?": "/",
  "|": "\\",
  "+": "=",
  "~": "`",
}

/** The key a hint should name, out of everything the binding matches. */
function preferredKey(keys: string[]): string {
  const letter = keys.find((key) => /^[A-Za-z]$/.test(key))
  if (letter) return letter
  const unshifted = keys.filter((key) => !(key in UNSHIFTED_PUNCTUATION && keys.includes(UNSHIFTED_PUNCTUATION[key]!)))
  return unshifted[0] ?? keys[0] ?? ""
}

/** The key name a hint shows, given the `KeyboardEvent.key` values it matches. */
function displayKey(key: string | string[], platform: ShortcutPlatform): string {
  const first = Array.isArray(key) ? preferredKey(key) : key
  const named: Record<string, string> = {
    ArrowUp: "↑",
    ArrowDown: "↓",
    ArrowLeft: "←",
    ArrowRight: "→",
    Backspace: platform === "mac" ? "⌫" : "Backspace",
    Escape: "Escape",
  }
  return named[first] ?? (first.length === 1 ? first.toUpperCase() : first)
}

function renderDisplay(binding: ShortcutModifiers & { key: string | string[] }, platform: ShortcutPlatform): string {
  const labels = MODIFIER_LABELS[platform]
  const parts: string[] = []
  // macOS orders modifiers ⌃⌥⇧⌘; Linux writes them Ctrl+Alt+Shift+Super.
  if (binding.ctrl) parts.push(labels.ctrl)
  if (binding.alt) parts.push(labels.alt)
  if (binding.shift) parts.push(labels.shift)
  if (binding.meta) parts.push(labels.meta)
  parts.push(displayKey(binding.key, platform))
  return platform === "mac" ? parts.join("") : parts.join("+")
}

/**
 * Linux bindings that the systematic mapping would put somewhere already
 * taken, or somewhere it should not go. Keyed by action name.
 */
const LINUX_EXCEPTIONS: Record<string, ShortcutModifiers> = {
  // Ctrl+Alt+↑/↓ switches GNOME workspaces, and an app cannot win that fight.
  // That rules the chord out for both arrow pairs, not only the ⌥⌘ one, so
  // repo navigation takes the Ctrl+Shift arrows the ⌘ tier never used.
  navigateUp: { alt: true },
  navigateDown: { alt: true },
  navigateRepoUp: { ctrl: true, shift: true },
  navigateRepoDown: { ctrl: true, shift: true },
  // ⌥⌘P would land on Ctrl+Alt+P, which ⇧⌘P (the command palette) already has.
  toggleFilePreview: { ctrl: true, alt: true, shift: true },
  // Ctrl+Alt+Backspace is the historical "kill the X server" chord. Disabled by
  // default on modern systems, but not something to bind an app action to.
  // Nothing else claims Ctrl+Shift+Backspace, because no ⌘⌫ shortcut exists.
  closeTask: { ctrl: true, shift: true },
}

/** The modifiers this action actually dispatches on, for this platform. */
export function platformModifiers(
  action: string,
  authored: ShortcutModifiers,
  platform: ShortcutPlatform,
): ShortcutModifiers {
  if (platform === "mac") {
    return {
      meta: authored.meta ?? false,
      shift: authored.shift ?? false,
      alt: authored.alt ?? false,
      ctrl: authored.ctrl ?? false,
    }
  }
  const exception = LINUX_EXCEPTIONS[action]
  if (exception) {
    return {
      meta: exception.meta ?? false,
      shift: exception.shift ?? false,
      alt: exception.alt ?? false,
      ctrl: exception.ctrl ?? false,
    }
  }
  if (!authored.meta) {
    // Already a Ctrl-only or bare binding; nothing to remap.
    return {
      meta: false,
      shift: authored.shift ?? false,
      alt: authored.alt ?? false,
      ctrl: authored.ctrl ?? false,
    }
  }
  return {
    meta: false,
    ctrl: true,
    shift: !authored.shift,
    alt: Boolean(authored.shift),
  }
}

/** The full binding — modifiers and hint — for this platform. */
export function platformBinding(
  action: string,
  authored: ShortcutModifiers & { key: string | string[]; code?: string | string[]; display: string },
  platform: ShortcutPlatform,
): ShortcutBinding {
  const modifiers = platformModifiers(action, authored, platform)
  return {
    ...modifiers,
    key: authored.key,
    code: authored.code,
    // macOS keeps its authored glyph string byte for byte; every existing
    // expectation is written against it.
    display: platform === "mac" ? authored.display : renderDisplay({ ...modifiers, key: authored.key }, platform),
  }
}

/**
 * The clipboard chord a terminal should act on, or `null` for a key that
 * belongs to the PTY.
 *
 * macOS can use ⌘C/⌘V because the terminal has no claim on the Command key.
 * Linux cannot: plain `Ctrl+C` is SIGINT and plain `Ctrl+V` is readline's
 * quoted-insert, so taking either would break the agent sessions the app
 * exists to run. `Ctrl+Shift+C` / `Ctrl+Shift+V` is what GNOME Terminal, and
 * every other Linux terminal, settled on for the same reason.
 */
export function terminalClipboardAction(
  event: Pick<KeyboardEvent, "key" | "metaKey" | "ctrlKey" | "shiftKey" | "altKey">,
  platform: ShortcutPlatform,
): "copy" | "paste" | null {
  const key = event.key.toLowerCase()
  if (key !== "c" && key !== "v") return null
  const wanted =
    platform === "mac"
      ? event.metaKey && !event.altKey && !event.ctrlKey
      : event.ctrlKey && event.shiftKey && !event.altKey && !event.metaKey
  if (!wanted) return null
  return key === "c" ? "copy" : "paste"
}

/**
 * A hint for a binding a component dispatches on `metaKey || ctrlKey` itself —
 * an in-view find, say. Those already work on Linux; only their labels said ⌘.
 */
export function metaOrControlHint(key: string, platform: ShortcutPlatform = resolveShortcutPlatform()): string {
  return platform === "mac" ? `⌘${key.toUpperCase()}` : `Ctrl+${key.toUpperCase()}`
}
