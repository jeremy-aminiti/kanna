import { describe, expect, it } from "vitest";
import {
  logicalMessageBytes,
  PASTE_FRAMING_MIN_LEN,
} from "../../helpers/pty";

// Mirrors crates/daemon/src/session.rs::logical_message_bytes and
// crates/kanna-server/src/http_api/task_input.rs::task_input_message. Kept
// in lockstep by hand, not by shared source, so a future change to either
// Rust function should update this file's expectations too — see
// docs/2026-09-10-mobile-connection-flicker-e2e-note.md for why this exists:
// the live PtySession.submit() helper encodes a policy the daemon no longer
// implements, and these are the byte-level assertions the live reproduction
// (tests/live/claude-logical-submission.test.ts,
// tests/live/codex-logical-submission.test.ts) builds its input from.
describe("logicalMessageBytes", () => {
  it("submits an empty message as a bare CR", () => {
    expect(logicalMessageBytes("", true)).toBe("\r");
    expect(logicalMessageBytes("", false)).toBe("\r");
  });

  it("leaves a short single-line message unframed even with paste mode advertised", () => {
    expect(logicalMessageBytes("owner reply", true)).toBe("owner reply\r");
    expect(logicalMessageBytes("owner reply", false)).toBe("owner reply\r");
  });

  it("never frames when the terminal has not advertised bracketed paste, regardless of size", () => {
    const long = "x".repeat(PASTE_FRAMING_MIN_LEN * 2);
    expect(logicalMessageBytes(long, false)).toBe(`${long}\r`);
  });

  it("frames a message at or above the threshold when paste mode is advertised", () => {
    const atThreshold = "a".repeat(PASTE_FRAMING_MIN_LEN);
    expect(logicalMessageBytes(atThreshold, true)).toBe(
      `\x1b[200~${atThreshold}\x1b[201~\r`,
    );

    const belowThreshold = "b".repeat(PASTE_FRAMING_MIN_LEN - 1);
    expect(logicalMessageBytes(belowThreshold, true)).toBe(`${belowThreshold}\r`);
  });

  it("frames any message carrying a newline, even a short one, when paste mode is advertised", () => {
    expect(logicalMessageBytes("commit instructions\n\nDo it now", true)).toBe(
      "\x1b[200~commit instructions\n\nDo it now\x1b[201~\r",
    );
  });

  it("does not frame a short single-line message just because it carries a newline, without paste mode", () => {
    expect(logicalMessageBytes("line one\nline two", false)).toBe(
      "line one\nline two\r",
    );
  });

  it("trims only a trailing newline before deciding, matching task_input_message", () => {
    // A trailing \n must not itself force framing, and must not survive into
    // the written buffer as a second submission boundary.
    expect(logicalMessageBytes("owner reply\n", true)).toBe("owner reply\r");
    expect(logicalMessageBytes("owner reply\r\n", true)).toBe("owner reply\r");
    // An embedded (non-trailing) newline is untouched and still forces framing.
    expect(logicalMessageBytes("line one\nline two\n", true)).toBe(
      "\x1b[200~line one\nline two\x1b[201~\r",
    );
  });

  it("measures the threshold in UTF-8 bytes, not JS string length", () => {
    // Each character below is a 3-byte UTF-8 sequence, so ~86 of them cross
    // the 256-byte threshold while staying under 256 JS characters.
    const short = "日".repeat(85); // 255 bytes, 85 chars
    const long = "日".repeat(86); // 258 bytes, 86 chars
    expect(logicalMessageBytes(short, true)).toBe(`${short}\r`);
    expect(logicalMessageBytes(long, true)).toBe(
      `\x1b[200~${long}\x1b[201~\r`,
    );
  });
});
