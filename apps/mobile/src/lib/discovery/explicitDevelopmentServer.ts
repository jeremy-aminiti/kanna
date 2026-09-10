import type { FetchLike } from "../transports/lanTransport";
import type { BonjourBrowser, BonjourService } from "./bonjour";
import { fetchDesktopStatus } from "./trustedBonjour";

const SERVICE_TYPE = "_kanna-mobile._tcp.";

export function resolveExplicitDevelopmentServerUrl(
  rawUrl: string | undefined,
  development: boolean
): string | null {
  const value = rawUrl?.trim();
  if (!development || !value) return null;

  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new Error(
      `EXPO_PUBLIC_KANNA_SERVER_URL must be an http URL, got ${JSON.stringify(value)}.`
    );
  }
  if (
    url.protocol !== "http:" ||
    url.username ||
    url.password ||
    (url.pathname !== "/" && url.pathname !== "") ||
    url.search ||
    url.hash
  ) {
    throw new Error(
      "EXPO_PUBLIC_KANNA_SERVER_URL must be an http origin without credentials, a path, query, or fragment."
    );
  }
  return url.toString().replace(/\/$/, "");
}

export function createExplicitDevelopmentServerBrowser(input: {
  baseUrl: string;
  browser: BonjourBrowser;
  fetchImpl: FetchLike;
  probeTimeoutMs?: number;
}): BonjourBrowser {
  const listeners = new Set<() => void>();
  let explicitService: BonjourService | null = null;
  let stopped = false;

  const notify = () => {
    for (const listener of listeners) listener();
  };
  const unsubscribe = input.browser.subscribe(() => notify());

  const refresh = async () => {
    const status = await fetchDesktopStatus(
      input.baseUrl,
      input.fetchImpl,
      input.probeTimeoutMs
    );
    const desktopId = typeof status?.desktopId === "string"
      ? status.desktopId.trim()
      : "";
    const desktopName = typeof status?.desktopName === "string"
      ? status.desktopName.trim()
      : "";
    if (!desktopId || !desktopName) {
      throw new Error(
        `The development server at ${input.baseUrl} did not report a valid desktop identity.`
      );
    }

    const url = new URL(input.baseUrl);
    const nextService: BonjourService = {
      name: desktopName,
      type: SERVICE_TYPE,
      host: url.hostname,
      port: Number.parseInt(url.port || "80", 10),
      txt: { desktopId }
    };
    const changed =
      explicitService?.name !== nextService.name ||
      explicitService?.host !== nextService.host ||
      explicitService?.port !== nextService.port ||
      explicitService?.txt.desktopId !== nextService.txt.desktopId;
    explicitService = nextService;
    if (!stopped && changed) notify();
  };

  return {
    getServices() {
      const services = [...input.browser.getServices()];
      if (
        explicitService &&
        !services.some((service) =>
          service.host === explicitService?.host &&
          service.port === explicitService.port &&
          service.txt.desktopId === explicitService.txt.desktopId
        )
      ) {
        services.push(explicitService);
      }
      return services;
    },
    refresh,
    start() {
      stopped = false;
      input.browser.start();
      void refresh().catch(() => undefined);
    },
    stop() {
      stopped = true;
      unsubscribe();
      input.browser.stop();
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    }
  };
}
