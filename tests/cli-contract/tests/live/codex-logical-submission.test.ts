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
// SCOPE, EXPLICIT: this test constructs the exact bytes
// `logical_message_bytes` would write (via `submitLogical`) and drives a
// real Codex TUI directly. It does not exercise the mobile app, the
// server's `/v1/tasks/{id}/input` route, or the daemon's own write path —
// only the boundary `reconnect.rs`'s synthetic readers cannot reach: whether
// a real CLI's own parser treats those exact bytes as submission. A pass
// here says nothing about whether the mobile → server → daemon pipeline
// delivers those same bytes correctly; that is a separate, already-covered
// question.
//
// The owner's provider for the mobile flicker/missing-Enter report is still
// unknown — this is not evidence it was Codex, only one of the two providers
// this harness can currently drive interactively (see the Claude case,
// authored but not run this pass).
//
// `--yolo` matches crates/kanna-server/src/task_creator/commands.rs::
// get_agent_permission_flags's Codex branch for permission_mode
// None/"dontAsk" — the same flag Kanna's own daemon passes for an ordinary
// PTY-mode Codex task — so the marker-file write below is not blocked behind
// an approval prompt this test has no way to answer. `-m gpt-5.6-sol -c
// model_reasoning_effort="low"` pins an explicit, economical model/effort
// pair for this fixture rather than leaving it to whatever `codex` defaults
// to locally — flag syntax confirmed against
// tests/live/codex-model-ids.test.ts (`-m`) and
// crates/kanna-agent-protocol/src/codex.rs (`-c model_reasoning_effort=`).
//
// TRUST_PROMPT and COMPOSER are carried from tests/live/codex-tui-quit.test.ts,
// which has actually been run and passed. Everything else below (the
// observed busy-phase boundary, the quota/auth-vs-harness-failure split, the
// observed bracketed-paste check, both message-size cases) is new for this
// file.

const TRUST_PROMPT = /trustthecontentsofthisdirectory/i;
// Not the same COMPOSER pattern codex-tui-quit.test.ts carries
// (/\/modeltochange|Use\/skills/i), deliberately: this file's own first real
// run showed "/model to change" is part of the static model-info header
// panel, visible immediately at launch and still visible while codex prints
// "Booting MCP server: codex_apps (0s • esc to interrupt)" — a genuinely
// busy, not-yet-accepting-input state where a submitted message is queued
// ("tab to queue message"), not acted on. Matching on it made reachComposer
// return "ready" before codex actually was, which is exactly why the
// observed-busy-phase check below never saw its start file appear — a
// harness precondition gap, not evidence about submission. "Use /skills" is
// the strictly narrower half of the same OR that codex-tui-quit.test.ts
// already relies on; keeping only it does not weaken anything that test
// proved, and was not re-verified live in this pass (its own live run
// showed the busy-boot state, not a green composer-ready read either way).
const COMPOSER = /Use\/skills/i;

// Substrings captured verbatim in
// tests/cli-contract/fixtures/provider-quota-rejection.json (codex/notice/
// quota-rejection) and helpers/codex-availability.ts's
// CODEX_UNAVAILABLE_PATTERNS — reused here rather than re-guessed, because
// this is the same signal in a different transport (raw PTY screen instead
// of `codex exec --json` lines).
const QUOTA_OR_AUTH_REJECTION_PATTERNS = [
  /hit your usage limit/i,
  /purchase more credits/i,
  /401 Unauthorized/i,
  /Missing bearer or basic authentication/i,
];

function detectQuotaOrAuthRejection(text: string): RegExp | null {
  return QUOTA_OR_AUTH_REJECTION_PATTERNS.find((pattern) => pattern.test(text)) ?? null;
}

const BUSY_START_FILE = ".kanna-busy-phase-start";
const BUSY_END_FILE = ".kanna-busy-phase-end";

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
  const session = startPtySession(
    binary,
    ["--yolo", "-m", "gpt-5.6-sol", "-c", 'model_reasoning_effort="low"'],
    { cwd, env: { CODEX_HOME: codexHome } },
  );
  return { session, cwd, codexHome };
}

/**
 * Reaches the composer, or fails/skips distinctly rather than always
 * skipping. With an installed, authenticated CLI, never recognizing the
 * composer is a harness failure worth investigating (a wrong pattern, or
 * codex's screen having changed) — not something to skip past as green.
 * The one legitimate skip here is codex itself reporting a provider
 * quota/auth rejection: a real, currently-absent prerequisite (this
 * account's usable quota), not a product-code result and not evidence of a
 * submission defect either way.
 */
async function reachComposer(
  setup: CodexTuiSetup,
  ctx: { skip: (reason?: string) => void },
): Promise<boolean> {
  const { session } = setup;
  if (await session.waitForOutput(TRUST_PROMPT, 30_000)) {
    // "1. Yes, continue" is preselected; Enter accepts it.
    session.write("\r");
  }
  const reachedComposer = await session.waitForOutput(COMPOSER, 30_000);
  if (reachedComposer) return true;

  const rejection = detectQuotaOrAuthRejection(session.output);
  if (rejection) {
    ctx.skip(
      `codex reported a provider quota/auth rejection (matched ${rejection}) before reaching ` +
      `its composer — a currently-absent prerequisite (account quota/auth), not a product-code ` +
      `result. TUI tail:\n${session.output.slice(-800)}`,
    );
    return false;
  }

  expect(
    reachedComposer,
    "codex TUI never reached a recognized composer and reported no quota/auth rejection " +
    "either — this is a harness precondition failure (an unrecognized screen), not a pass " +
    `or a skip. TUI tail:\n${session.output.slice(-1500)}`,
  ).toBe(true);
  return false;
}

/**
 * Puts codex into a genuinely observed busy/repainting state using a cheap,
 * deterministic shell sleep — not model-generation pacing (guessed timing
 * from "print slowly", which is neither cheap nor a proof of anything) and
 * not a bare `sleep()` in this test standing in for proof.
 *
 * The proof is filesystem state (`touch`ed by the shell command itself),
 * never transcript text — the same pattern
 * tests/live/opencode-injected-input.test.ts's "quits immediately when the
 * agent is mid-turn" case already uses (`existsSync(startedFile)`/
 * `existsSync(finishedFile)`), not a new one. A first version of this file
 * used literal marker strings embedded in the instruction text and matched
 * against the transcript; it failed instantly on its one real run because
 * Codex's own TUI echoes a submitted message and its command-invocation line
 * back onto the transcript before running anything, so both "start" and
 * "end" marker text appeared together as soon as the message was merely
 * *displayed* — proving nothing about execution. Touched files cannot be
 * satisfied by an echo.
 */
async function enterObservedBusyPhase(
  setup: CodexTuiSetup,
  bracketedPasteMode: boolean,
): Promise<{ started: boolean; startFile: string; endFile: string }> {
  const startFile = join(setup.cwd, BUSY_START_FILE);
  const endFile = join(setup.cwd, BUSY_END_FILE);
  await setup.session.submitLogical(
    `Run this exact shell command now, and do not reply until it finishes: ` +
    `sh -c 'touch ${BUSY_START_FILE}; sleep 12; touch ${BUSY_END_FILE}'`,
    bracketedPasteMode,
  );
  const started = await setup.session.waitUntil(() => existsSync(startFile), 30_000, 500);
  return { started, startFile, endFile };
}

async function teardown(setup: CodexTuiSetup): Promise<void> {
  // Only this test's own PTY child — nothing else on the machine.
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

async function runSubmissionCase(
  ctx: { skip: (reason?: string) => void },
  build: (
    setup: CodexTuiSetup,
    bracketedPasteMode: boolean,
  ) => { markerPath: string; message: string; label: string },
): Promise<void> {
  if (!(await requireEnvironment(ctx))) return;

  const setup = await startCodexTui();
  try {
    if (!(await reachComposer(setup, ctx))) return;

    // Observed from this actual session, not assumed: `logical_message_bytes`
    // only frames when the terminal itself advertised bracketed paste
    // (crates/daemon/src/session.rs::bracketed_paste_mode, set from the
    // DECSET 2004h the headless terminal actually saw). Forcing `true` here
    // regardless would test a framing decision the daemon would never have
    // made for this CLI.
    const bracketedPasteMode = setup.session.sawBracketedPasteEnable();

    const { started, startFile, endFile } = await enterObservedBusyPhase(setup, bracketedPasteMode);
    expect(
      started,
      `codex never entered the observed busy tool-work phase (${startFile} was never created) — ` +
      "the mid-turn condition this case needs was never established, so nothing below would " +
      `prove anything about delivery during a busy terminal. TUI tail:\n${setup.session.output.slice(-1500)}`,
    ).toBe(true);
    expect(
      existsSync(endFile),
      `the observed busy phase already completed (${endFile} exists) before the message under ` +
      "test could be injected — the 12s window closed before injection, so this run cannot " +
      `claim mid-turn delivery either. TUI tail:\n${setup.session.output.slice(-1500)}`,
    ).toBe(false);

    const { markerPath, message, label } = build(setup, bracketedPasteMode);
    await setup.session.submitLogical(message, bracketedPasteMode);

    const wrote = await setup.session.waitUntil(() => existsSync(markerPath), 180_000, 1_000);
    expect(
      wrote,
      `codex never acted on ${label} delivered mid-turn. Cause unattributed: the transcript ` +
      "below is the evidence, not a conclusion — it could be a swallowed submission boundary, " +
      "a CLI-side parser issue, or the model/tool declining the instruction, and only reading " +
      `the actual transcript distinguishes those. bracketedPasteMode observed: ` +
      `${bracketedPasteMode}. TUI tail:\n${setup.session.output.slice(-1500)}`,
    ).toBe(true);
    if (wrote) {
      expect(readFileSync(markerPath, "utf8")).toContain("SUBMITTED_OK");
    }
  } finally {
    await teardown(setup);
  }
}

describe("codex logical-input submission during a busy turn", () => {
  it(
    "a short message delivered mid-turn is acted on, not just written to the pty",
    async (ctx) => {
      await runSubmissionCase(ctx, (setup) => {
        const markerPath = join(setup.cwd, "logical-submission-marker-short.txt");
        return {
          markerPath,
          label: "a short message",
          // Short, single-line, no embedded newline: what an ordinary short
          // mobile Send actually puts on the wire. logicalMessageBytes leaves
          // this unframed regardless of bracketedPasteMode — below
          // PASTE_FRAMING_MIN_LEN (256 bytes), no newline.
          message:
            "Create a file named logical-submission-marker-short.txt containing exactly: SUBMITTED_OK",
        };
      });
    },
    300_000,
  );

  it(
    "a large message delivered mid-turn is acted on, not just written to the pty",
    async (ctx) => {
      await runSubmissionCase(ctx, (setup) => {
        const markerPath = join(setup.cwd, "logical-submission-marker-large.txt");
        // Padded well past PASTE_FRAMING_MIN_LEN (256 bytes) so the daemon
        // frames this as a paste when bracketedPasteMode is true — the shape
        // of the owner's original ~1.2KB report. If this session's own
        // observed bracketedPasteMode is false, logicalMessageBytes leaves it
        // unframed instead; the test still runs, but is no longer exercising
        // the paste-framed path, and its diagnostic says so via the observed
        // value it reports on failure.
        const message =
          "Stop the previous shell command. Create a file named " +
          "logical-submission-marker-large.txt in the current directory containing exactly: " +
          "SUBMITTED_OK. Then stop.\n" +
          `# padding: ${"x".repeat(400)}`;
        return { markerPath, label: "a large message", message };
      });
    },
    300_000,
  );
});
