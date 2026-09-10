import { constants } from "node:fs";
import { access, symlink } from "node:fs/promises";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { findCodexBinary } from "../../helpers/codex";
import { codexBinaryOrNull, ptyBridgeAvailable } from "../../helpers/availability";
import { makeRealTempDir, removeDir } from "../../helpers/background";
import { sleep, startPtySession, type PtySession } from "../../helpers/pty";

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
// TRUST_PROMPT is carried from tests/live/codex-tui-quit.test.ts, which has
// actually been run and passed. COMPOSER_READY/COMPOSER_BUSY_BOOTING and
// waitForComposerReady below replaced that file's own COMPOSER regex after
// two real runs each falsified it a different way (see the comment there).
//
// STATE AS OF THE LATEST REAL RUNS (5 total; docs/2026-09-10-mobile-
// connection-flicker-e2e-note.md has the full evidence): runs 1-4 each
// failed on a harness precondition (echo-based markers, a composer regex
// too broad then too narrow, the codex_apps MCP-boot race) and were each
// corrected in turn. Run 5 applied the `--disable apps` fix and confirmed it
// closed the boot-race signature ("Booting MCP server: codex_apps" no longer
// appears anywhere in the transcript) — but both cases still failed at the
// same assertion, `enterObservedBusyPhase`'s `.kanna-busy-phase-start` check
// (expected true, got false) within the 30s wait. The preserved tail shows
// header/tip render and the submitted instruction visible on the composer
// line, then nothing further within the captured window — no turn-start
// indicator, no tool call, no response. That is consistent with either
// ordinary session-startup latency independent of MCP boot (still exceeding
// 30s on its own) or the actual submission question this file exists to
// answer, and run 5's own evidence — a `session.output.slice(-1500)` tail —
// cannot distinguish an echoed history line from an unsent draft, so it was
// correctly marked unattributed rather than guessed either way. The fixture
// otherwise ran clean: `--yolo -m gpt-5.6-sol -c model_reasoning_effort="low"
// --disable apps` spawn confirmed applied, cleanup confirmed (no leftover
// processes/temp dirs under this fixture's own naming).
//
// Per review of that gap, no 6th run has been executed yet. Instead this
// file now carries the observation instrumentation described immediately
// below (full raw/rendered output retention, Codex's own rollout JSONL as
// submitted/tool-invocation ground truth, and separately timestamped
// composer-ready vs. busy-start-observed checkpoints) — implemented and
// ready, not yet exercised against a real session in this pass. The next
// authorized run is the first one that will actually produce artifacts
// under `.tmp/codex-run-artifacts/` to read.

// OBSERVATION PLAN — added before any 6th run, per review of run 5's own
// evidence gap: `session.output.slice(-1500)` and the visible `›` composer
// line cannot tell an *echoed* history line (what the TUI redraws to show
// what was already submitted) apart from an *unsent draft* still sitting in
// the composer, and composer-text-clearing alone is not proof of submission
// either — both are inferences from a screen-scrape of a bridge that
// concatenates and strips ANSI rather than emulating a terminal grid (see
// PtySession's own class doc). This section adds three things without
// touching the live control flow's assertions or gating conditions:
//
// 1. Full, timestamped retention — not a 1500-char tail. Both the rendered
//    (`PtySession.output`) and fully raw (`PtySession.rawOutput`, ANSI
//    intact) byte streams, plus an explicit checkpoint log
//    (stage -> ISO timestamp) for every stage transition below, written to
//    `.tmp/codex-run-artifacts/<run>/` on every exit (pass or fail), not
//    just captured inline in an assertion string.
//
// 2. Ground truth for "typed draft vs. submitted", from Codex's own record,
//    not a screen inference: Codex persists each session as a JSONL
//    "rollout" file under `$CODEX_HOME/sessions/YYYY/MM/DD/
//    rollout-<timestamp>-<id>.jsonl` (confirmed by inspecting the *shape*
//    of real, pre-existing rollout files under this machine's own
//    `~/.codex/sessions/` — structure/keys only, no message content or
//    secrets were read or are reproduced here). Each line is
//    `{ timestamp, ordinal, type, payload }`; a `type: "response_item"`
//    line with `payload.type === "message" && payload.role === "user"` is
//    Codex's own record of a message it actually treated as submitted and
//    handed to the model — categorically different from anything visible
//    on the composer line, which can show unsent, echoed, or in-flight
//    text indistinguishably to a screen-scrape. `payload.type ===
//    "function_call"` with the shell command in `arguments`, and the
//    matching `function_call_output`, is equally direct evidence that the
//    tool was actually invoked, versus a touched marker file (which proves
//    only that *a* shell ran, not that codex's own engine agrees it was
//    asked to). Because this test's `CODEX_HOME` is a fresh temp dir
//    created and owned solely by this test run (never the real
//    `~/.codex`), any rollout file found under it belongs to this test and
//    nothing else — captured here by copying only the `.jsonl` file(s)
//    themselves (never `auth.json`, never the whole `CODEX_HOME`, which
//    would risk pulling in the symlinked real credential) plus a
//    structural summary that records `ordinal`/`type`/`payload.type`/
//    `payload.role`/`timestamp` and a boolean match against this test's
//    own known marker strings — never the raw `content`/`arguments`/
//    `output` text — so nothing resembling a secret or arbitrary
//    conversation content is written to disk or asserted on.
//
// 3. Readiness and mid-turn stay two separate, separately timestamped
//    checkpoints, not one conflated signal: `composer-ready` (this test's
//    existing `waitForComposerReady` — "codex is ready to accept a new
//    message") is recorded distinctly from `busy-start-observed` ("a turn
//    actually started mid-message" — this test's existing file-touch
//    proof). The rollout's own `event_msg` `task_started`/`item_completed`
//    entries, once captured, give a third, independent cross-check for the
//    same distinction on the *next* run — not wired into this run's live
//    gating, since that would be exactly the kind of blind variant this
//    review asked not to introduce without first seeing what real
//    rollout data looks like.
//
// None of this changes what makes a case pass or fail, and none of it was
// run against a real session in this pass — it is instrumentation, staged
// for the next authorized run.

const ARTIFACTS_ROOT = join(
  dirname(fileURLToPath(import.meta.url)),
  "../../../../.tmp/codex-run-artifacts",
);

interface Checkpoint {
  label: string;
  atIso: string;
  atMs: number;
}

function makeCheckpointRecorder(): { checkpoints: Checkpoint[]; record: (label: string) => void } {
  const checkpoints: Checkpoint[] = [];
  return {
    checkpoints,
    record(label: string) {
      checkpoints.push({ label, atIso: new Date().toISOString(), atMs: Date.now() });
    },
  };
}

/** Every `.jsonl` under `<codexHome>/sessions/**` — the rollout file(s) this
 * specific, test-owned session wrote, if any. Never touches `auth.json` or
 * anything else in `codexHome`. */
function findRolloutFiles(codexHome: string): string[] {
  const sessionsRoot = join(codexHome, "sessions");
  if (!existsSync(sessionsRoot)) return [];
  const found: string[] = [];
  const walk = (dir: string) => {
    for (const entry of readdirSync(dir)) {
      const full = join(dir, entry);
      const stat = statSync(full);
      if (stat.isDirectory()) walk(full);
      else if (entry.endsWith(".jsonl")) found.push(full);
    }
  };
  walk(sessionsRoot);
  return found;
}

interface RolloutLineSummary {
  ordinal: unknown;
  type: unknown;
  payloadType: unknown;
  role: unknown;
  timestamp: unknown;
  matchesBusyPhaseInstruction: boolean;
  matchesTestMessage: boolean;
}

/** Structural summary only — `ordinal`/`type`/`payload.type`/`payload.role`/
 * `timestamp`, plus a boolean match against caller-supplied marker strings.
 * Never extracts or returns `payload.content`/`arguments`/`output` text. */
function summarizeRolloutStructurally(path: string, markers: string[]): RolloutLineSummary[] {
  const lines = readFileSync(path, "utf8").split("\n").filter((line) => line.trim().length > 0);
  return lines.map((line) => {
    let parsed: Record<string, unknown> = {};
    try {
      parsed = JSON.parse(line);
    } catch {
      return {
        ordinal: null,
        type: "PARSE_ERROR",
        payloadType: null,
        role: null,
        timestamp: null,
        matchesBusyPhaseInstruction: false,
        matchesTestMessage: false,
      };
    }
    const payload = (parsed.payload ?? {}) as Record<string, unknown>;
    const serializedPayload = JSON.stringify(payload);
    return {
      ordinal: parsed.ordinal,
      type: parsed.type,
      payloadType: payload.type,
      role: payload.role,
      timestamp: parsed.timestamp,
      matchesBusyPhaseInstruction: serializedPayload.includes(markers[0] ?? " "),
      matchesTestMessage: serializedPayload.includes(markers[1] ?? " "),
    };
  });
}

/** Retains full timestamped PTY output (rendered and raw) plus checkpoints
 * and any rollout evidence this session's own CODEX_HOME produced, under
 * `.tmp/codex-run-artifacts/` (gitignored, not committed). Called from
 * teardown, before either temp dir is removed, so nothing is lost to
 * cleanup. Read-only with respect to CODEX_HOME: copies only the rollout
 * `.jsonl` file(s), never `auth.json`, never the directory itself. */
interface ProcessCleanupReport {
  bridgePid: number | undefined;
  childPids: number[];
  forceKilled: number[];
  stillAlive: number[];
}

function captureArtifacts(
  setup: CodexTuiSetup,
  label: string,
  checkpoints: Checkpoint[],
  markers: string[],
  processCleanup: ProcessCleanupReport,
): void {
  const runDir = join(ARTIFACTS_ROOT, `${new Date().toISOString().replace(/[:.]/g, "-")}-${label}`);
  mkdirSync(runDir, { recursive: true });
  writeFileSync(join(runDir, "output.txt"), setup.session.output);
  writeFileSync(join(runDir, "raw-output.txt"), setup.session.rawOutput);
  writeFileSync(join(runDir, "checkpoints.json"), JSON.stringify(checkpoints, null, 2));
  writeFileSync(join(runDir, "process-cleanup.json"), JSON.stringify(processCleanup, null, 2));
  const rolloutFiles = findRolloutFiles(setup.codexHome);
  writeFileSync(join(runDir, "rollout-files-found.json"), JSON.stringify(rolloutFiles, null, 2));
  rolloutFiles.forEach((path, i) => {
    copyFileSync(path, join(runDir, `rollout-${i}.jsonl`));
    const summary = summarizeRolloutStructurally(path, markers);
    writeFileSync(join(runDir, `rollout-${i}-summary.json`), JSON.stringify(summary, null, 2));
  });
}

const TRUST_PROMPT = /trustthecontentsofthisdirectory/i;

// Two real runs now falsified two different single-regex composer checks,
// for the same underlying reason: `PtySession.output`/`waitForOutput` search
// the *entire* accumulated byte history, never just the current screen (this
// bridge is a byte concatenator, not a real terminal emulator that overwrites
// a grid) — so any text codex ever printed, including a stale "still
// booting" banner from seconds ago, stays matchable forever.
//   - Run 2: `/\/modeltochange|Use\/skills/i` (carried from the already-
//     passing codex-tui-quit.test.ts) matched "/model to change", part of
//     the *static* model-info header shown immediately at launch — true
//     throughout the whole "Booting MCP server: codex_apps" window, so
//     reachComposer reported ready before codex actually was.
//   - Run 3, after narrowing to `/Use\/skills/i` alone: never matched at
//     all — that tip is one of several *rotating* placeholder hints codex
//     cycles through, not a stable signal, confirming the "rotates" risk.
//     The same tail *did* show "Booting MCP server: codex_apps (0s • esc to
//     interrupt)" beside " Ask Codex to do anything" — the real composer's
//     idle placeholder text — meaning by the time either was captured, both
//     the stale boot banner and a genuinely-ready composer had, at various
//     points, occupied the same accumulated buffer.
// The fix below stops trying to find one string that only ever appears once
// truly ready, and instead requires two facts about the *recent tail* of
// output specifically (not the whole history): the ready placeholder is
// present, and the busy-boot banner is not — see waitForComposerReady.
const COMPOSER_READY = /Ask Codex to do anything/i;
const COMPOSER_BUSY_BOOTING = /Booting MCP server/i;
/** How much of the tail to treat as "the current screen" for readiness
 * purposes — generous enough to span one full redraw of the boxed banner
 * plus footer, per the ~1500-char TUI dumps captured in runs 2 and 3. */
const COMPOSER_RECENCY_WINDOW = 3_000;

async function waitForComposerReady(session: PtySession, timeoutMs: number): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const recent = session.output.slice(-COMPOSER_RECENCY_WINDOW);
    if (COMPOSER_READY.test(recent) && !COMPOSER_BUSY_BOOTING.test(recent)) {
      return true;
    }
    if (Date.now() >= deadline || session.exited) return false;
    await sleep(500);
  }
}

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
    ["--yolo", "-m", "gpt-5.6-sol", "-c", 'model_reasoning_effort="low"', "--disable", "apps"],
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
  const reachedComposer = await waitForComposerReady(session, 45_000);
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

async function teardown(
  setup: CodexTuiSetup,
  label: string,
  checkpoints: Checkpoint[],
  markers: string[],
): Promise<ProcessCleanupReport> {
  // Only this test's own PTY child — nothing else on the machine. Kills the
  // bridge and verifies (force-killing by exact pid if needed) that the real
  // agent CLI process it forked died too, rather than assuming the wrapper's
  // exit implies it.
  const processCleanup = await setup.session.killTreeAndVerify();
  if (processCleanup.stillAlive.length > 0) {
    console.error(
      `teardown for "${label}": pid(s) ${processCleanup.stillAlive.join(", ")} survived both the ` +
      "bridge kill and a follow-up SIGKILL — see process-cleanup.json.",
    );
  }
  try {
    // Capture before removing either temp dir — this is the one place the
    // rollout file and full output would otherwise be lost to cleanup.
    // Guarded: a capture failure (e.g. an unreadable rollout file) must
    // never skip the temp-dir removal below.
    captureArtifacts(setup, label, checkpoints, markers, processCleanup);
  } catch (error) {
    console.error(`artifact capture failed for "${label}" — cleanup proceeds regardless:`, error);
  } finally {
    await removeDir(setup.cwd);
    await removeDir(setup.codexHome);
  }
  return processCleanup;
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
  const { checkpoints, record } = makeCheckpointRecorder();
  record("case-start");
  if (!(await requireEnvironment(ctx))) return;

  const setup = await startCodexTui();
  record("codex-spawned");
  // Populated once `build()` runs below; captured on every exit path,
  // including an early return from reachComposer/enterObservedBusyPhase, so
  // markers defaults to just the busy-phase instruction until then.
  let markers: string[] = [BUSY_START_FILE];
  let label = "unlabeled";
  try {
    if (!(await reachComposer(setup, ctx))) return;
    // Distinct from busy-start-observed below: this is "codex is ready to
    // accept a new message" (initial submission readiness), not "a turn
    // actually started mid-message" (the mid-turn condition) — the two
    // conditions this review asked to keep separately timestamped rather
    // than inferred from one conflated signal.
    record("composer-ready");

    // Observed from this actual session, not assumed: `logical_message_bytes`
    // only frames when the terminal itself advertised bracketed paste
    // (crates/daemon/src/session.rs::bracketed_paste_mode, set from the
    // DECSET 2004h the headless terminal actually saw). Forcing `true` here
    // regardless would test a framing decision the daemon would never have
    // made for this CLI.
    const bracketedPasteMode = setup.session.sawBracketedPasteEnable();
    record(`bracketed-paste-observed:${bracketedPasteMode}`);

    const { started, startFile, endFile } = await enterObservedBusyPhase(setup, bracketedPasteMode);
    record(`busy-start-observed:${started}`);
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

    const built = build(setup, bracketedPasteMode);
    label = built.label;
    markers = [BUSY_START_FILE, built.message];
    await setup.session.submitLogical(built.message, bracketedPasteMode);
    record("test-message-submitted");

    const wrote = await setup.session.waitUntil(() => existsSync(built.markerPath), 180_000, 1_000);
    record(`marker-observed:${wrote}`);
    expect(
      wrote,
      `codex never acted on ${label} delivered mid-turn. Cause unattributed: the transcript ` +
      "below is the evidence, not a conclusion — it could be a swallowed submission boundary, " +
      "a CLI-side parser issue, or the model/tool declining the instruction, and only reading " +
      `the actual transcript distinguishes those. bracketedPasteMode observed: ` +
      `${bracketedPasteMode}. TUI tail:\n${setup.session.output.slice(-1500)}`,
    ).toBe(true);
    if (wrote) {
      expect(readFileSync(built.markerPath, "utf8")).toContain("SUBMITTED_OK");
    }
  } finally {
    record("teardown-start");
    // process-cleanup.json (written inside teardown, alongside the other
    // artifacts) is the durable record of forceKilled/stillAlive — read
    // that rather than a checkpoint here, since teardown captures artifacts
    // before this function could append to the in-memory array anyway.
    await teardown(setup, label, checkpoints, markers);
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
