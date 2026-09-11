import { fileURLToPath } from "node:url";
import { runCommand } from "./processes";
import {
  buffyStagingCredentialsFromEnv,
  stagingRemoteE2eSkipMessage
} from "./staging";
import { remoteHarnessSpecFiles, remoteHarnessVitestArgs } from "./vitestArgs";

export type RemoteE2eCommand = typeof runCommand;

const supportedArgs = new Set([
  "--dev",
  "--staging",
  "--mobile-relay",
  "--mobile-relay-terminal-control",
  "--desktop-pairing"
]);

export async function dispatchRemoteE2e(
  args: string[],
  options: { run?: RemoteE2eCommand; env?: NodeJS.ProcessEnv } = {},
): Promise<void> {
  const run = options.run ?? runCommand;
  const env = options.env ?? process.env;
  const staging = args.includes("--staging");
  const mobileRelay = args.includes("--mobile-relay");
  const mobileRelayTerminalControl = args.includes("--mobile-relay-terminal-control");
  const desktopPairing = args.includes("--desktop-pairing");
  const dev = args.includes("--dev") || !staging;
  const unsupportedArg = args.find((arg) => !supportedArgs.has(arg));
  if (unsupportedArg) throw new Error(`remote-e2e does not support ${unsupportedArg}`);
  if (staging && dev && args.includes("--dev")) {
    throw new Error("remote-e2e accepts only one of --dev or --staging");
  }

  if (staging) {
  if (mobileRelay || mobileRelayTerminalControl || desktopPairing) {
    throw new Error("Layer C/D staging remote-e2e lanes are human-gated.");
  }
  const credentials = buffyStagingCredentialsFromEnv(env);
  if (!credentials.ok) {
    console.log(stagingRemoteE2eSkipMessage(credentials.missing));
    process.exit(0);
  }
}

if (!mobileRelay && !mobileRelayTerminalControl && !desktopPairing) {
  for (const specFile of remoteHarnessSpecFiles(staging)) {
    await run("pnpm", remoteHarnessVitestArgs(specFile), {
      cwd: fileURLToPath(new URL("..", import.meta.url)),
      env: {
        ...env,
        KANNA_APP_ENV: env.KANNA_APP_ENV || "dev",
        KANNA_REMOTE_E2E_ENV: staging ? "staging" : "dev"
      }
    });
  }
}

if (mobileRelay || mobileRelayTerminalControl) {
  await run("pnpm", ["--dir", "apps/mobile", "run", mobileRelayTerminalControl ? "test:e2e:relay-terminal-control" : "test:e2e:relay"], {
    cwd: fileURLToPath(new URL("../../..", import.meta.url)),
    env: {
      ...env,
      KANNA_APP_ENV: env.KANNA_APP_ENV || "dev",
      KANNA_REMOTE_E2E_ENV: "dev"
    }
  });
}

if (desktopPairing) {
  await run("pnpm", [
    "--dir",
    "apps/desktop",
    "exec",
    "tsx",
    "tests/e2e/run.ts",
    "real/mobile-pairing-ui.test.ts"
  ], {
    cwd: fileURLToPath(new URL("../../..", import.meta.url)),
    env: {
      ...env,
      KANNA_APP_ENV: env.KANNA_APP_ENV || "dev",
      KANNA_REMOTE_E2E_ENV: "dev"
    }
  });
}

}

if (process.argv[1] && import.meta.url === new URL(process.argv[1], "file:").href) {
  await dispatchRemoteE2e(process.argv.slice(2));
}
