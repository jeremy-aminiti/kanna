import { createServer } from "node:net";
import { afterEach, describe, expect, it } from "vitest";
import WebSocket, { WebSocketServer } from "ws";
import {
  hasConnectionPairForTests,
  pendingResponseCountForTests,
  routeMessage,
  setPhoneConnection,
  setServerConnection,
} from "../src/router.js";

async function setUpTwoDesktops(userId: string, url: string) {
  const requester = await connect(url);
  const target = await connect(url);
  setServerConnection(
    userId,
    "desktop-requester",
    requester.server,
    desktopProof("desktop-requester"),
  );
  setServerConnection(
    userId,
    "desktop-target",
    target.server,
    desktopProof("desktop-target"),
  );
  return { requester, target };
}

function sendDesktopInvoke(
  userId: string,
  requester: { server: WebSocket },
  id: string,
  targetDesktopId = "desktop-target",
): void {
  routeMessage(
    userId,
    "server",
    JSON.stringify({
      type: "invoke",
      id,
      desktopId: targetDesktopId,
      method: "GET",
      path: "/v1/lan-routing/bootstrap",
      body: null,
    }),
    requester.server,
    "desktop-requester",
    desktopProof("desktop-requester"),
  );
}

const sockets: WebSocket[] = [];
let server: WebSocketServer | null = null;

async function freePort(): Promise<number> {
  return await new Promise((resolve, reject) => {
    const probe = createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const address = probe.address();
      if (!address || typeof address === "string") {
        reject(new Error("missing probe address"));
        return;
      }
      probe.close((error) => error ? reject(error) : resolve(address.port));
    });
  });
}

async function startServer(): Promise<string> {
  const port = await freePort();
  server = new WebSocketServer({ port, host: "127.0.0.1" });
  await new Promise<void>((resolve) => server!.once("listening", resolve));
  return `ws://127.0.0.1:${port}`;
}

async function connect(url: string): Promise<{ client: WebSocket; server: WebSocket }> {
  const accepted = new Promise<WebSocket>((resolve) => server!.once("connection", resolve));
  const client = new WebSocket(url);
  await new Promise<void>((resolve, reject) => {
    client.once("open", resolve);
    client.once("error", reject);
  });
  const serverSocket = await accepted;
  sockets.push(client, serverSocket);
  return { client, server: serverSocket };
}

async function disconnect(peer: { client: WebSocket; server: WebSocket }): Promise<void> {
  const closed = new Promise<void>((resolve) => peer.server.once("close", resolve));
  peer.client.close();
  await closed;
}

function desktopProof(desktopId: string): {
  kind: "desktop";
  desktopId: string;
  desktopSecret: string;
} {
  return {
    kind: "desktop",
    desktopId,
    desktopSecret: `${desktopId}-secret`,
  };
}

function nextMessage(socket: WebSocket): Promise<Record<string, unknown>> {
  return new Promise((resolve) => {
    socket.once("message", (raw) => resolve(JSON.parse(raw.toString())));
  });
}

async function listActiveDesktops(
  userId: string,
  phone: { client: WebSocket; server: WebSocket },
): Promise<string[]> {
  const response = nextMessage(phone.client);
  routeMessage(
    userId,
    "phone",
    JSON.stringify({ type: "invoke", id: "presence", command: "list_active_desktops" }),
    phone.server,
  );
  const data = (await response).data as { desktopIds?: unknown };
  return [...(data.desktopIds as string[])].sort();
}

afterEach(async () => {
  for (const socket of sockets.splice(0)) socket.terminate();
  await new Promise<void>((resolve) => server?.close(() => resolve()) ?? resolve());
  server = null;
});

describe("connection pair lifetime", () => {
  it("routes requests and responses directly between sibling desktops", async () => {
    const url = await startServer();
    const userId = "desktop-controller-user";
    const requester = await connect(url);
    const target = await connect(url);
    setServerConnection(
      userId,
      "desktop-requester",
      requester.server,
      desktopProof("desktop-requester"),
    );
    setServerConnection(
      userId,
      "desktop-target",
      target.server,
      desktopProof("desktop-target"),
    );

    const listed = nextMessage(requester.client);
    routeMessage(
      userId,
      "server",
      JSON.stringify({
        type: "invoke",
        id: "desktop-list",
        command: "list_active_desktops",
        args: {},
      }),
      requester.server,
      "desktop-requester",
      {
        kind: "desktop",
        desktopId: "desktop-requester",
        desktopSecret: "requester-secret",
      },
    );
    expect((await listed).data).toEqual({
      desktopIds: ["desktop-requester", "desktop-target"],
    });

    const delivered = nextMessage(target.client);
    routeMessage(
      userId,
      "server",
      JSON.stringify({
        type: "invoke",
        id: "desktop-invoke",
        desktopId: "desktop-target",
        method: "GET",
        path: "/v1/tasks/recent",
        body: null,
      }),
      requester.server,
      "desktop-requester",
      {
        kind: "desktop",
        desktopId: "desktop-requester",
        desktopSecret: "requester-secret",
      },
    );
    expect(await delivered).toMatchObject({
      id: "desktop-invoke",
      desktopId: "desktop-target",
      path: "/v1/tasks/recent",
    });

    const response = nextMessage(requester.client);
    routeMessage(
      userId,
      "server",
      JSON.stringify({
        type: "response",
        id: "desktop-invoke",
        status: 200,
        body: [{ id: "task-on-target" }],
      }),
      target.server,
      "desktop-target",
    );
    expect((await response).body).toEqual([{ id: "task-on-target" }]);
  });

  it("stamps a forwarded invoke with the sender's own verified identity, overwriting any claim in the frame", async () => {
    const url = await startServer();
    const userId = "desktop-provenance-user";
    const requester = await connect(url);
    const target = await connect(url);
    setServerConnection(
      userId,
      "desktop-requester",
      requester.server,
      desktopProof("desktop-requester"),
    );
    setServerConnection(
      userId,
      "desktop-target",
      target.server,
      desktopProof("desktop-target"),
    );

    const delivered = nextMessage(target.client);
    routeMessage(
      userId,
      "server",
      JSON.stringify({
        type: "invoke",
        id: "desktop-invoke-provenance",
        desktopId: "desktop-target",
        method: "GET",
        path: "/v1/status",
        body: null,
        // A sender cannot self-report who it is; this must be discarded in
        // favor of the identity this connection actually authenticated as.
        sourceDesktopId: "desktop-impostor",
      }),
      requester.server,
      "desktop-requester",
      {
        kind: "desktop",
        desktopId: "desktop-requester",
        desktopSecret: "requester-secret",
      },
    );

    expect(await delivered).toMatchObject({
      id: "desktop-invoke-provenance",
      sourceDesktopId: "desktop-requester",
    });
  });

  it("rejects sibling desktop invokes authenticated by a legacy device token", async () => {
    const url = await startServer();
    const userId = "legacy-device-user";
    const requester = await connect(url);
    const target = await connect(url);
    setServerConnection(userId, "unverified-requester", requester.server);
    setServerConnection(userId, "desktop-target", target.server);

    let targetReceivedMessage = false;
    target.client.once("message", () => {
      targetReceivedMessage = true;
    });
    const rejected = nextMessage(requester.client);
    routeMessage(
      userId,
      "server",
      JSON.stringify({
        type: "invoke",
        id: "legacy-device-invoke",
        desktopId: "desktop-target",
        method: "GET",
        path: "/v1/tasks/recent",
        body: null,
      }),
      requester.server,
      "unverified-requester",
      {
        kind: "device",
        desktopId: "unverified-requester",
        deviceToken: "legacy-device-token",
      },
    );

    expect(await rejected).toMatchObject({
      type: "response",
      id: "legacy-device-invoke",
      error: "desktop-secret authentication is required",
    });
    await new Promise<void>((resolve) => setImmediate(resolve));
    expect(targetReceivedMessage).toBe(false);
  });

  it("returns an uncorrelated error for a rejected sibling invoke without an id", async () => {
    const url = await startServer();
    const userId = "missing-invoke-id-user";
    const requester = await connect(url);
    const target = await connect(url);
    setServerConnection(userId, "unverified-requester", requester.server);
    setServerConnection(
      userId,
      "desktop-target",
      target.server,
      desktopProof("desktop-target"),
    );

    const rejected = nextMessage(requester.client);
    routeMessage(
      userId,
      "server",
      JSON.stringify({
        type: "invoke",
        desktopId: "desktop-target",
        method: "GET",
        path: "/v1/tasks/recent",
        body: null,
      }),
      requester.server,
      "unverified-requester",
      {
        kind: "device",
        desktopId: "unverified-requester",
        deviceToken: "legacy-device-token",
      },
    );

    expect(await rejected).toMatchObject({
      type: "response",
      id: null,
      error: "desktop-secret authentication is required",
    });
  });

  it("rejects sibling invokes to a target authenticated by a legacy device token", async () => {
    const url = await startServer();
    const userId = "legacy-target-user";
    const requester = await connect(url);
    const target = await connect(url);
    setServerConnection(
      userId,
      "verified-requester",
      requester.server,
      desktopProof("verified-requester"),
    );
    setServerConnection(userId, "unverified-target", target.server, {
      kind: "device",
      desktopId: "unverified-target",
      deviceToken: "legacy-device-token",
    });

    let targetReceivedMessage = false;
    target.client.once("message", () => {
      targetReceivedMessage = true;
    });
    const rejected = nextMessage(requester.client);
    routeMessage(
      userId,
      "server",
      JSON.stringify({
        type: "invoke",
        id: "legacy-target-invoke",
        desktopId: "unverified-target",
        method: "GET",
        path: "/v1/tasks/recent",
        body: null,
      }),
      requester.server,
      "verified-requester",
      desktopProof("verified-requester"),
    );

    expect(await rejected).toMatchObject({
      type: "response",
      id: "legacy-target-invoke",
      error: "target desktop-secret authentication is required",
    });
    await new Promise<void>((resolve) => setImmediate(resolve));
    expect(targetReceivedMessage).toBe(false);
  });

  it("cleans pending responses owned by a replaced same-id desktop socket", async () => {
    const url = await startServer();
    const userId = "same-id-reconnect-user";
    let requester = await connect(url);
    const target = await connect(url);
    setServerConnection(
      userId,
      "desktop-requester",
      requester.server,
      desktopProof("desktop-requester"),
    );
    setServerConnection(
      userId,
      "desktop-target",
      target.server,
      desktopProof("desktop-target"),
    );

    for (let attempt = 1; attempt <= 3; attempt += 1) {
      routeMessage(
        userId,
        "server",
        JSON.stringify({
          type: "invoke",
          id: `in-flight-${attempt}`,
          desktopId: "desktop-target",
          method: "GET",
          path: "/v1/tasks/recent",
          body: null,
        }),
        requester.server,
        "desktop-requester",
        desktopProof("desktop-requester"),
      );
      expect(pendingResponseCountForTests(userId)).toBe(1);

      const oldRequester = requester;
      const oldClosed = new Promise<void>((resolve) => {
        oldRequester.server.once("close", resolve);
      });
      requester = await connect(url);
      setServerConnection(
        userId,
        "desktop-requester",
        requester.server,
        desktopProof("desktop-requester"),
      );
      await oldClosed;

      expect(pendingResponseCountForTests(userId)).toBe(0);
    }
  });

  it("keeps the other desktops online when one disconnects with no phone attached", async () => {
    const url = await startServer();
    const userId = "multi-desktop-user";
    const macbook = await connect(url);
    const studio = await connect(url);
    const imac = await connect(url);
    setServerConnection(userId, "desktop-macbook", macbook.server);
    setServerConnection(userId, "desktop-studio", studio.server);
    setServerConnection(userId, "desktop-imac", imac.server);

    // The phone is away — exactly the state the desktops sit in most of the day.
    await disconnect(macbook);

    const phone = await connect(url);
    setPhoneConnection(userId, phone.server);
    expect(await listActiveDesktops(userId, phone)).toEqual([
      "desktop-imac",
      "desktop-studio",
    ]);
  });

  it("keeps a remaining phone client routable when another disconnects with no desktop attached", async () => {
    const url = await startServer();
    const userId = "multi-phone-user";
    const first = await connect(url);
    const second = await connect(url);
    setPhoneConnection(userId, first.server);
    setPhoneConnection(userId, second.server);

    await disconnect(first);

    const desktop = await connect(url);
    setServerConnection(userId, "desktop-macbook", desktop.server);
    const delivered = nextMessage(second.client);
    routeMessage(
      userId,
      "server",
      JSON.stringify({ type: "event", name: "task_changed" }),
      desktop.server,
      "desktop-macbook",
    );
    expect((await delivered).name).toBe("task_changed");
  });

  it("still forgets the user once both sides have disconnected", async () => {
    const url = await startServer();
    const userId = "drained-user";
    const desktop = await connect(url);
    const phone = await connect(url);
    setServerConnection(userId, "desktop-macbook", desktop.server);
    setPhoneConnection(userId, phone.server);
    expect(hasConnectionPairForTests(userId)).toBe(true);

    await disconnect(desktop);
    await disconnect(phone);

    expect(hasConnectionPairForTests(userId)).toBe(false);
  });
});

describe("relay v2 provenance and response correlation", () => {
  it("strips a phone-forged sourceDesktopId before forwarding to a desktop", async () => {
    const url = await startServer();
    const userId = "phone-forged-provenance-user";
    const phone = await connect(url);
    const target = await connect(url);
    setPhoneConnection(userId, phone.server);
    setServerConnection(
      userId,
      "desktop-target",
      target.server,
      desktopProof("desktop-target"),
    );

    const delivered = nextMessage(target.client);
    routeMessage(
      userId,
      "phone",
      JSON.stringify({
        type: "invoke",
        id: "phone-forged",
        desktopId: "desktop-target",
        method: "GET",
        path: "/v1/lan-routing/bootstrap",
        // A phone has no relay-verified desktop identity; this must never
        // reach the target, regardless of what a v2 receiver would do with
        // it if it did.
        sourceDesktopId: "desktop-a",
      }),
      phone.server,
    );

    const message = await delivered;
    expect(message).not.toHaveProperty("sourceDesktopId");
    expect(message).toMatchObject({ id: "phone-forged", desktopId: "desktop-target" });
  });

  it("leaves an unrelated field untouched when a phone frame carries no sourceDesktopId", async () => {
    const url = await startServer();
    const userId = "phone-clean-frame-user";
    const phone = await connect(url);
    const target = await connect(url);
    setPhoneConnection(userId, phone.server);
    setServerConnection(
      userId,
      "desktop-target",
      target.server,
      desktopProof("desktop-target"),
    );

    const delivered = nextMessage(target.client);
    routeMessage(
      userId,
      "phone",
      JSON.stringify({
        type: "invoke",
        id: "phone-clean",
        desktopId: "desktop-target",
        method: "GET",
        path: "/v1/tasks/recent",
        body: null,
      }),
      phone.server,
    );

    expect(await delivered).toMatchObject({ id: "phone-clean", path: "/v1/tasks/recent" });
  });

  it("refuses a response from a desktop other than the one the request was addressed to, without consuming the pending entry", async () => {
    const url = await startServer();
    const userId = "wrong-responder-user";
    const { requester, target } = await setUpTwoDesktops(userId, url);
    const impostor = await connect(url);
    setServerConnection(
      userId,
      "desktop-impostor",
      impostor.server,
      desktopProof("desktop-impostor"),
    );

    sendDesktopInvoke(userId, requester, "bootstrap-1");
    expect(pendingResponseCountForTests(userId)).toBe(1);

    let requesterReceivedMessage = false;
    requester.client.once("message", () => {
      requesterReceivedMessage = true;
    });
    routeMessage(
      userId,
      "server",
      JSON.stringify({
        type: "response",
        id: "bootstrap-1",
        status: 200,
        body: { caCertificatePem: "fake-impostor-ca" },
      }),
      impostor.server,
      "desktop-impostor",
    );
    await new Promise<void>((resolve) => setImmediate(resolve));
    expect(requesterReceivedMessage).toBe(false);
    expect(pendingResponseCountForTests(userId)).toBe(1);

    const legitimate = nextMessage(requester.client);
    routeMessage(
      userId,
      "server",
      JSON.stringify({
        type: "response",
        id: "bootstrap-1",
        status: 200,
        body: { caCertificatePem: "real-target-ca" },
      }),
      target.server,
      "desktop-target",
    );
    expect((await legitimate).body).toEqual({ caCertificatePem: "real-target-ca" });
  });

  it("refuses a response from a replacement connection for a request addressed to its now-superseded generation", async () => {
    const url = await startServer();
    const userId = "replaced-responder-user";
    const { requester, target: originalTarget } = await setUpTwoDesktops(userId, url);
    void originalTarget;

    sendDesktopInvoke(userId, requester, "bootstrap-2");
    expect(pendingResponseCountForTests(userId)).toBe(1);

    // desktop-target reconnects (a new socket, same desktop id) before ever
    // answering the request above, which was addressed to - and bound to -
    // its previous connection instance.
    const freshTarget = await connect(url);
    setServerConnection(
      userId,
      "desktop-target",
      freshTarget.server,
      desktopProof("desktop-target"),
    );

    let requesterReceivedMessage = false;
    requester.client.once("message", () => {
      requesterReceivedMessage = true;
    });
    // The replacement connection never actually received this request (the
    // router forwarded it to the old generation), so it answering anyway -
    // whether confused or malicious - must not be honored: the pending
    // entry is bound to the exact socket generation that was addressed, not
    // merely to the desktop id string.
    routeMessage(
      userId,
      "server",
      JSON.stringify({
        type: "response",
        id: "bootstrap-2",
        status: 200,
        body: { caCertificatePem: "wrong-generation-ca" },
      }),
      freshTarget.server,
      "desktop-target",
    );
    await new Promise<void>((resolve) => setImmediate(resolve));
    expect(requesterReceivedMessage).toBe(false);
    expect(pendingResponseCountForTests(userId)).toBe(1);
  });

  it("still refuses cross-user desktop invokes to a same-id-but-different-account desktop", async () => {
    const url = await startServer();
    const userA = "uid-a";
    const userB = "uid-b";
    const requester = await connect(url);
    const otherAccountDesktop = await connect(url);
    setServerConnection(
      userA,
      "desktop-requester",
      requester.server,
      desktopProof("desktop-requester"),
    );
    setServerConnection(
      userB,
      "desktop-target",
      otherAccountDesktop.server,
      desktopProof("desktop-target"),
    );

    let otherAccountReceivedMessage = false;
    otherAccountDesktop.client.once("message", () => {
      otherAccountReceivedMessage = true;
    });
    const rejected = nextMessage(requester.client);
    sendDesktopInvoke(userA, requester, "cross-uid-invoke");

    expect(await rejected).toMatchObject({
      type: "response",
      id: "cross-uid-invoke",
      error: "Desktop offline",
    });
    await new Promise<void>((resolve) => setImmediate(resolve));
    expect(otherAccountReceivedMessage).toBe(false);
  });
});
