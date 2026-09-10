import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

/**
 * Drive an agent CLI's interactive TUI on a real terminal.
 *
 * Kanna's daemon writes input bytes straight to a PTY master fd
 * (`Command::Input` — no per-source transformation), and `kanna-server`'s
 * submission policy is "write the message, wait 150 ms, then send CR as a
 * discrete keystroke" (`crates/kanna-server/src/http_api/task_input.rs`,
 * `LOGICAL_INPUT_SUBMIT_DELAY_MS`). {@link PtySession.submit} reproduces that exactly,
 * so a test that passes here is evidence about the real injection path rather
 * than about an approximation of it.
 *
 * Node has no built-in PTY and the live suite must stay free of native
 * dependencies, so the terminal comes from `helpers/pty-bridge.py` (system
 * python3 on macOS). The bridge is a byte pipe and nothing more.
 */

const HERE = dirname(fileURLToPath(import.meta.url));
const BRIDGE = join(HERE, "pty-bridge.py");

/** Mirrors LOGICAL_INPUT_SUBMIT_DELAY_MS in crates/daemon/src/session.rs. */
export const SUBMIT_ENTER_DELAY_MS = 150;

/**
 * {@link PtySession.submit}'s "write text, wait, then write CR separately"
 * policy is what the daemon's logical-input writer did before task d2eb7fa0
 * (PR #1369, "Always submit delivered task input; remove the draft-protection
 * hold", 2026-09-08) removed the settle-wait fence entirely. It no longer
 * matches what kanna-server puts on the wire and must not be read as
 * evidence about current submission behavior — see
 * docs/2026-09-10-mobile-connection-flicker-e2e-note.md. {@link submit} is
 * left as-is for whatever it currently happens to cover; new coverage of
 * real submission behavior should use {@link logicalMessageBytes} /
 * {@link PtySession.submitLogical} below instead.
 */

const BRACKETED_PASTE_BEGIN = "\x1b[200~";
const BRACKETED_PASTE_END = "\x1b[201~";

/** Mirrors PASTE_FRAMING_MIN_LEN in crates/daemon/src/session.rs. */
export const PASTE_FRAMING_MIN_LEN = 256;

/**
 * Mirrors `crates/daemon/src/session.rs::logical_message_bytes` (plus the
 * trailing-newline trim `crates/kanna-server/src/http_api/task_input.rs::
 * task_input_message` applies before handing the daemon anything) exactly:
 * one PTY write containing the text — bracket-paste-framed when the
 * terminal supports it and the trimmed message is >=256 bytes or carries a
 * newline — with its `\r` submission boundary appended in the same buffer.
 * No wait, no separate write: this is what a real logical-input delivery
 * (kanna_send_task_input, the mobile composer's ordinary Send, a stage
 * prompt) puts on the wire today, byte for byte.
 */
export function logicalMessageBytes(text: string, bracketedPasteMode: boolean): string {
  const trimmed = text.replace(/[\r\n]+$/, "");
  if (trimmed.length === 0) {
    return "\r";
  }
  const hasNewline = /[\r\n]/.test(trimmed);
  const byteLength = Buffer.byteLength(trimmed, "utf8");
  if (!bracketedPasteMode || (!hasNewline && byteLength < PASTE_FRAMING_MIN_LEN)) {
    return `${trimmed}\r`;
  }
  return `${BRACKETED_PASTE_BEGIN}${trimmed}${BRACKETED_PASTE_END}\r`;
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Strip ANSI/OSC noise so TUI output can be pattern-matched. */
export function stripAnsi(raw: string): string {
  return raw
    .replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, "")
    .replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, "")
    .replace(/\x1b[()][A-Z0-9]/g, "")
    .replace(/\x1b[=>]/g, "")
    .replace(/\r/g, "\n");
}

export class PtySession {
  private readonly child: ChildProcessWithoutNullStreams;
  private raw = "";
  private closed = false;
  private exitCodeValue: number | null = null;

  constructor(command: string, args: string[], opts: { cwd: string; env?: Record<string, string> }) {
    this.child = spawn("/usr/bin/python3", [BRIDGE, command, ...args], {
      cwd: opts.cwd,
      env: { ...process.env, TERM: "xterm-256color", ...opts.env },
      stdio: ["pipe", "pipe", "pipe"],
    }) as ChildProcessWithoutNullStreams;

    this.child.stdout.setEncoding("utf8");
    this.child.stderr.setEncoding("utf8");
    this.child.stdout.on("data", (chunk: string) => {
      this.raw += chunk;
    });
    this.child.stderr.on("data", (chunk: string) => {
      this.raw += chunk;
    });
    this.child.on("close", (code) => {
      this.closed = true;
      this.exitCodeValue = code ?? -1;
    });
    // The agent can quit between a write being decided on and being flushed;
    // that is a normal race here, not a test failure.
    this.child.stdin.on("error", () => undefined);
  }

  get output(): string {
    return stripAnsi(this.raw);
  }

  /**
   * Whether this PTY session's own output has, at any point, sent DECSET
   * 2004h (`\x1b[?2004h`) — the terminal-capability announcement that
   * enables bracketed paste. Mirrors exactly what the daemon itself keys
   * framing on: `crates/daemon/src/session.rs`'s headless terminal tracks
   * this same sequence and `Session::bracketed_paste_mode()` reads it back
   * before `logical_message_bytes` decides whether to frame. A test must
   * observe this from the real session before choosing `bracketedPasteMode`
   * for {@link submitLogical} — asserting `true` unconditionally would be
   * testing a framing decision the daemon itself would never have made for
   * a CLI that never advertised the mode.
   */
  sawBracketedPasteEnable(): boolean {
    return this.raw.includes("\x1b[?2004h");
  }

  /**
   * {@link output} with all whitespace removed. TUIs place each word with
   * cursor-movement escapes rather than spaces, so "Do you trust the contents"
   * strips down to "Doyoutrustthecontents" — matching against this form is the
   * only reliable way to find a phrase on the screen.
   */
  get compactOutput(): string {
    return this.output.replace(/\s+/g, "");
  }

  get exited(): boolean {
    return this.closed;
  }

  get exitCode(): number | null {
    return this.closed ? this.exitCodeValue : null;
  }

  /** Write bytes to the PTY master, unmodified — the daemon's `Command::Input`. */
  write(bytes: string): void {
    if (this.closed) return;
    this.child.stdin.write(bytes);
  }

  /** Write text, pause, then send CR — `try_submit_task_input`'s exact policy. */
  async submit(text: string): Promise<void> {
    if (text.length > 0) {
      this.write(text.replace(/[\r\n]+$/, ""));
      await sleep(SUBMIT_ENTER_DELAY_MS);
    }
    this.write("\r");
  }

  /**
   * Deliver a logical message the way the daemon actually does today: one
   * write, framed per {@link logicalMessageBytes}, no intervening wait. Use
   * this — not {@link submit} — to test whether a real CLI's own input
   * parser treats the trailing CR as submission; `bracketedPasteMode` should
   * match whatever the target CLI actually advertises (the daemon frames a
   * message only when the terminal itself has advertised the mode).
   */
  async submitLogical(text: string, bracketedPasteMode: boolean): Promise<void> {
    this.write(logicalMessageBytes(text, bracketedPasteMode));
  }

  /** Write text one character at a time, the way a person types it. */
  async typePaced(text: string, perCharMs = 80): Promise<void> {
    for (const char of text) {
      this.write(char);
      await sleep(perCharMs);
    }
  }

  /**
   * Wait for `pattern` to appear on screen. Matched against both the stripped
   * output and {@link compactOutput}, so patterns should be written without
   * whitespace to survive cursor-positioned text.
   */
  async waitForOutput(pattern: string | RegExp, timeoutMs: number): Promise<boolean> {
    const matches = (): boolean => {
      const candidates = [this.output, this.compactOutput];
      return candidates.some((text) =>
        typeof pattern === "string" ? text.includes(pattern) : pattern.test(text),
      );
    };
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (matches()) return true;
      if (this.closed) return matches();
      await sleep(200);
    }
    return matches();
  }

  async waitUntil(predicate: () => boolean, timeoutMs: number, pollMs = 250): Promise<boolean> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (predicate()) return true;
      await sleep(pollMs);
    }
    return predicate();
  }

  /** Resolves to the exit code, or null if the process was still alive. */
  async waitForExit(timeoutMs: number): Promise<number | null> {
    await this.waitUntil(() => this.closed, timeoutMs);
    return this.exitCode;
  }

  kill(): void {
    if (!this.closed) this.child.kill("SIGKILL");
  }
}

export function startPtySession(
  command: string,
  args: string[],
  opts: { cwd: string; env?: Record<string, string> },
): PtySession {
  return new PtySession(command, args, opts);
}
