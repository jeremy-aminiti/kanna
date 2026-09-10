import type { WebDriverClient } from "./webdriver";
import {
  assertNativeWindowIdentity,
  type ExpectedNativeWindowIdentity,
} from "./windowIdentity";

type PreflightClient = Pick<
  WebDriverClient,
  | "createSession"
  | "deleteSession"
  | "getAppBuildInfo"
  | "getBaseUrl"
  | "getNativeWindowTitle"
>;

export async function preflightNativeWindowIdentity(
  client: PreflightClient,
  expected: ExpectedNativeWindowIdentity,
  label: string,
): Promise<void> {
  try {
    await client.createSession({
      dismissStartupShortcuts: false,
    });
    await assertNativeWindowIdentity(client, expected, label);
  } finally {
    await client.deleteSession().catch(() => undefined);
  }
}
