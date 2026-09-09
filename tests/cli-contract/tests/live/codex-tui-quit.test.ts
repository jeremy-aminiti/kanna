import { constants } from "node:fs";
import { access, symlink } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { findCodexBinary } from "../../helpers/codex";
import { codexBinaryOrNull, ptyBridgeAvailable } from "../../helpers/availability";
import { makeRealTempDir, removeDir } from "../../helpers/background";
import { startPtySession, type PtySession } from "../../helpers/pty";

// WHAT BREAKS IN KANNA IF THIS PIN FAILS: transfer finalization sequencing for
// Codex tasks (Decision 3 step 3 — inject the provider quit command).
//
// The daemon submits the whole slash command and its terminating CR in one
// buffer. Codex's composer opens a command popup on `/`, so the question is
// whether a burst-written command survives that path or lands on the model as
// chat text. It survives and executes `/quit`, which is what lets
// finalization reuse the ordinary input helper for Codex instead of growing a
// provider-specific keystroke pacer.
//
// If this pin ever fails, injecting `/quit` sends the agent a chat message
// instead of quitting: finalization would hang waiting for an Exit that never
// comes, then fall through to the destructive teardown at the end of the
// degradation ladder.

// Written without whitespace: the TUI positions words with cursor escapes, so
// the text on screen strips down to "Doyoutrustthecontents…" (see
// PtySession.compactOutput).
const QUIT_POPUP = /\/quitexitCodex/i;
const TRUST_PROMPT = /trustthecontentsofthisdirectory/i;
// The composer's placeholder text rotates, so key on the banner instead.
const COMPOSER = /\/modeltochange|Use\/skills/i;

interface CodexTuiSetup {
  session: PtySession;
  cwd: string;
  codexHome: string;
}

async function startCodexTui(): Promise<CodexTuiSetup> {
  const binary = await findCodexBinary();
  const cwd = await makeRealTempDir("kanna-codex-tui-");
  // An isolated CODEX_HOME keeps the directory-trust answer (and the rollout
  // this run writes) out of the developer's real ~/.codex.
  const codexHome = await makeRealTempDir("kanna-codex-home-");
  await symlink(join(homedir(), ".codex", "auth.json"), join(codexHome, "auth.json"));
  const session = startPtySession(binary, [], { cwd, env: { CODEX_HOME: codexHome } });
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

describe("codex TUI quit command", () => {
  it("executes /quit submitted the way kanna-server submits task input", async (ctx) => {
    if (!(await requireEnvironment(ctx))) return;

    const setup = await startCodexTui();
    try {
      if (!(await reachComposer(setup, ctx))) return;

      await setup.session.submit("/quit");
      expect(
        await setup.session.waitForExit(20_000),
        `codex did not execute /quit from one logical-input buffer. Finalization would wait for an Exit that never ` +
        `arrives and fall through to destructive teardown. TUI tail:\n` +
        setup.session.output.slice(-800),
      ).toBe(0);
    } finally {
      await teardown(setup);
    }
  }, 180_000);

  it("keeps typed-out keystrokes working too", async (ctx) => {
    if (!(await requireEnvironment(ctx))) return;

    const setup = await startCodexTui();
    try {
      if (!(await reachComposer(setup, ctx))) return;

      // The same command a character at a time — what a person does, and the
      // fallback if the burst form ever stops being recognised.
      await setup.session.typePaced("/quit");
      expect(
        await setup.session.waitForOutput(QUIT_POPUP, 10_000),
        `codex did not offer /quit for paced keystrokes either. TUI tail:\n${setup.session.output.slice(-800)}`,
      ).toBe(true);

      setup.session.write("\r");
      expect(await setup.session.waitForExit(20_000)).toBe(0);
    } finally {
      await teardown(setup);
    }
  }, 180_000);
});
