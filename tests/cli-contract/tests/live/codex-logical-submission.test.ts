import { constants } from "node:fs";
import { access, symlink } from "node:fs/promises";
import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { findCodexBinary } from "../../helpers/codex";
import { codexBinaryOrNull, ptyBridgeAvailable } from "../../helpers/availability";
import { makeRealTempDir, removeDir } from "../../helpers/background";
import { startPtySession, type PtySession } from "../../helpers/pty";

// WHAT BREAKS IN KANNA IF THIS PIN FAILS: ordinary task input (mobile Send,
// kanna_send_task_input, stage prompts) delivered into a Codex session that
// is busy mid-turn.
//
// crates/daemon/src/session.rs::logical_message_bytes writes the whole
// framed message plus its CR in one PTY write, with no wait for Codex to
// prove it consumed anything (that settle-wait was removed by task d2eb7fa0,
// PR #1369, 2026-09-08). docs/2026-09-10-mobile-connection-flicker-e2e-note.md
// found that crates/daemon/tests/reconnect.rs only proves the daemon writes
// the right bytes to the PTY's *read side*, through a synthetic dd/stty
// reader — never that Codex's own paste/composer handling treats the CR
// immediately after the closing paste marker as submission while Codex is
// itself mid-repaint. This is the missing half: a real Codex TUI.
//
// The owner's provider for the mobile flicker/missing-Enter report is still
// unknown — this is not evidence it was Codex, only one of the two providers
// this harness can currently drive interactively (see the Claude case).
//
// `--yolo` matches crates/kanna-server/src/task_creator/commands.rs::
// get_agent_permission_flags's Codex branch for permission_mode
// None/"dontAsk" — the same flag Kanna's own daemon passes for an ordinary
// PTY-mode Codex task — so the marker-file write below is not blocked behind
// an approval prompt this test has no way to answer.
//
// UNVERIFIED: this file has not been run against a real Codex TUI (no live
// CLI turns authorized in the pass that wrote it). TRUST_PROMPT and COMPOSER
// are carried from tests/live/codex-tui-quit.test.ts, which *has* been run
// and passed; the busy-turn marker-file scenario below is new. Its first
// real run should confirm or correct the timings and patterns here before
// this is trusted as a pass/fail signal, not just read as a plan.

const TRUST_PROMPT = /trustthecontentsofthisdirectory/i;
const COMPOSER = /\/modeltochange|Use\/skills/i;

interface CodexTuiSetup {
  session: PtySession;
  cwd: string;
  codexHome: string;
}

async function startCodexTui(): Promise<CodexTuiSetup> {
  const binary = await findCodexBinary();
  const cwd = await makeRealTempDir("kanna-codex-submission-");
  // An isolated CODEX_HOME keeps the directory-trust answer (and the rollout
  // this run writes) out of the developer's real ~/.codex.
  const codexHome = await makeRealTempDir("kanna-codex-home-");
  await symlink(join(homedir(), ".codex", "auth.json"), join(codexHome, "auth.json"));
  const session = startPtySession(binary, ["--yolo"], { cwd, env: { CODEX_HOME: codexHome } });
  return { session, cwd, codexHome };
}

async function reachComposer(
  setup: CodexTuiSetup,
  ctx: { skip: (reason?: string) => void },
): Promise<boolean> {
  const { session } = setup;
  if (await session.waitForOutput(TRUST_PROMPT, 30_000)) {
    // "1. Yes, continue" is preselected; Enter accepts it.
    session.write("\r");
  }
  if (!(await session.waitForOutput(COMPOSER, 30_000))) {
    ctx.skip(`codex TUI never reached its composer. TUI tail:\n${session.output.slice(-600)}`);
    return false;
  }
  return true;
}

async function teardown(setup: CodexTuiSetup): Promise<void> {
  setup.session.kill();
  await removeDir(setup.cwd);
  await removeDir(setup.codexHome);
}

async function requireEnvironment(ctx: { skip: (reason?: string) => void }): Promise<boolean> {
  if (!(await codexBinaryOrNull())) {
    ctx.skip("codex CLI is not installed");
    return false;
  }
  if (!(await ptyBridgeAvailable())) {
    ctx.skip("/usr/bin/python3 is unavailable, so no PTY can be allocated");
    return false;
  }
  try {
    await access(join(homedir(), ".codex", "auth.json"), constants.R_OK);
  } catch {
    ctx.skip("codex CLI is not authenticated");
    return false;
  }
  return true;
}

describe("codex logical-input submission during a busy turn", () => {
  it("a large paste-framed message delivered mid-turn is acted on, not just written to the pty", async (ctx) => {
    if (!(await requireEnvironment(ctx))) return;

    const setup = await startCodexTui();
    try {
      if (!(await reachComposer(setup, ctx))) return;

      // Start a long turn so the composer is genuinely busy/repainting when
      // the second message lands — the exact condition
      // a_submission_boundary_is_written_even_while_the_terminal_repaints
      // simulates with a synthetic reader instead of a real CLI.
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
        "codex never acted on a large paste-framed message delivered mid-turn — either " +
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
