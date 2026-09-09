import { useEffect, useState } from "react";
import type { TaskTerminalStatus } from "../state/sessionStore";

/**
 * How long the transport may be away before the reader is told anything.
 *
 * A relay tunnel that dies on a hotel NAT is redialled on the stream client's
 * 250/500/1000/2000 ms ladder, so an ordinary redial completes well inside this
 * window. Presenting one is worse than presenting nothing: the grid on screen
 * stays correct across it — the server either replays the gap or replaces the
 * buffer atomically — so a spinner over readable content only blinks the reader
 * out of what they were reading, several times a minute on a flaky link.
 */
export const TERMINAL_RECONNECT_GRACE_MS = 2_500;

/** The transport is between attachments: not live, but not over either. */
export function isTerminalTransportGap(status: TaskTerminalStatus): boolean {
  return status === "connecting" || status === "restarting";
}

export interface TerminalPresentationStatusInput {
  /** The raw transport status the controller publishes. */
  status: TaskTerminalStatus;
  /** Whether an authoritative grid is already rendered and still on screen. */
  hasRenderedGrid: boolean;
  /** Whether this gap has outlasted {@link TERMINAL_RECONNECT_GRACE_MS}. */
  gapExceededGrace: boolean;
}

/**
 * The status the *presentation* should be built from, which is not the same
 * question as what the transport is doing. A sub-threshold gap under a rendered
 * grid presents as live, because nothing the reader can see has changed yet.
 * Every other case — no grid to keep, a real outage, a closed or failed
 * session — presents the truth.
 */
export function resolveTerminalPresentationStatus({
  status,
  hasRenderedGrid,
  gapExceededGrace
}: TerminalPresentationStatusInput): TaskTerminalStatus {
  if (!hasRenderedGrid || gapExceededGrace || !isTerminalTransportGap(status)) {
    return status;
  }
  return "live";
}

export interface TerminalReconnectPresentation {
  /** Status to derive every visible terminal affordance from. */
  presentationStatus: TaskTerminalStatus;
  /** The gap outlasted the grace window and the reader is being told. */
  isReconnectVisible: boolean;
}

/**
 * Hold the last presented status across a short transport gap.
 *
 * The timer is the only stateful part: it exists because "this gap has gone on
 * long enough to mention" is a fact about elapsed time, not about any event the
 * transport emits. It is armed once per gap and cleared as soon as the stream
 * is live again, so a reconnect ladder that lands inside the window produces no
 * render-visible transition at all.
 */
export function useTerminalReconnectPresentation(
  status: TaskTerminalStatus,
  hasRenderedGrid: boolean,
  graceMs: number = TERMINAL_RECONNECT_GRACE_MS
): TerminalReconnectPresentation {
  const [gapExceededGrace, setGapExceededGrace] = useState(false);
  const withinGrace = hasRenderedGrid && isTerminalTransportGap(status);

  useEffect(() => {
    if (!withinGrace) {
      setGapExceededGrace(false);
      return;
    }
    const timer = setTimeout(() => setGapExceededGrace(true), graceMs);
    return () => clearTimeout(timer);
  }, [graceMs, withinGrace]);

  return {
    presentationStatus: resolveTerminalPresentationStatus({
      status,
      hasRenderedGrid,
      gapExceededGrace
    }),
    isReconnectVisible: withinGrace && gapExceededGrace
  };
}
