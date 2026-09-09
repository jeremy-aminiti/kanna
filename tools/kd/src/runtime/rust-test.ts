import type { CommandResult, CommandRunner } from "./process";

export interface RustTestCommand {
  name: "agent-protocol" | "frontend" | "sidecars" | "clippy" | "workspace" | "daemon";
  command: "./scripts/check-agent-protocol-types.sh" | "pnpm" | "./kd" | "cargo";
  args: string[];
}

interface ExecutedRustTestCommand extends RustTestCommand, CommandResult {}

export interface RustTestOptions {
  /**
   * Include the Tauri desktop crate on a platform whose default is headless.
   *
   * Linux's default stays headless because the worker is the shipped Linux
   * product and its gate must not depend on WebKitGTK being installed. But the
   * desktop crate does build and test there, so "excluded by default" had
   * become indistinguishable from "cannot run" — which is how a Linux desktop
   * regression would reach a review with nothing to catch it. This is the
   * switch that tells them apart. No effect on macOS, where the desktop crate
   * is always in.
   */
  desktop?: boolean;
}

/**
 * The lanes `./kd test rust` runs.
 *
 * Off macOS the desktop crate is excluded and its frontend build skipped by
 * default. That is not a lowered bar: the Tauri app is not part of the
 * headless worker's surface. `--desktop` opts back in; the sidecars, the
 * daemon and the server are built and tested in full either way.
 */
export function buildRustTestCommands(
  platform: NodeJS.Platform = process.platform,
  options: RustTestOptions = {},
): RustTestCommand[] {
  const headless = platform !== "darwin" && !options.desktop;
  const commands: RustTestCommand[] = [
    {
      name: "agent-protocol",
      command: "./scripts/check-agent-protocol-types.sh",
      args: [],
    },
  ];
  if (!headless) {
    commands.push({ name: "frontend", command: "pnpm", args: ["--dir", "apps/desktop", "build"] });
  }
  commands.push(
    { name: "sidecars", command: "./kd", args: ["build", "sidecars"] },
    {
      name: "clippy",
      command: "cargo",
      args: [
        "clippy",
        "--workspace",
        "--all-targets",
        ...(headless ? ["--exclude", "kanna-desktop"] : []),
        "--",
        "-D",
        "warnings",
      ],
    },
    {
      name: "workspace",
      command: "cargo",
      args: [
        "test",
        "--workspace",
        "--exclude",
        "kanna-daemon",
        ...(headless ? ["--exclude", "kanna-desktop"] : []),
      ],
    },
    { name: "daemon", command: "cargo", args: ["test", "-p", "kanna-daemon", "--", "--test-threads=1"] },
  );
  return commands;
}

export async function executeRustTests(input: {
  repoRoot: string;
  env: NodeJS.ProcessEnv;
  runner: CommandRunner;
  desktop?: boolean;
}) {
  const commands: ExecutedRustTestCommand[] = [];
  for (const command of buildRustTestCommands(process.platform, { desktop: input.desktop })) {
    const result = await input.runner.run(command.command, command.args, {
      cwd: input.repoRoot,
      env: input.env,
      streamOutput: true,
    });
    commands.push({ ...command, ...result });
    if (result.exitCode !== 0) {
      return {
        ok: false,
        message: result.stderr || result.stdout || `${command.name} Rust tests failed.`,
        data: { commands },
      };
    }
  }
  return {
    ok: true,
    message: input.desktop
      ? "Canonical Rust tests passed, including the desktop crate."
      : "Canonical Rust tests passed.",
    data: { commands },
  };
}
