//! Shutting the source agent down so its conversation can be shipped.
//!
//! The old mechanism was a single `SIGINT` to the source session followed by a
//! 1500 ms wait. It could not work, for a reason that only shows up after an
//! app upgrade: the daemon **refuses signals for adopted sessions** — sessions
//! it inherited through handoff, where it holds the master fd but never forked
//! the child, so the pid cannot be pinned across `kill(2)`
//! (`crates/daemon/src/pty.rs`). It fails closed by design. Every session older
//! than the running daemon is adopted, so after every upgrade no pre-existing
//! task could be finalized at all. On 2026-08-06 that is exactly what happened:
//! `[handoff] adopted session …` at 10:43, the signal refused at 13:43.
//!
//! Injected input has none of that constraint. `Command::SubmitInputIfSession`
//! accepts a logical message for an adopted session while fencing it to the PTY
//! process observed at attach. So finalization *asks* the agent to stop instead
//! of signalling it:
//!
//! 1. inject a wrap-up message and wait for the daemon's delivery acknowledgement;
//! 2. use the existing settled-`Idle` policy on the daemon `StatusChanged`
//!    stream before sending anything else;
//! 3. inject the provider's quit command (`/exit`, `/quit` for Codex);
//! 4. wait for the daemon `Exit`.
//!
//! On the clean path, only then are artifacts staged, which is also what fixes
//! Codex: its rollout under `~/.codex/sessions` is nameable long before the
//! process exits but is still growing at that point, so the old mid-session
//! staging shipped a truncated conversation (pinned by
//! `tests/cli-contract/tests/live/codex-rollout-timing.test.ts`).
//!
//! Step 2 is a sequencing heuristic, not a provider acknowledgement: daemon
//! status has no input identity and a fast turn may never publish `Busy`. It is
//! retained because the quit command preempts a mid-turn agent (pinned against
//! OpenCode in `opencode-injected-input.test.ts`). What finalization can prove
//! locally is narrower: only a fresh daemon acknowledgement permits this attempt
//! to continue; a pre-existing phase claim or uncertain reply does not.
//!
//! **`Waiting` is not `Idle`, and nothing may be typed while it holds.** It
//! means the agent is parked on a permission prompt, which consumes the next
//! input as its *answer* — and the submission policy ends every message with a
//! discrete CR, which is exactly the keystroke that accepts the prompt's
//! highlighted option. Approving a pending tool call on the operator's behalf
//! is not something a transfer may do, and it would be silent: the agent would
//! resume, reach `Idle`, quit on cue, and ship `cleanlyFinalized: true` with
//! nothing anywhere saying a tool call had been approved.
//!
//! The quit gets that guarantee from step 2, which only lets it through on
//! `Idle`. The wrap-up has nothing in front of it, so it checks the status
//! `attach` read off the daemon itself. A session already parked when
//! finalization starts degrades on the spot rather than waiting: nobody is
//! going to answer that prompt — the operator is in the
//! middle of pushing the task away from this machine — so waiting out the
//! wrap-up budget would buy minutes of user-visible latency and reach the same
//! verdict. A session that parks *during* the wrap-up reaches the same rung
//! through the idle timeout. Pushing a task that is parked on a prompt is a
//! normal thing to do; it is often *why* someone pushes it, and the destination
//! resumes with the prompt still to answer.
//!
//! The ladder, each rung loud and recorded on the transfer:
//!
//! - injection failure, or the session never reaching `Idle`, degrades the
//!   finalization: artifacts are staged from the transcript as it stands and
//!   the payload carries `cleanlyFinalized: false` plus the reason. That rung
//!   only ships a conversation because Claude appends to its transcript while
//!   the session runs, which is pinned by
//!   `tests/cli-contract/tests/live/claude-transcript-append.test.ts`.
//! - destructive teardown stays the last resort and stays *after* staging: the
//!   source task's session is killed by `close_task_in_process` once the
//!   destination acknowledges the import (`push::outgoing_committed`), which is
//!   a SIGKILL sweep authenticated by the master fd and therefore works on
//!   adopted sessions. Finalization does not duplicate it — killing here would
//!   destroy a live agent for a transfer that may still fail.

use crate::db::{TaskEventKind, TransferWorkItem};
use crate::http_api::{try_submit_task_input_if_session, AppState, TaskInputError};
use kanna_agent_protocol::AgentProvider;
use kanna_daemon::protocol::{Command as DaemonCommand, Event as DaemonEvent, SessionStatus};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

/// The sequence spans minutes of waiting, so it opens a DB connection per step
/// rather than holding one across the waits — the same shape the engine's drain
/// loop uses, and the only one that keeps the future `Send` (`rusqlite`'s
/// connection is `Send` but not `Sync`, so a shared `&Db` cannot cross an
/// `await`).
fn open_db(state: &Arc<AppState>) -> Result<crate::db::Db, String> {
    state.transfer_work().open_db()
}

/// What the source agent is told before it is asked to quit.
///
/// It is written to be acted on by any provider's model: say what is happening,
/// bound the work, and forbid starting anything new. The reply it produces is
/// the last thing appended to the transcript the destination resumes from.
const WRAP_UP_MESSAGE: &str = "This task is being transferred to another machine right now. \
     Wrap up: finish the thought you are on, do not start any new work, and do not run any \
     further commands. Briefly state where you left off. Your conversation is being shipped \
     and will resume on the destination machine.";

/// How long the source agent gets to finish its turn after the wrap-up.
///
/// Generous on purpose. The old mechanism allowed 1500 ms, which was never a
/// wrap-up budget at all — it was how long it waited for a `SIGINT` to take. A
/// busy agent legitimately takes minutes to close out a turn, and the cost of
/// waiting is user-visible latency on a transfer, while the cost of not waiting
/// is a truncated conversation.
///
/// This is not a free number. The destination is blocked on this finalization
/// over a peer request the whole time, so it has to be bounded by what that
/// request allows — `kanna_runtime_defaults::TRANSFER_FINALIZATION_REQUEST_TIMEOUT`,
/// held by [`the_shutdown_budget_fits_inside_the_peer_finalization_window`].
const WRAP_UP_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a session that is *already* `Idle` may stay silent before the
/// existing completion policy lets finalization proceed.
///
/// The daemon only publishes status changes, so a turn shorter than its
/// detector interval can produce no `Busy` edge. This is deliberately a
/// heuristic, not proof that a particular input was parsed or completed.
const IDLE_SETTLE: Duration = Duration::from_secs(20);

/// How long an observed `Idle` edge must hold before finalization proceeds.
///
/// Short idle repaints can occur between stretches of one turn, so an edge uses
/// a settle window rather than releasing the quit immediately. Like
/// [`IDLE_SETTLE`], this is existing sequencing policy rather than causal input
/// acknowledgement.
const IDLE_EDGE_SETTLE: Duration = Duration::from_secs(2);

/// How long the agent gets to exit after the quit command.
///
/// This is a process teardown, not a turn: a provider that has not exited by
/// now is not going to.
const QUIT_EXIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Claimed before the wrap-up is injected, so a work item resumed after a crash
/// does not type the message into the agent a second time.
const WRAP_UP_PHASE: &str = "finalization-wrap-up";
/// Claimed before the quit command is injected, for the same reason.
const QUIT_PHASE: &str = "finalization-quit";
/// Where the verdict is recorded, first-writer-wins.
///
/// Only the attempt that ran against a live agent can judge whether the
/// shutdown was clean. By the time a retry looks, the session is gone — which
/// is indistinguishable from "it exited cleanly" — so a retry that recomputed
/// the verdict would quietly upgrade a degraded finalization to a clean one.
const OUTCOME_PHASE: &str = "finalization-outcome";

/// What finalization achieved, and what the destination is told about it.
#[derive(Debug, Default)]
pub(super) struct SourceFinalization {
    /// `None` when the agent shut down cleanly; otherwise the reason the
    /// payload is marked `cleanlyFinalized: false`.
    pub degraded_reason: Option<String>,
    /// The source terminal as it looked before the agent was asked to quit.
    ///
    /// Captured mid-sequence rather than after it, because there is nothing
    /// left to photograph once the process has exited — and the destination
    /// replays this so the operator arrives at the terminal they left.
    pub recovery_snapshot: Option<crate::mobile_api::CreateTaskRecoverySnapshot>,
}

impl SourceFinalization {
    pub(super) fn cleanly_finalized(&self) -> bool {
        self.degraded_reason.is_none()
    }
}

/// Runs the shutdown sequence for one transfer's source session.
///
/// Never fails the transfer: every way this can go wrong degrades the
/// finalization instead, because a transfer that ships a slightly stale
/// conversation is recoverable and one that refuses to ship at all is not. The
/// payload-level refusals — a session that vanished, a promised artifact that
/// is not there — are the caller's, and they run after this.
pub(super) async fn finalize_source_session(
    state: &Arc<AppState>,
    work: &TransferWorkItem,
    task_id: &str,
    agent_type: Option<&str>,
    agent_provider: Option<&str>,
) -> SourceFinalization {
    // An `agent`-type session is headless: there is no TUI to type into and no
    // transcript being held open by a live process.
    if agent_type != Some("pty") {
        return SourceFinalization::default();
    }

    // The verdict from the attempt that ran against the live agent wins.
    if let Ok(Some(recorded)) = open_db(state).and_then(|db| {
        db.read_transfer_work_observation(&work.id, OUTCOME_PHASE)
            .map_err(|error| format!("db error: {error}"))
    }) {
        log::info!(
            "transfer finalization for {task_id} already reached a verdict on an earlier attempt: {}",
            recorded.as_deref().unwrap_or("clean"),
        );
        return SourceFinalization {
            degraded_reason: recorded,
            recovery_snapshot: None,
        };
    }

    let outcome = run_sequence(state, work, task_id, agent_provider).await;
    let recorded = open_db(state).and_then(|db| {
        db.record_transfer_work_observation(
            &work.id,
            OUTCOME_PHASE,
            outcome.degraded_reason.as_deref(),
        )
        .map_err(|error| format!("db error: {error}"))
    });
    if let Err(error) = recorded {
        log::error!("failed to record the finalization verdict for {task_id}: {error}");
    }
    outcome
}

async fn run_sequence(
    state: &Arc<AppState>,
    work: &TransferWorkItem,
    task_id: &str,
    agent_provider: Option<&str>,
) -> SourceFinalization {
    let mut observer = match SessionObserver::attach(&state.config().daemon_dir, task_id).await {
        Ok(observer) => observer,
        Err(error) => {
            return degraded(
                state,
                task_id,
                format!("the source agent session could not be observed: {error}"),
            )
        }
    };
    if !observer.present {
        // A vanished session is clean only when no lifecycle effect was
        // claimed ambiguously.  If a prior attempt claimed preparation or
        // quit and crashed before recording its outcome, absence cannot prove
        // that the bytes were never delivered; upgrading that state to clean
        // would both hide uncertainty and permit an unsafe retry.
        let ambiguous_phase = match open_db(state) {
            Ok(db) => {
                let mut found = None;
                for phase in [WRAP_UP_PHASE, QUIT_PHASE] {
                    match db.read_transfer_work_observation(&work.id, phase) {
                        Ok(Some(_)) => {
                            found = Some(phase);
                            break;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            return degraded(
                                state,
                                task_id,
                                format!(
                                    "could not establish finalization phase {phase} after session disappearance: {error}"
                                ),
                            );
                        }
                    }
                }
                found
            }
            Err(error) => {
                return degraded(
                    state,
                    task_id,
                    format!(
                        "could not inspect finalization phase history after session disappearance: {error}"
                    ),
                );
            }
        };
        if let Some(phase) = ambiguous_phase {
            return degraded(
                state,
                task_id,
                format!(
                    "the source agent session disappeared while finalization phase {phase} had no durable delivery outcome"
                ),
            );
        }
        // Nothing to wrap up: the conversation on disk is already whole, which
        // is the state this whole sequence exists to reach.
        record_phase(state, task_id, "already-exited", None);
        return SourceFinalization::default();
    }

    // Nothing may be typed at a session parked on a permission prompt — the
    // wrap-up's trailing CR would accept the prompt's highlighted option and
    // approve a pending tool call in the operator's name. `attach` read the
    // live status off the daemon's session list, so this is known before a
    // single byte goes out.
    if observer.status == SessionStatus::Waiting {
        return degraded(
            state,
            task_id,
            "the source agent is parked on a permission prompt, so it was not asked to wrap up: \
             answering that prompt is the operator's to do, and any input sent now would answer it"
                .to_string(),
        );
    }

    let provider = match agent_provider.and_then(|provider| AgentProvider::from_str(provider).ok()) {
        Some(provider) => provider,
        None => {
            return degraded(
                state,
                task_id,
                format!(
                    "the source agent provider {:?} is unavailable, so no quit command can be chosen safely",
                    agent_provider
                ),
            )
        }
    };
    let quit_command = provider.quit_command();
    let Some(session_pid) = observer.pid else {
        return degraded(
            state,
            task_id,
            "the source agent session was listed without a PTY process id".to_string(),
        );
    };

    // 1. Wrap-up.
    match inject(
        state,
        work,
        task_id,
        session_pid,
        WRAP_UP_PHASE,
        WRAP_UP_MESSAGE,
    )
    .await
    {
        Injected::Sent => {
            record_phase(state, task_id, "wrap-up-sent", None);
        }
        Injected::SessionGone => match source_session_is_absent(state, task_id).await {
            Ok(true) => {
                record_phase(state, task_id, "already-exited", None);
                return SourceFinalization::default();
            }
            Ok(false) => {
                return degraded(
                    state,
                    task_id,
                    "the source agent session changed before the wrap-up could be delivered"
                        .to_string(),
                )
            }
            Err(reason) => return degraded(state, task_id, reason),
        },
        Injected::Failed(reason) => {
            return degraded(
                state,
                task_id,
                format!("the source agent could not be asked to wrap up: {reason}"),
            );
        }
        Injected::DeliveryUnknown(reason) => {
            return degraded(
                state,
                task_id,
                format!(
                    "the source agent's wrap-up delivery is uncertain, so no quit command was sent: {reason}"
                ),
            );
        }
    }

    // 2. Existing settled-idle sequencing policy. Daemon status carries no
    // input identity, so this is deliberately not called proof of preparation.
    match observer
        .wait_for_idle(WRAP_UP_TIMEOUT, IDLE_SETTLE, IDLE_EDGE_SETTLE)
        .await
    {
        IdleOutcome::Idle => record_phase(state, task_id, "idle", None),
        IdleOutcome::Exited { killed: false } => {
            // The agent ended its own session while wrapping up. That is the
            // destination state, reached without the quit command.
            record_phase(state, task_id, "exited", None);
            return SourceFinalization::default();
        }
        IdleOutcome::Exited { killed: true } => {
            return degraded(
                state,
                task_id,
                "the source agent was forcibly killed while finalization was waiting for it"
                    .to_string(),
            )
        }
        IdleOutcome::TimedOut(status) => {
            let detail = match status {
                // Typing the quit command now would answer the prompt, not quit
                // the agent. Refusing to is the whole point of keying on `Idle`.
                SessionStatus::Waiting => {
                    "it is parked on a permission prompt and was not answered on the operator's behalf"
                }
                _ => "it was still working",
            };
            return degraded(
                state,
                task_id,
                format!(
                    "the source agent did not finish its turn within {}s: {detail}",
                    WRAP_UP_TIMEOUT.as_secs(),
                ),
            );
        }
    }

    // 3. The terminal picture, while there is still a terminal.
    let recovery_snapshot = super::push::session_recovery_snapshot(state, task_id).await;

    // 4. Quit.
    match inject(state, work, task_id, session_pid, QUIT_PHASE, quit_command).await {
        Injected::Sent => {
            record_phase(state, task_id, "quit-sent", Some(quit_command));
        }
        Injected::SessionGone => match source_session_is_absent(state, task_id).await {
            Ok(true) => {
                record_phase(state, task_id, "exited", None);
                return SourceFinalization {
                    degraded_reason: None,
                    recovery_snapshot,
                };
            }
            Ok(false) => {
                let mut outcome = degraded(
                    state,
                    task_id,
                    "the source agent session changed before the quit command could be delivered"
                        .to_string(),
                );
                outcome.recovery_snapshot = recovery_snapshot;
                return outcome;
            }
            Err(reason) => {
                let mut outcome = degraded(state, task_id, reason);
                outcome.recovery_snapshot = recovery_snapshot;
                return outcome;
            }
        },
        Injected::Failed(reason) => {
            let mut outcome = degraded(
                state,
                task_id,
                format!(
                    "the source agent could not be asked to quit with {quit_command}: {reason}"
                ),
            );
            outcome.recovery_snapshot = recovery_snapshot;
            return outcome;
        }
        Injected::DeliveryUnknown(reason) => {
            let mut outcome = degraded(
                state,
                task_id,
                format!(
                    "delivery of the source agent's {quit_command} command is uncertain: {reason}"
                ),
            );
            outcome.recovery_snapshot = recovery_snapshot;
            return outcome;
        }
    }

    // 5. Exit.
    match observer.wait_for_exit(QUIT_EXIT_TIMEOUT).await {
        ExitOutcome::Exited { killed: false } => {
            record_phase(state, task_id, "exited", None);
            SourceFinalization {
                degraded_reason: None,
                recovery_snapshot,
            }
        }
        ExitOutcome::Exited { killed: true } => {
            let mut outcome = degraded(
                state,
                task_id,
                "the source agent was forcibly killed after the quit command was delivered"
                    .to_string(),
            );
            outcome.recovery_snapshot = recovery_snapshot;
            outcome
        }
        ExitOutcome::TimedOut => {
            let mut outcome = degraded(
                state,
                task_id,
                format!(
                    "the source agent did not exit within {}s of {quit_command}",
                    QUIT_EXIT_TIMEOUT.as_secs(),
                ),
            );
            outcome.recovery_snapshot = recovery_snapshot;
            outcome
        }
    }
}

/// A fenced submission reports both an absent session and a same-id PTY
/// replacement as `SessionNotFound` to ordinary task-input callers. Finalization
/// may call the former clean but must degrade the latter, so refresh the daemon
/// snapshot before deciding. The check never types into either incarnation.
async fn source_session_is_absent(state: &Arc<AppState>, task_id: &str) -> Result<bool, String> {
    SessionObserver::attach(&state.config().daemon_dir, task_id)
        .await
        .map(|observer| !observer.present)
        .map_err(|error| {
            format!(
                "the source agent session disappeared during finalization and its current state could not be confirmed: {error}"
            )
        })
}

fn degraded(state: &Arc<AppState>, task_id: &str, reason: String) -> SourceFinalization {
    log::warn!("transfer finalization for {task_id} degraded: {reason}");
    record_phase(state, task_id, "degraded", Some(&reason));
    SourceFinalization {
        degraded_reason: Some(reason),
        recovery_snapshot: None,
    }
}

/// Publishes one step of the sequence onto the task event feed.
///
/// A wrap-up is minutes of latency the operator did not ask for, so the phase
/// has to be visible somewhere other than the server log. Best effort: a feed
/// write that fails must not fail a finalization that is otherwise working.
fn record_phase(state: &Arc<AppState>, task_id: &str, phase: &str, detail: Option<&str>) {
    log::info!("transfer finalization for {task_id}: {phase}");
    let payload = match detail {
        Some(detail) => serde_json::json!({ "phase": phase, "detail": detail }),
        None => serde_json::json!({ "phase": phase }),
    };
    let appended = open_db(state).and_then(|db| {
        db.append_task_event(task_id, TaskEventKind::TransferFinalizing, payload)
            .map_err(|error| format!("db error: {error}"))
    });
    if let Err(error) = appended {
        log::warn!("failed to record transfer finalization phase for {task_id}: {error}");
    }
}

enum Injected {
    Sent,
    /// The phase is claimed or the daemon round trip was lost, so delivery may
    /// have happened but neither submission nor non-delivery is proven.
    DeliveryUnknown(String),
    SessionGone,
    Failed(String),
}

/// Submits one message to the observed PTY incarnation, at most once for the
/// life of the work item.
///
/// The claim is taken before the write and given back only when the write
/// definitely did not land. `TaskInputError::Uncertain` means the message bytes
/// may have reached the PTY before the response was lost, so the claim is kept: a
/// retry that re-typed a wrap-up (or a second `/exit`) would corrupt the
/// composer of an agent that already has the first one. Neither uncertainty nor
/// an existing claim is success: only a fresh daemon acknowledgement may let
/// this attempt observe preparation completion and send the quit command.
/// `Other` does release: nothing reached the terminal, so re-claiming and
/// retrying is both safe and the only way the message ever arrives.
async fn inject(
    state: &Arc<AppState>,
    work: &TransferWorkItem,
    task_id: &str,
    expected_pid: u32,
    phase: &str,
    message: &str,
) -> Injected {
    // Claimed before the write, so a crash between the two leaves the claim
    // held and the resumed item does not type it again.
    match open_db(state).and_then(|db| {
        db.claim_transfer_work_phase(&work.id, phase)
            .map_err(|error| format!("db error: {error}"))
    }) {
        Ok(true) => {}
        Ok(false) => {
            return Injected::DeliveryUnknown(format!(
                "an earlier attempt claimed {phase}, but no durable observation proves what reached the terminal"
            ))
        }
        Err(error) => return Injected::Failed(error),
    }
    let release = |reason: String| {
        let released = open_db(state).and_then(|db| {
            db.release_transfer_work_phase(&work.id, phase)
                .map_err(|error| format!("db error: {error}"))
        });
        if let Err(error) = released {
            log::error!("failed to release the {phase} claim for {task_id}: {error}");
        }
        Injected::Failed(reason)
    };

    let mut daemon =
        match crate::daemon_client::DaemonClient::connect(&state.config().daemon_dir).await {
            Ok(daemon) => daemon,
            Err(error) => return release(format!("daemon error: {error}")),
        };
    // Keep the ordinary always-submit logical-input behavior, but fence this
    // lifecycle command to the PTY incarnation the observer attached to.
    match try_submit_task_input_if_session(&mut daemon, task_id, expected_pid, message).await {
        Ok(()) => Injected::Sent,
        Err(TaskInputError::SessionNotFound) => Injected::SessionGone,
        Err(TaskInputError::Uncertain(reason)) => {
            log::warn!("transfer finalization {phase} for {task_id} may have landed: {reason}");
            Injected::DeliveryUnknown(reason)
        }
        Err(TaskInputError::Other(reason)) => release(reason),
    }
}

enum IdleOutcome {
    Idle,
    Exited {
        killed: bool,
    },
    /// Carries the status it gave up on, which decides how the ladder reports.
    TimedOut(SessionStatus),
}

enum ExitOutcome {
    Exited { killed: bool },
    TimedOut,
}

/// A read-only view of one session's daemon event stream.
///
/// Subscribed rather than polled — the daemon already publishes what this needs
/// — and deliberately on its own connection: `send_command` reads a single line
/// and takes it as the response, so issuing commands on a subscribed connection
/// risks handing a pushed event back as a command result. Input goes out over
/// short-lived connections of its own.
struct SessionObserver {
    reader: crate::daemon_client::DaemonClientReader,
    /// Held for the observer's whole life, and never written to after the
    /// handshake.
    ///
    /// Dropping it is not free: `OwnedWriteHalf::drop` shuts the socket's write
    /// side down, the daemon's command loop reads EOF and breaks, and breaking
    /// aborts the subscription task feeding this reader
    /// (`crates/daemon/src/connection.rs`). The stream this observer exists to
    /// read would end milliseconds after it was opened, and every PTY
    /// finalization would degrade with "the agent did not finish its turn"
    /// before the agent had a chance to. `terminal_watcher` holds its whole
    /// `DaemonClient` for the life of its subscription for the same reason.
    _writer: crate::daemon_client::DaemonClientWriter,
    session_id: String,
    status: SessionStatus,
    present: bool,
    pid: Option<u32>,
}

impl SessionObserver {
    async fn attach(daemon_dir: &str, session_id: &str) -> Result<Self, String> {
        let client = crate::daemon_client::DaemonClient::connect(daemon_dir)
            .await
            .map_err(|error| error.to_string())?;
        let (reader, mut writer) = client.into_split();
        // Subscribe *then* list, both written before either answer is read: a
        // status change between the snapshot and the stream would otherwise
        // fall in the gap and be lost.
        writer
            .send_one_way(&DaemonCommand::Subscribe)
            .await
            .map_err(|error| error.to_string())?;
        writer
            .send_one_way(&DaemonCommand::List)
            .await
            .map_err(|error| error.to_string())?;

        let mut observer = Self {
            reader,
            _writer: writer,
            session_id: session_id.to_string(),
            status: SessionStatus::Idle,
            present: false,
            pid: None,
        };
        // Everything ahead of the session list is either the Subscribe ack or a
        // pushed event; both are folded in before the list overwrites them.
        let mut listed = false;
        while !listed {
            match observer
                .reader
                .read_event()
                .await
                .map_err(|error| error.to_string())?
            {
                DaemonEvent::SessionList { sessions } => {
                    if let Some(session) = sessions
                        .iter()
                        .find(|session| session.session_id == observer.session_id)
                    {
                        observer.present = true;
                        observer.status = session.status;
                        observer.pid = Some(session.pid);
                    }
                    listed = true;
                }
                DaemonEvent::Error { message, .. } => {
                    return Err(format!("daemon list error: {message}"))
                }
                other => {
                    observer.absorb(&other);
                }
            }
        }
        Ok(observer)
    }

    /// Folds a pushed event into the observer's view of the session, and
    /// reports whether the event was about this session at all.
    ///
    /// The daemon's `Subscribe` stream is machine-wide — every session's status
    /// changes and exits reach every subscriber — so "an event arrived" and
    /// "this session did something" are different facts, and the settle window
    /// below depends on the second one.
    fn absorb(&mut self, event: &DaemonEvent) -> bool {
        match event {
            DaemonEvent::StatusChanged {
                session_id, status, ..
            } if *session_id == self.session_id => {
                self.status = *status;
                self.present = true;
                true
            }
            DaemonEvent::Exit { session_id, .. } if *session_id == self.session_id => {
                self.present = false;
                true
            }
            _ => false,
        }
    }

    /// Waits for the existing settled-idle completion policy.
    async fn wait_for_idle(
        &mut self,
        budget: Duration,
        settle: Duration,
        edge_settle: Duration,
    ) -> IdleOutcome {
        let start = tokio::time::Instant::now();
        let deadline = start + budget;
        let mut silent_since = start;
        let mut idle_edge_seen = false;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return IdleOutcome::TimedOut(self.status);
            }
            let settled_at = silent_since + if idle_edge_seen { edge_settle } else { settle };
            if self.status == SessionStatus::Idle && now >= settled_at {
                return IdleOutcome::Idle;
            }
            let wake = if self.status == SessionStatus::Idle {
                settled_at.min(deadline)
            } else {
                deadline
            };
            match tokio::time::timeout_at(wake, self.reader.read_event()).await {
                Ok(Ok(event)) => {
                    if self.absorb(&event) {
                        silent_since = tokio::time::Instant::now();
                    }
                    match event {
                        DaemonEvent::StatusChanged {
                            ref session_id,
                            status: SessionStatus::Idle,
                            ..
                        } if *session_id == self.session_id => {
                            idle_edge_seen = true;
                        }
                        DaemonEvent::Exit {
                            ref session_id,
                            killed,
                            ..
                        } if *session_id == self.session_id => {
                            return IdleOutcome::Exited { killed };
                        }
                        _ => {}
                    }
                }
                Ok(Err(_)) => return IdleOutcome::TimedOut(self.status),
                Err(_) => continue,
            }
        }
    }

    async fn wait_for_exit(&mut self, budget: Duration) -> ExitOutcome {
        let observe = async {
            loop {
                let Ok(event) = self.reader.read_event().await else {
                    return ExitOutcome::TimedOut;
                };
                self.absorb(&event);
                if let DaemonEvent::Exit {
                    session_id, killed, ..
                } = event
                {
                    if session_id == self.session_id {
                        return ExitOutcome::Exited { killed };
                    }
                }
            }
        };
        tokio::time::timeout(budget, observe)
            .await
            .unwrap_or(ExitOutcome::TimedOut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanna_daemon::protocol::{
        ErrorCode as DaemonErrorCode, SessionInfo, SessionKind, SessionState,
    };
    use std::sync::Mutex;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    const SESSION: &str = "task-finalize";

    /// What the fake daemon saw, in the order it saw it.
    ///
    /// The whole sequence is about ordering — a quit typed before the agent
    /// went idle truncates the wrap-up the transfer exists to capture — so the
    /// assertions are on this transcript, not on the return value.
    #[derive(Debug, Default)]
    struct DaemonLog {
        /// Every fenced logical message accepted for the session, in order.
        inputs: Vec<String>,
        /// How many inputs had arrived when `Idle` was published.
        inputs_at_idle: Option<usize>,
    }

    /// A scripted daemon over a real Unix socket.
    ///
    /// It serves the observer's subscription and the short-lived connections
    /// each injection opens, which is the arrangement the production code
    /// deliberately uses (commands must not be issued on a subscribed
    /// connection, where a pushed event can be mistaken for a response).
    struct FakeDaemon {
        dir: String,
        log: Arc<Mutex<DaemonLog>>,
        events: tokio::sync::broadcast::Sender<DaemonEvent>,
        _accept: tokio::task::JoinHandle<()>,
    }

    /// The directory sits under this process's test root, so an aborted run is
    /// still reclaimed; the socket lives in the shared socket directory and
    /// only this removal takes it back.
    impl Drop for FakeDaemon {
        fn drop(&mut self) {
            let dir = std::path::Path::new(&self.dir);
            let _ = std::fs::remove_file(kanna_runtime_defaults::socket_path(dir));
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    impl FakeDaemon {
        fn start(label: &str, listed: Option<SessionStatus>) -> Self {
            Self::start_refusing(label, listed, None)
        }

        /// A daemon that refuses every `SubmitInput` with one error code, so a
        /// test can pin what finalization does with each refusal.
        fn start_refusing(
            label: &str,
            listed: Option<SessionStatus>,
            submit_refusal: Option<DaemonErrorCode>,
        ) -> Self {
            let dir = crate::test_paths::unique_test_dir(&format!("kanna-finalize-{label}"))
                .to_string_lossy()
                .to_string();
            let socket = kanna_runtime_defaults::socket_path(std::path::Path::new(&dir));
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).expect("bind fake daemon");
            let log = Arc::new(Mutex::new(DaemonLog::default()));
            let (events, _) = tokio::sync::broadcast::channel(64);

            let accept_log = Arc::clone(&log);
            let accept_events = events.clone();
            let accept = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    tokio::spawn(serve(
                        stream,
                        Arc::clone(&accept_log),
                        accept_events.clone(),
                        listed,
                        submit_refusal,
                    ));
                }
            });

            Self {
                dir,
                log,
                events,
                _accept: accept,
            }
        }

        fn publish(&self, event: DaemonEvent) {
            if let DaemonEvent::StatusChanged {
                ref session_id,
                status: SessionStatus::Idle,
                ..
            } = event
            {
                if session_id == SESSION {
                    let mut log = self.log.lock().expect("log");
                    let seen = log.inputs.len();
                    log.inputs_at_idle.get_or_insert(seen);
                }
            }
            let _ = self.events.send(event);
        }

        fn status(&self, status: SessionStatus) {
            self.status_of(SESSION, status);
        }

        /// The daemon publishes every session on the machine to every
        /// subscriber, so a test can put another task's traffic on the wire.
        fn status_of(&self, session_id: &str, status: SessionStatus) {
            self.publish(DaemonEvent::StatusChanged {
                session_id: session_id.to_string(),
                status,
                waiting_prompt_snippet: None,
            });
        }

        fn exit(&self) {
            self.publish(DaemonEvent::Exit {
                session_id: SESSION.to_string(),
                code: 0,
                resume_session_id: None,
                killed: false,
            });
        }

        fn inputs(&self) -> Vec<String> {
            self.log.lock().expect("log").inputs.clone()
        }

        fn inputs_at_idle(&self) -> Option<usize> {
            self.log.lock().expect("log").inputs_at_idle
        }

        /// Blocks until `count` input writes have arrived, so a test never
        /// races the injection it is about to react to.
        async fn wait_for_inputs(&self, count: usize) {
            for _ in 0..600 {
                if self.inputs().len() >= count {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!(
                "fake daemon never received {count} inputs: {:?}",
                self.inputs()
            );
        }
    }

    async fn serve(
        stream: UnixStream,
        log: Arc<Mutex<DaemonLog>>,
        events: tokio::sync::broadcast::Sender<DaemonEvent>,
        listed: Option<SessionStatus>,
        submit_refusal: Option<DaemonErrorCode>,
    ) {
        let (read_half, write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let writer = Arc::new(tokio::sync::Mutex::new(write_half));
        let mut subscription: Option<tokio::task::JoinHandle<()>> = None;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(command) = serde_json::from_str::<DaemonCommand>(line.trim()) else {
                break;
            };
            let response = match command {
                DaemonCommand::Subscribe => {
                    if subscription.is_none() {
                        let mut stream = events.subscribe();
                        let writer = Arc::clone(&writer);
                        subscription = Some(tokio::spawn(async move {
                            while let Ok(event) = stream.recv().await {
                                let line = serde_json::to_string(&event).expect("event");
                                let mut writer = writer.lock().await;
                                if writer.write_all(line.as_bytes()).await.is_err()
                                    || writer.write_all(b"\n").await.is_err()
                                {
                                    return;
                                }
                            }
                        }));
                    }
                    DaemonEvent::Ok
                }
                DaemonCommand::List => DaemonEvent::SessionList {
                    sessions: listed
                        .map(|status| SessionInfo {
                            session_id: SESSION.to_string(),
                            pid: 4242,
                            cwd: "/tmp".to_string(),
                            state: SessionState::Active,
                            idle_seconds: 0,
                            status,
                            status_observed: true,
                            kind: SessionKind::default(),
                            composer_text: None,
                            composer_attestation: Default::default(),
                        })
                        .into_iter()
                        .collect(),
                },
                DaemonCommand::SubmitInputIfSession { expected_pid, .. }
                    if expected_pid != 4242 =>
                {
                    DaemonEvent::Error {
                        code: Some(DaemonErrorCode::SessionIncarnationMismatch),
                        message: "the fake session incarnation changed".to_string(),
                    }
                }
                DaemonCommand::SubmitInputIfSession { data, .. } => match submit_refusal {
                    Some(code) => DaemonEvent::Error {
                        code: Some(code),
                        message: "the fake daemon refused this submission".to_string(),
                    },
                    None => {
                        log.lock()
                            .expect("log")
                            .inputs
                            .push(String::from_utf8_lossy(&data).into_owned());
                        DaemonEvent::Ok
                    }
                },
                DaemonCommand::NegotiateProtectedInput { version } => {
                    DaemonEvent::ProtectedInputReady { version }
                }
                DaemonCommand::Snapshot { .. } => DaemonEvent::Error {
                    code: None,
                    message: "no snapshot in the fake daemon".to_string(),
                },
                _ => DaemonEvent::Ok,
            };
            let line = serde_json::to_string(&response).expect("response");
            let mut writer = writer.lock().await;
            if writer.write_all(line.as_bytes()).await.is_err()
                || writer.write_all(b"\n").await.is_err()
            {
                break;
            }
        }

        // Fidelity, not tidiness. The real daemon aborts its subscription task
        // the moment the command loop ends (`crates/daemon/src/connection.rs`),
        // so a client that lets its write half drop stops receiving events —
        // which is exactly how a subscriber that fails to hold its writer open
        // loses the stream. A fake that keeps publishing to a half-closed
        // connection hides that bug, and did.
        if let Some(task) = subscription {
            task.abort();
        }
    }

    fn work_item() -> TransferWorkItem {
        TransferWorkItem {
            id: "finalize:transfer-1".to_string(),
            kind: super::super::queue::KIND_FINALIZE.to_string(),
            transfer_id: Some("transfer-1".to_string()),
            payload_json: "{}".to_string(),
            attempts: 1,
        }
    }

    fn state_for(daemon: &FakeDaemon, label: &str) -> Arc<AppState> {
        crate::http_api::test_state_with_daemon_dir(label, label, &daemon.dir, |db| {
            db.insert_test_repo("repo-finalize", "Finalize Repo")
                .expect("repo");
            db.insert_test_pipeline_item(
                SESSION,
                "repo-finalize",
                "finalize me",
                None,
                "in progress",
                "2026-08-07 00:00:00",
            )
            .expect("task");
            // The phase claims and the verdict memo hang off this row.
            db.enqueue_transfer_work(&work_item().id, "finalize", None, "{}")
                .expect("queue the finalize work item");
        })
    }

    fn phases(state: &Arc<AppState>) -> Vec<String> {
        let db = open_db(state).expect("db");
        let head = db.latest_task_event_seq().expect("head");
        db.list_task_events(
            &crate::db::TaskEventScope::Tasks(vec![SESSION.into()]),
            0,
            head,
            64,
        )
        .expect("events")
        .into_iter()
        .filter(|event| event.event_type == "task.transfer_finalizing")
        .map(|event| {
            event.payload["phase"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
    }

    /// The existing settled-idle policy keeps `/exit` from preempting a turn.
    #[tokio::test]
    async fn the_quit_command_is_never_typed_while_the_agent_is_busy() {
        let daemon = FakeDaemon::start("busy-then-idle", Some(SessionStatus::Busy));
        let state = state_for(&daemon, "desktop-finalize-busy");

        let sequence = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                finalize_source_session(&state, &work_item(), SESSION, Some("pty"), Some("claude"))
                    .await
            }
        });

        daemon.wait_for_inputs(1).await;
        // Still busy — a quit typed now would truncate the turn.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            daemon.inputs().len(),
            1,
            "something was typed at a busy agent: {:?}",
            daemon.inputs(),
        );

        daemon.status(SessionStatus::Idle);
        daemon.wait_for_inputs(2).await;
        daemon.exit();

        let outcome = sequence.await.expect("sequence");
        assert!(
            outcome.cleanly_finalized(),
            "a sequence that ran to completion reported degraded: {:?}",
            outcome.degraded_reason,
        );

        let inputs = daemon.inputs();
        assert!(
            inputs[0].contains("transferred to another machine"),
            "the first thing typed was not the wrap-up: {inputs:?}",
        );
        assert_eq!(
            inputs[1], "/exit",
            "the quit command was not typed: {inputs:?}"
        );
        assert_eq!(
            daemon.inputs_at_idle(),
            Some(1),
            "the quit was typed before the session was reported idle: {inputs:?}",
        );
        assert_eq!(
            phases(&state),
            vec!["wrap-up-sent", "idle", "quit-sent", "exited"],
            "the transfer's finalization was not observable step by step",
        );
    }

    /// A wrap-up the daemon refused outright releases its phase claim: nothing
    /// reached the terminal, so re-claiming on a retry is both safe and the
    /// only way the wrap-up ever gets typed.
    #[tokio::test]
    async fn a_wrap_up_the_daemon_refused_releases_its_phase_claim_for_a_retry() {
        let daemon = FakeDaemon::start_refusing(
            "input-refused",
            Some(SessionStatus::Idle),
            Some(DaemonErrorCode::InputUnauthorized),
        );
        let state = state_for(&daemon, "desktop-finalize-refused");

        let outcome =
            finalize_source_session(&state, &work_item(), SESSION, Some("pty"), Some("claude"))
                .await;
        assert!(
            !outcome.cleanly_finalized(),
            "a wrap-up that never reached the terminal must degrade finalization: {outcome:?}"
        );

        let db = open_db(&state).expect("db");
        assert!(
            db.claim_transfer_work_phase(&work_item().id, WRAP_UP_PHASE)
                .expect("claim"),
            "nothing was written, so the claim must be available to a retry"
        );
    }

    /// The task id can be rebound to a replacement PTY after observation. The
    /// pid fence keeps the lifecycle command off that replacement, and the
    /// fresh session snapshot keeps the refusal from masquerading as a cleanly
    /// exited source.
    #[tokio::test]
    async fn a_replaced_session_is_fenced_and_degrades_instead_of_looking_exited() {
        let daemon = FakeDaemon::start_refusing(
            "session-replaced",
            Some(SessionStatus::Idle),
            Some(DaemonErrorCode::SessionIncarnationMismatch),
        );
        let state = state_for(&daemon, "desktop-finalize-session-replaced");

        let outcome = run_sequence(&state, &work_item(), SESSION, Some("claude")).await;

        assert!(daemon.inputs().is_empty(), "replacement PTY received input");
        let reason = outcome
            .degraded_reason
            .expect("a replaced live session reported clean finalization");
        assert!(reason.contains("session changed"), "{reason}");
        assert_eq!(phases(&state), vec!["degraded"]);
    }

    /// The quit command uses the same incarnation fence as preparation; a
    /// replacement cannot receive a lifecycle command from the old run.
    #[tokio::test]
    async fn a_replaced_session_is_fenced_for_quit_too() {
        let daemon = FakeDaemon::start("quit-session-replaced", Some(SessionStatus::Idle));
        let state = state_for(&daemon, "desktop-finalize-quit-session-replaced");

        let result = inject(&state, &work_item(), SESSION, 7, QUIT_PHASE, "/exit").await;

        assert!(matches!(result, Injected::SessionGone));
        assert!(daemon.inputs().is_empty(), "replacement PTY received quit");
    }

    /// A crash can leave the at-most-once phase claimed before any daemon
    /// acknowledgement was recorded. That prevents a blind resend, but is not
    /// evidence that preparation was submitted and cannot release a quit.
    #[tokio::test]
    async fn a_preclaimed_wrap_up_is_unknown_not_submitted() {
        let daemon = FakeDaemon::start("preclaimed", Some(SessionStatus::Idle));
        let state = state_for(&daemon, "desktop-finalize-preclaimed");
        open_db(&state)
            .expect("db")
            .claim_transfer_work_phase(&work_item().id, WRAP_UP_PHASE)
            .expect("claim preparation phase");

        let outcome = run_sequence(&state, &work_item(), SESSION, Some("claude")).await;

        assert!(daemon.inputs().is_empty(), "a claimed phase was resent");
        let reason = outcome
            .degraded_reason
            .expect("an unproved phase claim reported clean finalization");
        assert!(reason.contains("no quit command was sent"), "{reason}");
    }

    #[tokio::test]
    async fn an_absent_session_after_an_ambiguous_claim_degrades() {
        let daemon = FakeDaemon::start("absent-after-claim", None);
        let state = state_for(&daemon, "desktop-finalize-absent-after-claim");
        open_db(&state)
            .expect("db")
            .claim_transfer_work_phase(&work_item().id, WRAP_UP_PHASE)
            .expect("claim preparation phase");

        let outcome = run_sequence(&state, &work_item(), SESSION, Some("claude")).await;
        let reason = outcome
            .degraded_reason
            .expect("ambiguous finalization was upgraded to clean");
        assert!(reason.contains("disappeared"), "{reason}");
        assert!(phases(&state).contains(&"degraded".to_string()));
    }

    /// Losing the daemon response after a write is also not success. The phase
    /// stays claimed so recovery cannot duplicate it, while the quit remains
    /// fenced behind proof that this delivery completed.
    #[tokio::test]
    async fn an_uncertain_wrap_up_keeps_its_claim_and_never_sends_quit() {
        let daemon = FakeDaemon::start_refusing(
            "uncertain",
            Some(SessionStatus::Idle),
            Some(DaemonErrorCode::WriteFailed),
        );
        let state = state_for(&daemon, "desktop-finalize-uncertain");

        let outcome = run_sequence(&state, &work_item(), SESSION, Some("claude")).await;

        assert!(!outcome.cleanly_finalized());
        assert!(
            daemon.inputs().is_empty(),
            "a quit followed uncertain input"
        );
        assert!(
            !open_db(&state)
                .expect("db")
                .claim_transfer_work_phase(&work_item().id, WRAP_UP_PHASE)
                .expect("claim state"),
            "uncertain delivery was released for a blind resend"
        );
    }

    /// Codex names its quit command differently, and finalization reads it off
    /// the provider registry rather than hard-coding one command.
    #[tokio::test]
    async fn the_quit_command_comes_from_the_task_s_provider() {
        let daemon = FakeDaemon::start("codex-quit", Some(SessionStatus::Idle));
        let state = state_for(&daemon, "desktop-finalize-codex");

        let sequence = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                finalize_source_session(&state, &work_item(), SESSION, Some("pty"), Some("codex"))
                    .await
            }
        });

        daemon.wait_for_inputs(1).await;
        daemon.status(SessionStatus::Busy);
        daemon.status(SessionStatus::Idle);
        daemon.wait_for_inputs(2).await;
        daemon.exit();
        sequence.await.expect("sequence");

        assert_eq!(daemon.inputs()[1], "/quit");
    }

    /// `/exit` belongs to four providers and therefore cannot identify a task's
    /// provider. A missing or future provider value degrades without guessing a
    /// command or changing the source session.
    #[tokio::test]
    async fn an_unknown_provider_is_never_guessed_from_a_quit_command() {
        let daemon = FakeDaemon::start("unknown-provider", Some(SessionStatus::Idle));
        let state = state_for(&daemon, "desktop-finalize-unknown-provider");

        let outcome = run_sequence(
            &state,
            &work_item(),
            SESSION,
            Some("provider-from-a-newer-server"),
        )
        .await;

        assert!(daemon.inputs().is_empty());
        let reason = outcome
            .degraded_reason
            .expect("an unknown provider reported clean finalization");
        assert!(reason.contains("no quit command can be chosen safely"));
    }

    /// A task whose agent already stopped has nothing to wrap up: the
    /// conversation on disk is already whole. Typing into a session that is not
    /// there would only produce a spurious failure — the old `SIGINT` path
    /// degraded exactly this case, because signalling a missing session errors.
    #[tokio::test]
    async fn a_session_that_has_already_exited_is_finalized_without_typing_into_it() {
        let daemon = FakeDaemon::start("already-gone", None);
        let state = state_for(&daemon, "desktop-finalize-gone");

        let outcome =
            finalize_source_session(&state, &work_item(), SESSION, Some("pty"), Some("claude"))
                .await;

        assert!(outcome.cleanly_finalized(), "{:?}", outcome.degraded_reason);
        assert!(daemon.inputs().is_empty(), "{:?}", daemon.inputs());
        assert_eq!(phases(&state), vec!["already-exited"]);
    }

    /// A headless session has no TUI to type into, so the sequence does not run
    /// at all — and must not degrade the transfer for not running.
    #[tokio::test]
    async fn a_headless_session_is_left_alone() {
        let daemon = FakeDaemon::start("headless", Some(SessionStatus::Busy));
        let state = state_for(&daemon, "desktop-finalize-headless");

        let outcome =
            finalize_source_session(&state, &work_item(), SESSION, Some("agent"), Some("claude"))
                .await;

        assert!(outcome.cleanly_finalized());
        assert!(daemon.inputs().is_empty());
    }

    /// The verdict is the first attempt's, not the retry's.
    ///
    /// A retry looks at a machine the first attempt already changed: the agent
    /// is gone, which reads identically to "it exited cleanly". Recomputing
    /// would quietly upgrade a degraded finalization to a clean one and tell
    /// the destination the conversation is whole when it is not.
    #[tokio::test]
    async fn a_retry_reports_the_verdict_the_live_attempt_reached() {
        let daemon = FakeDaemon::start("verdict-memo", None);
        let state = state_for(&daemon, "desktop-finalize-verdict");
        let work = work_item();
        open_db(&state)
            .expect("db")
            .record_transfer_work_observation(&work.id, OUTCOME_PHASE, Some("the agent hung"))
            .expect("attempt 1's verdict");

        let outcome =
            finalize_source_session(&state, &work, SESSION, Some("pty"), Some("claude")).await;

        assert_eq!(outcome.degraded_reason.as_deref(), Some("the agent hung"));
        assert!(
            daemon.inputs().is_empty(),
            "the retry typed at the agent again"
        );
    }

    async fn observer_for(daemon: &FakeDaemon) -> SessionObserver {
        SessionObserver::attach(&daemon.dir, SESSION)
            .await
            .expect("attach")
    }

    /// `Waiting` is a permission prompt, not idleness. Typing the quit command
    /// into one would answer it on the operator's behalf, so the sequence never
    /// treats it as the go-ahead.
    #[tokio::test]
    async fn a_session_parked_on_a_permission_prompt_never_reads_as_idle() {
        let daemon = FakeDaemon::start("waiting-prompt", Some(SessionStatus::Busy));
        let mut observer = observer_for(&daemon).await;
        daemon.status(SessionStatus::Waiting);

        let outcome = observer
            .wait_for_idle(
                Duration::from_millis(400),
                Duration::from_millis(20),
                Duration::from_millis(20),
            )
            .await;

        assert!(
            matches!(outcome, IdleOutcome::TimedOut(SessionStatus::Waiting)),
            "a permission prompt was mistaken for a finished turn",
        );
    }

    /// …and reading it correctly is not enough on its own: a session already
    /// parked when finalization starts must be left completely alone.
    ///
    /// The submission policy ends every message with a discrete CR, which is
    /// the keystroke that accepts a permission prompt's highlighted option — so
    /// typing the *wrap-up* at a parked session approves whatever tool call it
    /// is holding, in the operator's name. Worse, it does so invisibly: the
    /// agent resumes, goes idle, quits on cue, and the payload ships
    /// `cleanlyFinalized: true` with nothing recording the approval. Pushing a
    /// task that is parked on a prompt is a normal thing to do, so this is the
    /// ordinary case, not an exotic one.
    #[tokio::test]
    async fn nothing_is_typed_at_a_session_already_parked_on_a_permission_prompt() {
        let daemon = FakeDaemon::start("waiting-at-start", Some(SessionStatus::Waiting));
        let state = state_for(&daemon, "desktop-finalize-waiting");

        let outcome =
            finalize_source_session(&state, &work_item(), SESSION, Some("pty"), Some("claude"))
                .await;

        assert!(
            daemon.inputs().is_empty(),
            "the transfer typed at a permission prompt: {:?}",
            daemon.inputs(),
        );
        let reason = outcome
            .degraded_reason
            .expect("a finalization that never asked the agent anything reported itself clean");
        assert!(
            reason.contains("permission prompt"),
            "the degradation does not say the prompt is why: {reason}",
        );
        assert_eq!(phases(&state), vec!["degraded"]);
    }

    /// The destination waits out this whole sequence over a single peer
    /// request, so the sequence has to fit inside what that request allows.
    ///
    /// When it did not, a wrap-up longer than the sidecar's ordinary 15 s
    /// window surfaced on the destination as `PeerRequestTimeout` and spent one
    /// of `MAX_TRANSFER_WORK_ATTEMPTS` on an import that was going fine — the
    /// budget reserved for a locked OpenCode store or a dropped artifact fetch,
    /// not for waiting. The transfer still completed off the cached
    /// finalization result, so nothing failed loudly; the cost was invisible.
    ///
    /// The two ends of the request are enforced in crates that do not depend on
    /// each other (`kanna-server` reaches the sidecar over stdio), so the window
    /// is read from `kanna-runtime-defaults`, which both already depend on,
    /// rather than restated here. That makes this one assertion guard both
    /// directions: raising the budget past the window fails it, and so does
    /// shrinking the window under the budget — which a hand-copied window could
    /// not catch. The behaviour itself is pinned where both ends of the request
    /// exist, in `crates/task-transfer/tests/runtime.rs`.
    #[test]
    fn the_shutdown_budget_fits_inside_the_peer_finalization_window() {
        let window = kanna_runtime_defaults::TRANSFER_FINALIZATION_REQUEST_TIMEOUT;
        let shutdown = WRAP_UP_TIMEOUT + QUIT_EXIT_TIMEOUT;
        assert!(
            shutdown < window,
            "the shutdown budget ({}s) no longer fits inside the sidecar's finalization window \
             ({}s); raise TRANSFER_FINALIZATION_REQUEST_TIMEOUT in \
             crates/runtime-defaults/src/lib.rs first, or the destination will time out \
             mid-wrap-up and spend an import attempt on it",
            shutdown.as_secs(),
            window.as_secs(),
        );
        // Staging runs after the sequence and inside the same request: gzipping
        // a session archive and reading a rollout are not instant on a large
        // conversation, so the fit has to leave room rather than merely hold.
        assert!(
            window - shutdown >= Duration::from_secs(120),
            "no room left in the finalization window for staging the session artifacts",
        );
    }
}

/// Cross-process regression coverage for [`finalize_source_session`] against a
/// real `kanna-daemon` executable and a real PTY child, not the scripted
/// in-process [`tests::FakeDaemon`] above.
///
/// The fake daemon proves finalization's *ordering* logic. It cannot prove the
/// two things this file exists for: that the CR/paste-framed bytes the
/// production write path constructs actually reach a real terminal's child
/// process, and that a real PID fence, a real forced exit, and a real
/// permission-prompt frame drive the same verdicts end to end.
/// `kanna-daemon`'s own `authorize_spawn` accepts a connection from this test
/// binary without any negotiation dance because this test binary is that
/// daemon's live direct parent — the same trust the desktop app gets, and
/// the same reason `crates/daemon/tests/reconnect.rs` and
/// `detection_rules.rs` can send `Spawn` directly.
///
/// **Scope this module does not claim.** A negotiated pty is a byte pipe with
/// a line discipline in front of it, not a transparent wire: the child's
/// shell `read` builtin observes whatever that discipline hands it after
/// canonical-mode processing, not the raw bytes the daemon wrote. In
/// particular the trailing CR every write ends with is consumed as `read`'s
/// line terminator, not captured as a literal byte in what a test asserts on
/// — what these tests can and do prove about it is that exactly one shell
/// line was produced per logical write, not that a specific `\r` byte
/// survived untouched. And the child's printed frames are synthetic patterns
/// built to match this repo's own bundled Codex detection rules
/// (`crates/daemon/src/detection/rules.json`) closely enough to drive the
/// real classifier the way a real Codex session's screen would — they are
/// not a claim that an installed Codex or Claude CLI parses these bytes the
/// same way. That is `tests/cli-contract`'s job and stays out of this file.
///
/// The child is a `/bin/sh -c` script using only POSIX builtins (`printf`,
/// `read`, `sleep`, `exit`) so it never depends on `PATH` or anything the
/// daemon's environment override might drop. It is deliberately built on the
/// `codex` provider: `codex/idle/composer` classifies off one textual rule
/// (`lastNonEmptyLine` starts with `›`) and, unlike Claude,
/// `allows_output_triggered_idle` lets a Codex session publish `Idle` the
/// moment that frame is observed rather than waiting out a quiet-refresh
/// timer (`crates/daemon/src/session.rs`) — which is what keeps these tests
/// bounded in real time instead of racing a multi-second heuristic.
///
/// **Proving a negative.** A script that never reads its own stdin (a bare
/// `sleep`, or a one-shot `read` that already returned) cannot tell this test
/// anything: an empty log file next to it proves only that the script never
/// logs, not that nothing reached the PTY. Every test that asserts an absence
/// therefore gives its child a loop that keeps reading and logging every
/// line for the test's whole life, and follows its negative assertion with
/// [`assert_only_control_probe_was_received`] — a real delivery, through the
/// same production `try_submit_task_input_if_session` path, sent after the
/// code under test has already run and waited for deterministically. Seeing
/// the probe's own line, and only the probe's own line, in the log is what
/// proves both that the reader was alive the whole time and that nothing
/// else arrived before it.
///
/// No production code changes with this module: it drives
/// `finalize_source_session`, `run_sequence` and `inject` exactly as
/// `tests::FakeDaemon` does, over the real `crate::daemon_client::DaemonClient`
/// production code already uses.
#[cfg(test)]
mod real_daemon_tests {
    use super::*;
    use kanna_daemon::protocol::SessionInfo;
    use std::collections::HashMap;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command as StdCommand};
    use std::time::Instant;

    const SESSION: &str = "task-finalize-real";

    /// Finds the compiled `kanna-daemon` executable this test can spawn.
    ///
    /// `kanna-daemon` is an ordinary `path` dependency of this crate
    /// (`crates/kanna-server/Cargo.toml`), not a `[[bin]]` artifact
    /// dependency, so Cargo never populates `CARGO_BIN_EXE_kanna-daemon` for
    /// this crate's own test binaries the way it does inside
    /// `crates/daemon/tests/*.rs`, which belong to the daemon's own package.
    /// `KANNA_DAEMON_TEST_BIN` is the explicit override for a caller that
    /// built the daemon somewhere non-standard; otherwise this locates the
    /// binary the same way Cargo already laid it out. Measured directly
    /// against this workspace's actual `.cargo/config.toml`: `build-dir`
    /// (`.build/cargo-build`) and `target-dir` (`.build`) are split, so this
    /// test binary itself compiles under
    /// `.build/cargo-build/<profile>/deps/<this test>` while a named
    /// `[[bin]]` like `kanna-daemon` is copied to `.build/<profile>/` --
    /// *not* to a sibling of this test binary's own directory. The profile
    /// name (the directory that holds `deps/`) is the one thing shared by
    /// both layouts, so it locates `kanna-daemon` under the workspace's
    /// fixed `target-dir` rather than by walking up from wherever the test
    /// harness happened to land. The plain sibling-of-this-binary layout is
    /// kept as a fallback in case `build-dir` is ever unset.
    fn resolve_daemon_binary() -> PathBuf {
        if let Ok(path) = std::env::var("KANNA_DAEMON_TEST_BIN") {
            let path = PathBuf::from(path);
            assert!(
                path.is_file(),
                "KANNA_DAEMON_TEST_BIN does not name a file: {path:?}"
            );
            return path;
        }
        let exe = std::env::current_exe().expect("this test binary's own path");
        let profile_dir = exe
            .parent()
            .and_then(Path::parent)
            .expect("test binary has a profile directory two levels up from itself");
        let mut candidates = Vec::new();
        if let Some(profile) = profile_dir.file_name() {
            let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(Path::parent)
                .expect("crates/kanna-server has a workspace root two levels up");
            candidates.push(repo_root.join(".build").join(profile).join("kanna-daemon"));
        }
        candidates.push(profile_dir.join("kanna-daemon"));
        candidates
            .into_iter()
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| {
                panic!(
                    "kanna-daemon binary not found next to this test binary ({exe:?}); build it \
                     first with `cargo build -p kanna-daemon` (it shares this workspace's \
                     target-dir with kanna-server), or set KANNA_DAEMON_TEST_BIN to an \
                     already-built binary's path"
                )
            })
    }

    /// A real `kanna-daemon` child process, listening on its own socket
    /// directory. Never scripted: every reply in these tests came from the
    /// daemon actually running the command.
    struct RealDaemon {
        child: Child,
        dir: PathBuf,
    }

    impl Drop for RealDaemon {
        fn drop(&mut self) {
            // Best-effort: a test that already asserted on the daemon's own
            // exit, or one whose child process outlived an assertion failure,
            // must not panic again on the way out.
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(kanna_runtime_defaults::socket_path(&self.dir));
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl RealDaemon {
        /// `label` only has to be unique within one test; `crate::test_paths`
        /// already makes the directory unique across every concurrent test
        /// and every concurrent worktree's gate on this machine.
        fn start(label: &str) -> Self {
            let dir = crate::test_paths::unique_test_dir(&format!("kanna-finalize-real-{label}"));
            let socket_path = kanna_runtime_defaults::socket_path(&dir);
            let _ = std::fs::remove_file(&socket_path);
            let pid_path = dir.join("daemon.pid");
            let _ = std::fs::remove_file(&pid_path);

            let mut command = StdCommand::new(resolve_daemon_binary());
            command.env("KANNA_DAEMON_DIR", dir.to_str().expect("utf-8 daemon dir"));
            let child = command
                .spawn()
                .expect("failed to start a real kanna-daemon");
            // Own the child in the RAII guard *before* the readiness wait
            // below, not after: `Child`'s own `Drop` does not kill the
            // process, so a timeout panic here would otherwise leak a real
            // daemon process that nothing ever reaps.
            let daemon = Self { child, dir };

            for _ in 0..100 {
                let pid_matches = std::fs::read_to_string(&pid_path)
                    .ok()
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
                    == Some(daemon.child.id());
                if pid_matches && UnixStream::connect(&socket_path).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(
                std::fs::read_to_string(&pid_path)
                    .ok()
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
                    == Some(daemon.child.id())
                    && UnixStream::connect(&socket_path).is_ok(),
                "real daemon was not ready at {socket_path:?}"
            );

            daemon
        }

        fn dir_str(&self) -> String {
            self.dir.to_string_lossy().to_string()
        }

        async fn connect(&self) -> crate::daemon_client::DaemonClient {
            crate::daemon_client::DaemonClient::connect(&self.dir_str())
                .await
                .expect("connect to the real daemon")
        }
    }

    async fn list_sessions(client: &mut crate::daemon_client::DaemonClient) -> Vec<SessionInfo> {
        match client
            .send_command(&DaemonCommand::List)
            .await
            .expect("List round trip against the real daemon")
        {
            DaemonEvent::SessionList { sessions } => sessions,
            other => panic!("unexpected reply to List: {other:?}"),
        }
    }

    async fn wait_for_status(
        client: &mut crate::daemon_client::DaemonClient,
        session_id: &str,
        expected: SessionStatus,
        timeout: Duration,
    ) {
        let deadline = Instant::now() + timeout;
        loop {
            let sessions = list_sessions(client).await;
            if let Some(session) = sessions
                .iter()
                .find(|session| session.session_id == session_id)
            {
                if session.status == expected {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "session {session_id} never reached {expected:?} on the real daemon"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Spawns the controlled PTY child for `session_id` and returns the real
    /// PID the daemon reports for it, straight off `List` — never invented,
    /// which is what makes the PID-fence tests below a real proof rather than
    /// a restatement of a constant.
    async fn spawn_pty_session(
        daemon: &RealDaemon,
        session_id: &str,
        script: &str,
        log_path: &Path,
    ) -> u32 {
        let mut client = daemon.connect().await;
        let mut env = HashMap::new();
        env.insert(
            "KANNA_TEST_LOG".to_string(),
            log_path.to_string_lossy().to_string(),
        );
        let command = DaemonCommand::Spawn {
            session_id: session_id.to_string(),
            executable: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: "/tmp".to_string(),
            env,
            cols: 120,
            rows: 40,
            agent_provider: Some(AgentProvider::Codex),
            agent_executable: None,
            terminal_prelude: None,
            operator_input_only: false,
        };
        match client
            .send_command(&command)
            .await
            .expect("Spawn round trip against the real daemon")
        {
            DaemonEvent::SessionCreated {
                session_id: created,
            } => assert_eq!(created, session_id),
            other => panic!("unexpected reply to Spawn: {other:?}"),
        }
        list_sessions(&mut client)
            .await
            .into_iter()
            .find(|session| session.session_id == session_id)
            .expect("the just-spawned session is listed")
            .pid
    }

    /// What the real child actually consumed, one logical message per line, in
    /// the order it read them off its own stdin -- proof of delivery that
    /// crossed a real PTY, not a scripted acknowledgement.
    async fn wait_for_log_lines(path: &Path, count: usize, timeout: Duration) -> Vec<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let lines: Vec<String> = std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect();
            if lines.len() >= count {
                return lines;
            }
            assert!(
                Instant::now() < deadline,
                "the real child never logged {count} consumed line(s); saw {lines:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn work_item() -> TransferWorkItem {
        TransferWorkItem {
            id: "finalize:transfer-real-1".to_string(),
            kind: super::super::queue::KIND_FINALIZE.to_string(),
            transfer_id: Some("transfer-real-1".to_string()),
            payload_json: "{}".to_string(),
            attempts: 1,
        }
    }

    fn state_for(daemon: &RealDaemon, label: &str) -> Arc<AppState> {
        let daemon_dir = daemon.dir_str();
        crate::http_api::test_state_with_daemon_dir(label, label, &daemon_dir, |db| {
            db.insert_test_repo("repo-finalize-real", "Finalize Real Repo")
                .expect("repo");
            db.insert_test_pipeline_item(
                SESSION,
                "repo-finalize-real",
                "finalize me for real",
                None,
                "in progress",
                "2026-09-09 00:00:00",
            )
            .expect("task");
            db.enqueue_transfer_work(&work_item().id, "finalize", None, "{}")
                .expect("queue the finalize work item");
        })
    }

    fn phases(state: &Arc<AppState>) -> Vec<String> {
        let db = open_db(state).expect("db");
        let head = db.latest_task_event_seq().expect("head");
        db.list_task_events(
            &crate::db::TaskEventScope::Tasks(vec![SESSION.into()]),
            0,
            head,
            64,
        )
        .expect("events")
        .into_iter()
        .filter(|event| event.event_type == "task.transfer_finalizing")
        .map(|event| {
            event.payload["phase"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
    }

    async fn kill_session_best_effort(daemon: &RealDaemon, session_id: &str) {
        let mut client = daemon.connect().await;
        let _ = client
            .send_command(&DaemonCommand::Kill {
                session_id: session_id.to_string(),
            })
            .await;
    }

    /// The deterministic barrier a negative assertion needs: sends one known
    /// control message through the exact same production
    /// `try_submit_task_input_if_session` path the finalizer itself uses,
    /// fenced to `real_pid`, then waits for that one line to appear in the
    /// log and asserts it is the *only* line there.
    ///
    /// This only proves anything against a child whose script is a loop that
    /// keeps reading and logging every line for the test's whole life --
    /// against a `sleep`-only or already-returned one-shot `read` script, the
    /// probe itself would never be logged either, and this call would hang
    /// until its own timeout rather than silently pass. Waiting for the
    /// probe's own line is the barrier: a single reader drains the PTY's
    /// bytes in the order they arrived, so anything the code under test
    /// wrongly wrote earlier is already in the file by the time the probe's
    /// line shows up.
    async fn assert_only_control_probe_was_received(
        daemon: &RealDaemon,
        real_pid: u32,
        log_path: &Path,
    ) {
        const PROBE: &str = "CONTROL-PROBE-ONLY-LINE";
        let mut client = daemon.connect().await;
        // `TaskInputError` carries no `Debug` impl (production code only ever
        // matches its variants), so this reports failure without formatting it.
        if try_submit_task_input_if_session(&mut client, SESSION, real_pid, PROBE)
            .await
            .is_err()
        {
            panic!("the control probe itself must reach the real, still-live session");
        }
        let lines = wait_for_log_lines(log_path, 1, Duration::from_secs(10)).await;
        assert_eq!(
            lines,
            vec![PROBE.to_string()],
            "the real child's reader was alive (the control probe proves that) but the log held \
             more than just the probe -- something else reached the real PTY first: {lines:?}",
        );
    }

    /// The whole point of this file: a fast, legitimate preparation turn that
    /// never shows an "esc to interrupt" busy frame at all -- the session's
    /// only observed status is the `Idle` composer it reaches right after
    /// reading the wrap-up -- still reaches a real, separate quit and a real,
    /// voluntary process exit. `finalize_source_session` never required Busy;
    /// this is the real-daemon proof of that, not a restatement of the fake
    /// test with the same name.
    ///
    /// It is also the one test in this file that proves the paste framing
    /// itself: `WRAP_UP_MESSAGE` is 277 bytes with no embedded newline, over
    /// `PASTE_FRAMING_MIN_LEN` (256), so once the child's real
    /// `\x1b[?2004h` has been parsed by the daemon's real terminal emulator,
    /// the production write path must wrap it in `\x1b[200~` / `\x1b[201~`
    /// before the trailing CR. What the log then holds is what the child's
    /// `read -r` actually assembled: the literal paste markers survive
    /// untouched (they are ordinary bytes to the pty's line discipline), but
    /// the trailing CR itself is consumed as `read`'s line terminator, not
    /// captured as a byte -- so this proves one shell line was produced per
    /// logical write and that the paste markers travelled with it, not that
    /// a specific `\r` byte was seen on the other side.
    #[tokio::test]
    async fn fast_preparation_without_busy_is_paste_framed_and_reaches_a_clean_quit() {
        let daemon = RealDaemon::start("fast-idle");
        let log_path = daemon.dir.join("child-consumed.log");
        let script = "\
printf '\\033[?2004h'
IFS= read -r prep
printf '%s\\n' \"$prep\" >> \"$KANNA_TEST_LOG\"
printf '\\r\\nUnderstood, wrapping up now.\\r\\n'
printf '\\r\\n\\342\\200\\272 \\r\\n'
IFS= read -r quit
printf '%s\\n' \"$quit\" >> \"$KANNA_TEST_LOG\"
exit 0
";
        spawn_pty_session(&daemon, SESSION, script, &log_path).await;
        // Real VT state, not something this test can force synchronously: give
        // the daemon's terminal emulator time to have actually parsed the
        // bracketed-paste DECSET the child just wrote before the wrap-up below
        // is constructed.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let state = state_for(&daemon, "desktop-finalize-real-fast");
        let outcome =
            finalize_source_session(&state, &work_item(), SESSION, Some("pty"), Some("codex"))
                .await;

        assert!(
            outcome.cleanly_finalized(),
            "a real fast turn with no observed Busy chrome reported degraded: {:?}",
            outcome.degraded_reason,
        );

        let lines = wait_for_log_lines(&log_path, 2, Duration::from_secs(10)).await;
        assert!(
            lines[0].starts_with("\u{1b}[200~") && lines[0].ends_with("\u{1b}[201~"),
            "the wrap-up did not reach the real child paste-framed: {:?}",
            lines[0],
        );
        assert!(
            lines[0].contains("transferred to another machine"),
            "the framed wrap-up lost its text crossing the real PTY: {:?}",
            lines[0],
        );
        assert_eq!(
            lines[1], "/quit",
            "the quit command the real child actually consumed was not exactly /quit \
             (also proving the short command was NOT paste-framed): {:?}",
            lines[1],
        );
        assert_eq!(
            phases(&state),
            vec!["wrap-up-sent", "idle", "quit-sent", "exited"],
        );
    }

    /// A session already parked on a real permission-prompt frame -- matched
    /// by the daemon's own bundled `common/waiting/permission-prompt` rule,
    /// not a fabricated status -- must never be typed into. The child is a
    /// loop that logs every line it ever reads, so the negative assertion
    /// below is backed by [`assert_only_control_probe_was_received`] rather
    /// than an empty log file a non-reading script could never have falsified.
    #[tokio::test]
    async fn a_real_permission_prompt_is_never_typed_into() {
        let daemon = RealDaemon::start("real-waiting");
        let log_path = daemon.dir.join("child-consumed.log");
        let script = "\
printf '\\033[?2004h'
printf '\\r\\ndo you want to allow this command to run?\\r\\n'
while IFS= read -r line; do
  printf '%s\\n' \"$line\" >> \"$KANNA_TEST_LOG\"
done
";
        let real_pid = spawn_pty_session(&daemon, SESSION, script, &log_path).await;

        {
            let mut client = daemon.connect().await;
            wait_for_status(
                &mut client,
                SESSION,
                SessionStatus::Waiting,
                Duration::from_secs(10),
            )
            .await;
        }

        let state = state_for(&daemon, "desktop-finalize-real-waiting");
        let outcome =
            finalize_source_session(&state, &work_item(), SESSION, Some("pty"), Some("codex"))
                .await;

        assert!(!outcome.cleanly_finalized());
        let reason = outcome
            .degraded_reason
            .expect("a session parked on a real permission prompt reported clean finalization");
        assert!(reason.contains("permission prompt"), "{reason}");
        assert_eq!(phases(&state), vec!["degraded"]);

        assert_only_control_probe_was_received(&daemon, real_pid, &log_path).await;
        kill_session_best_effort(&daemon, SESSION).await;
    }

    /// `inject` fences every lifecycle write to the PID `SessionObserver`
    /// observed. Calling it directly with a PID that is not this real
    /// session's -- the shape a same-id replacement leaves behind -- proves
    /// the real daemon actually enforces `SessionIncarnationMismatch` on
    /// `SubmitInputIfSession`, not merely that `inject`'s match arms compile.
    /// The child is a loop reader so [`assert_only_control_probe_was_received`]
    /// can back the negative assertion with a real, ordered proof rather than
    /// an empty file.
    #[tokio::test]
    async fn a_stale_pid_is_fenced_by_the_real_daemon() {
        let daemon = RealDaemon::start("real-pid-fence");
        let log_path = daemon.dir.join("child-consumed.log");
        let script = "\
printf '\\033[?2004h'
while IFS= read -r line; do
  printf '%s\\n' \"$line\" >> \"$KANNA_TEST_LOG\"
done
";
        let real_pid = spawn_pty_session(&daemon, SESSION, script, &log_path).await;

        let state = state_for(&daemon, "desktop-finalize-real-pid-fence");
        let stale_pid = real_pid.wrapping_add(1);
        let result = inject(
            &state,
            &work_item(),
            SESSION,
            stale_pid,
            QUIT_PHASE,
            "/exit",
        )
        .await;

        assert!(
            matches!(result, Injected::SessionGone),
            "a stale pid was not fenced against the real session"
        );

        assert_only_control_probe_was_received(&daemon, real_pid, &log_path).await;
        kill_session_best_effort(&daemon, SESSION).await;
    }

    /// No session at all -- the real daemon's own `List` legitimately reports
    /// it absent, not a fake `listed: None`. Nothing to wrap up: the
    /// conversation on disk is already whole.
    #[tokio::test]
    async fn an_absent_real_session_finalizes_clean_without_typing_into_anything() {
        let daemon = RealDaemon::start("real-absent");
        let state = state_for(&daemon, "desktop-finalize-real-absent");

        let outcome =
            finalize_source_session(&state, &work_item(), SESSION, Some("pty"), Some("codex"))
                .await;

        assert!(outcome.cleanly_finalized(), "{:?}", outcome.degraded_reason);
        assert_eq!(phases(&state), vec!["already-exited"]);
    }

    /// `face01227` fixed a swallowed-error defect in exactly this branch: a DB
    /// failure while checking ambiguous phase history after a session
    /// disappeared used to be discarded by `.ok()`, so `ambiguous_phase` read
    /// as `None` and finalization reported clean even though the read never
    /// actually proved anything -- silently permitting an unsafe retry after
    /// a crash the DB itself could no longer attest to. This corrupts the
    /// real sqlite file backing a live `AppState` so the failure the fix
    /// handles is a real one, not an injected mock error.
    #[tokio::test]
    async fn a_real_db_read_error_after_disappearance_degrades_rather_than_reading_clean() {
        let daemon = RealDaemon::start("real-db-error");
        let state = state_for(&daemon, "desktop-finalize-real-db-error");

        // The session is absent (never spawned) -- the shape the swallowed
        // error used to hide. Pull the schema out from under the connection
        // `open_db` is about to make, so the ambiguous-phase read this branch
        // performs fails for real.
        let db_path = state.config().db_path.clone();
        std::fs::write(&db_path, b"not a sqlite database").expect("corrupt the real db file");

        let outcome =
            finalize_source_session(&state, &work_item(), SESSION, Some("pty"), Some("codex"))
                .await;

        assert!(
            !outcome.cleanly_finalized(),
            "a real DB read failure after disappearance was swallowed into a clean finalization",
        );
        let reason = outcome
            .degraded_reason
            .expect("a real DB read failure reported clean finalization");
        assert!(
            reason.contains("after session disappearance"),
            "the degraded reason does not name the read failure: {reason}",
        );
    }

    /// A crash can leave `WRAP_UP_PHASE` claimed with no durable delivery
    /// outcome. Against a real, live, present session this proves the claim
    /// is checked before the daemon connection for submission is ever opened:
    /// nothing reaches the real PTY, because `inject` never gets that far. The
    /// child is a loop reader so [`assert_only_control_probe_was_received`]
    /// can back that with a real, ordered proof rather than an empty file a
    /// `sleep`-only script could never have falsified.
    #[tokio::test]
    async fn a_preclaimed_wrap_up_against_a_real_session_never_touches_the_real_pty() {
        let daemon = RealDaemon::start("real-preclaimed");
        let log_path = daemon.dir.join("child-consumed.log");
        let script = "\
printf '\\033[?2004h'
while IFS= read -r line; do
  printf '%s\\n' \"$line\" >> \"$KANNA_TEST_LOG\"
done
";
        let real_pid = spawn_pty_session(&daemon, SESSION, script, &log_path).await;

        let state = state_for(&daemon, "desktop-finalize-real-preclaimed");
        open_db(&state)
            .expect("db")
            .claim_transfer_work_phase(&work_item().id, WRAP_UP_PHASE)
            .expect("claim preparation phase");

        let outcome = run_sequence(&state, &work_item(), SESSION, Some("codex")).await;

        let reason = outcome
            .degraded_reason
            .expect("an unproved phase claim reported clean finalization");
        assert!(reason.contains("no quit command was sent"), "{reason}");

        assert_only_control_probe_was_received(&daemon, real_pid, &log_path).await;
        kill_session_best_effort(&daemon, SESSION).await;
    }

    /// The agent acknowledges the quit command but does not actually exit --
    /// finalization's own `wait_for_exit` budget is 60s, so this test instead
    /// forces the real daemon to `Kill` the real child the moment it observes
    /// the quit line landed, and asserts on the `killed: true` `Exit` that
    /// real kill produces. No process outside this test's own fixture is
    /// touched.
    #[tokio::test]
    async fn a_real_forced_kill_after_quit_is_recorded_as_a_degraded_finalization() {
        let daemon = RealDaemon::start("real-forced-exit");
        let log_path = daemon.dir.join("child-consumed.log");
        let script = "\
printf '\\033[?2004h'
IFS= read -r prep
printf '%s\\n' \"$prep\" >> \"$KANNA_TEST_LOG\"
printf '\\r\\nUnderstood, wrapping up now.\\r\\n'
printf '\\r\\n\\342\\200\\272 \\r\\n'
IFS= read -r quit
printf '%s\\n' \"$quit\" >> \"$KANNA_TEST_LOG\"
sleep 60
";
        spawn_pty_session(&daemon, SESSION, script, &log_path).await;
        tokio::time::sleep(Duration::from_millis(500)).await;

        let state = state_for(&daemon, "desktop-finalize-real-forced-exit");
        let sequence = tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                finalize_source_session(&state, &work_item(), SESSION, Some("pty"), Some("codex"))
                    .await
            }
        });

        // The real signal to intervene is the child having actually consumed
        // the quit command, not a fixed sleep guessing when that happened.
        wait_for_log_lines(&log_path, 2, Duration::from_secs(15)).await;
        kill_session_best_effort(&daemon, SESSION).await;

        let outcome = sequence.await.expect("finalization sequence task");
        assert!(!outcome.cleanly_finalized());
        let reason = outcome
            .degraded_reason
            .expect("a forced kill after quit reported clean finalization");
        assert!(
            reason.contains("forcibly killed") && reason.contains("after the quit command"),
            "{reason}",
        );
    }
}
