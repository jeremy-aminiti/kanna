import { invoke } from "../invoke"

/**
 * How this machine launches a shell, as resolved by the shared policy in
 * `kanna-runtime-defaults` and handed over by Tauri.
 *
 * The frontend used to answer this itself with a `/bin/zsh` literal. That was
 * a second source of truth for something the server already decides, and on a
 * stock Linux image — which has no zsh at all — it was simply wrong.
 */
export interface ShellLaunch {
  /** Absolute path to the shell binary. */
  executable: string
  /** The shell's own name: `zsh`, `bash`, `sh`. */
  name: string
  /** How this shell spells "login": `--login`, or `-l` for POSIX `sh`. */
  loginArg: string
}

let cached: Promise<ShellLaunch> | null = null

/**
 * Resolved once per app run. The policy itself is process-wide and immutable
 * in the Rust side, so re-asking would only cost an IPC round trip on every
 * shell tab.
 */
export function resolveShellLaunch(): Promise<ShellLaunch> {
  cached ??= invoke<ShellLaunch>("shell_launch")
  return cached
}

/** Test seam: the cache is per app run, and a test run is not one. */
export function resetShellLaunchCacheForTests(): void {
  cached = null
}
