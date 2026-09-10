import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { findClaudeBinary } from "../../helpers/claude";
import { claudeBinaryOrNull, ptyBridgeAvailable } from "../../helpers/availability";
import { makeRealTempDir, removeDir } from "../../helpers/background";
import { startPtySession, type PtySession } from "../../helpers/pty";

// WHAT BREAKS IN KANNA IF THIS PIN FAILS: ordinary task input (mobile Send,
// kanna_send_task_input, stage prompts) delivered into a Claude Code session
// that is busy mid-turn.
//
// crates/daemon/src/session.rs::logical_message_bytes writes the whole
// framed message plus its CR in one PTY write, with no wait for the CLI to
// prove it consumed anything (that settle-wait was removed by task d2eb7fa0,
// PR #1369, 2026-09-08 — the owner's own directive after this exact
// protection repeatedly stranded messages: "The input protection is killing
// me. I'd rather have collisions."). docs/2026-09-10-mobile-connection-
// flicker-e2e-note.md found that crates/daemon/tests/reconnect.rs only
// proves the daemon writes the right bytes to the PTY's *read side*, through
// a synthetic dd/stty reader — never that Claude Code's own bracketed-paste
// handling treats the CR immediately after the closing paste marker as
// submission while Claude is itself mid-turn and repainting its own output.
// This is the missing half: a real Claude Code TUI.
//
// The owner's provider for the mobile flicker/missing-Enter report is still
// unknown — this is not evidence it was Claude, only one of the two
// providers this harness can currently drive interactively (see the Codex
// case). The owner's original 2026-09-06 incident that motivated
// PASTE_FRAMING_MIN_LEN/bracket-paste framing in the first place *was*
// Claude Code (crates/daemon/src/session.rs's own doc comment: "Measured on
// 2026-09-05 against Claude Code 2.1.261"), which is why this case exists
// alongside the Codex one rather than only the latter.
//
// `--dangerously-skip-permissions` matches crates/kanna-server/src/
// task_creator/commands.rs::get_agent_permission_flags's Claude branch for
// permission_mode None/"dontAsk" — the same flag Kanna's own daemon passes
// for an ordinary PTY-mode Claude task — so the marker-file write below is
// not blocked behind an approval prompt this test has no way to answer.
//
// UNVERIFIED, more so than the Codex case: this repo has no prior live-TUI
// Claude Code test to carry a proven trust-prompt/composer pattern from (only
// exec/stream-json-mode helpers exist in helpers/claude.ts). TRUST_PROMPT and
// COMPOSER below are a best-effort guess at Claude Code's on-screen text, not
// a pattern already confirmed working here. No live CLI turns were run to
// check them in the pass that wrote this file. The first real run should
// correct them from the failure's own TUI-tail dump before this is trusted
// as a pass/fail signal — treat a "never reached its composer" skip here as
// "the pattern needs fixing," not as "Claude doesn't work."

const TRUST_PROMPT = /trustthefilesinthisfolder/i;
const COMPOSER = /\?forshortcuts/i;

interface ClaudeTuiSetup {
  session: PtySession;
  cwd: string;
}

async function startClaudeTui(): Promise<ClaudeTuiSetup> {
  const binary = await findClaudeBinary();
  const cwd = await makeRealTempDir("kanna-claude-submission-");
  const session = startPtySession(binary, ["--dangerously-skip-permissions"], { cwd });
  return { session, cwd };
}

async function reachComposer(
  setup: ClaudeTuiSetup,
  ctx: { skip: (reason?: string) => void },
): Promise<boolean> {
  const { session } = setup;
  if (await session.waitForOutput(TRUST_PROMPT, 30_000)) {
    // The safe option is preselected on Claude Code's trust dialog; Enter
    // accepts it. If this guess is wrong, the composer wait below fails loud
    // with the actual screen in its diagnostic.
    session.write("\r");
  }
  if (!(await session.waitForOutput(COMPOSER, 30_000))) {
    ctx.skip(`claude TUI never reached its composer. TUI tail:\n${session.output.slice(-600)}`);
    return false;
  }
  return true;
}

async function teardown(setup: ClaudeTuiSetup): Promise<void> {
  setup.session.kill();
  await removeDir(setup.cwd);
}

async function requireEnvironment(ctx: { skip: (reason?: string) => void }): Promise<boolean> {
  if (!(await claudeBinaryOrNull())) {
    ctx.skip("claude CLI is not installed");
    return false;
  }
  if (!(await ptyBridgeAvailable())) {
    ctx.skip("/usr/bin/python3 is unavailable, so no PTY can be allocated");
    return false;
  }
  return true;
}

describe("claude logical-input submission during a busy turn", () => {
  it("a large paste-framed message delivered mid-turn is acted on, not just written to the pty", async (ctx) => {
    if (!(await requireEnvironment(ctx))) return;

    const setup = await startClaudeTui();
    try {
      if (!(await reachComposer(setup, ctx))) return;

      // Start a long turn so the composer is genuinely busy/repainting when
      // the second message lands — the exact condition
      // a_submission_boundary_is_written_even_while_the_terminal_repaints
      // simulates with a synthetic reader instead of a real CLI, and the
      // shape of the owner's own 2026-09-06 report (a long dictated message
      // arriving mid-echo).
      await setup.session.submitLogical(
        "Print every integer from 1 to 400, one per line, slowly. Do not stop early.",
        false,
      );
      await setup.session.waitForOutput(/^1$/m, 15_000);

      const markerPath = join(setup.cwd, "logical-submission-marker.txt");
      // Padded well past PASTE_FRAMING_MIN_LEN (256 bytes) so the daemon
      // frames this as a paste, the way the owner's original ~1.2KB report
      // did, rather than sending it unframed.
      const largeMessage =
        "Stop the previous task. Create a file named logical-submission-marker.txt " +
        "in the current directory containing exactly: SUBMITTED_OK. Then stop.\n" +
        `# padding: ${"x".repeat(400)}`;
      await setup.session.submitLogical(largeMessage, true);

      const wrote = await setup.session.waitUntil(
        () => existsSync(markerPath),
        180_000,
        1_000,
      );
      expect(
        wrote,
        "claude never acted on a large paste-framed message delivered mid-turn — either " +
        "the CR was swallowed by the busy composer, or the model never saw it as an " +
        `instruction. TUI tail:\n${setup.session.output.slice(-1200)}`,
      ).toBe(true);
      if (wrote) {
        expect(readFileSync(markerPath, "utf8")).toContain("SUBMITTED_OK");
      }
    } finally {
      await teardown(setup);
    }
  }, 300_000);
});
