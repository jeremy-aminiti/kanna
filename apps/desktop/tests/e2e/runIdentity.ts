import { createHash } from "node:crypto";
import { Buffer } from "node:buffer";
import { basename } from "node:path";

// Darwin's sockaddr_un.sun_path is 104 bytes including its terminator. tmux
// places -L sockets below /private/tmp/tmux-<uid>/, so 56 ASCII bytes leave
// at least 18 bytes of margin even for the longest unsigned uid.
export const E2E_TMUX_NAME_MAX_BYTES = 56;
const SECONDARY_SUFFIX = "-secondary";
const PRIMARY_NAME_MAX_BYTES = E2E_TMUX_NAME_MAX_BYTES - SECONDARY_SUFFIX.length;
const IDENTITY_HASH_HEX_LENGTH = 20;

export interface E2eRunIdentity {
  worktreeName: string;
  runSuffix: string;
  primarySessionName: string;
  secondarySessionName: string;
}

function sanitizeSuffix(value: string): string {
  return value.replace(/[^a-zA-Z0-9_-]/g, "-");
}

function boundedSessionBase(worktreeName: string, runSuffix: string): string {
  const unescaped = `kanna-e2e-${worktreeName}-${runSuffix}`;
  const pathSafe = sanitizeSuffix(unescaped);
  if (
    unescaped === pathSafe &&
    Buffer.byteLength(pathSafe, "utf8") <= PRIMARY_NAME_MAX_BYTES
  ) {
    return pathSafe;
  }

  const digest = createHash("sha256")
    .update(unescaped, "utf8")
    .digest("hex")
    .slice(0, IDENTITY_HASH_HEX_LENGTH);
  const readableBytes = PRIMARY_NAME_MAX_BYTES - digest.length - 1;
  return `${pathSafe.slice(0, readableBytes)}-${digest}`;
}

export function createE2eRunIdentity(input: {
  repoRoot: string;
  pid: number;
  now: number;
}): E2eRunIdentity {
  const rawWorktreeName = basename(input.repoRoot);
  const worktreeName = sanitizeSuffix(rawWorktreeName);
  const runSuffix = sanitizeSuffix(`${input.pid}-${input.now}`);
  const primarySessionName = boundedSessionBase(rawWorktreeName, `${input.pid}-${input.now}`);

  return {
    worktreeName,
    runSuffix,
    primarySessionName,
    secondarySessionName: `${primarySessionName}${SECONDARY_SUFFIX}`,
  };
}
