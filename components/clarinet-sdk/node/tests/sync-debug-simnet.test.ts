/**
 * Tests for the synchronous debug proxy that `initSimnet()` returns when
 * `CLARINET_DEBUG_PORT` is set (`syncDebugSimnet.ts` + `syncDebugSocket.ts`).
 *
 * A mock JSON-line server stands in for `clarinet dap`, so the proxy and its
 * worker transport are exercised without a Rust build. The mock has to run in
 * its own thread: `syncSend` blocks the main thread with `Atomics.wait`, so a
 * server on the main thread could never answer. Requests it receives are
 * relayed back with `postMessage` and drain the next time the main thread
 * yields.
 */
import { Worker } from "node:worker_threads";
import { afterEach, expect, it } from "vitest";

import { closeSyncSocket, connectSyncSocket } from "../src/syncDebugSocket";
import { createSyncDebugSimnet } from "../src/syncDebugSimnet";

type SdkRequest = Record<string, unknown> & { id?: number; method?: string };

const ACCOUNTS_DEPLOYER = "ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM";

/**
 * Mock `clarinet dap` SDK server. Serialized and spawned with `eval: true`, the
 * same way `syncDebugSocket.ts` spawns its own worker, so this file stays
 * self-contained. Plain JS only — the body is stringified.
 */
function mockServerMain() {
  /* eslint-disable */
  const net = require("node:net");
  const { workerData, parentPort } = require("node:worker_threads");
  const { mode, label } = workerData;

  const ACCOUNTS = {
    deployer: "ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM",
    wallet_1: "ST1SJ3DTE5DN7X54YDH5D64R3BCB6A2AG2ZQ8YPD5",
  };

  const line = (value: unknown) => JSON.stringify(value) + "\n";

  function respond(socket: any, request: any) {
    if (request.method === "getAccounts") {
      socket.write(line({ id: request.id, result: { accounts: ACCOUNTS } }));
      return;
    }
    if (mode === "labelled") {
      socket.write(line({ id: request.id, result: { server: label } }));
      return;
    }
    socket.write(
      line({ id: request.id, result: { result: "0x0703", events: "[]", costs: "null" } }),
    );
  }

  const server = net.createServer((socket: any) => {
    let buffer = "";
    socket.on("data", (chunk: any) => {
      buffer += chunk.toString("utf8");
      const lines = buffer.split("\n");
      buffer = lines.pop() ?? "";
      for (const raw of lines) {
        if (!raw.trim()) continue;
        const request = JSON.parse(raw);
        parentPort.postMessage({ type: "request", request });
        respond(socket, request);
      }
    });
  });

  server.listen(0, "127.0.0.1", () => {
    parentPort.postMessage({ type: "listening", port: server.address().port });
  });
  /* eslint-enable */
}

const MOCK_SERVER_SOURCE = `(${mockServerMain.toString()})()`;

type MockServer = {
  port: number;
  /** Requests the server has received, as of the last time the loop yielded. */
  requests: () => Promise<SdkRequest[]>;
};

const workers: Worker[] = [];

async function startMockServer(
  options: { mode?: "default" | "labelled"; label?: string } = {},
): Promise<MockServer> {
  const received: SdkRequest[] = [];
  const worker = new Worker(MOCK_SERVER_SOURCE, {
    eval: true,
    workerData: { mode: options.mode ?? "default", label: options.label ?? "" },
  });
  worker.unref();
  workers.push(worker);

  const { promise, resolve, reject } = Promise.withResolvers<number>();
  worker.on("message", (message: { type: string; port?: number; request?: SdkRequest }) => {
    if (message.type === "listening") resolve(message.port!);
    else if (message.type === "request") received.push(message.request!);
  });
  worker.on("error", reject);

  return {
    port: await promise,
    requests: async () => {
      // Messages posted while the main thread was blocked in `Atomics.wait`
      // only drain once it yields, so give the event loop a real turn.
      await new Promise((r) => setTimeout(r, 50));
      return received;
    },
  };
}

/** Connect the worker transport and build a proxy against a mock server. */
async function connectProxy(options?: { mode?: "default" | "labelled"; label?: string }) {
  const server = await startMockServer(options);
  await connectSyncSocket(server.port);
  return { proxy: createSyncDebugSimnet(), server };
}

afterEach(async () => {
  closeSyncSocket();
  await Promise.all(workers.splice(0).map((worker) => worker.terminate()));
});

/**
 * Finding 20 of the PR #2483 review: `index.ts` casts the proxy with
 * `as unknown as Simnet`, and `Simnet` is a mapped type over the *entire* wasm
 * `SDK`. `SyncDebugSimnet` declares 18 of those members; the rest are a runtime
 * `TypeError` or a silent `undefined` as soon as `CLARINET_DEBUG_PORT` is set.
 *
 * `transferSTX` and `deployContract` matter most: both exist as `mineBlock`
 * transaction types but not as top-level methods, so `simnet.transferSTX(...)` —
 * the form most tests use — is a `TypeError`.
 *
 * The list is `keyof SDK` from `clarinet_sdk.d.ts`, minus `constructor`, `free`,
 * `[Symbol.dispose]` and the static `getDefaultEpoch`, minus what
 * `SyncDebugSimnet` declares. The `as unknown as` cast is what removes the
 * compile-time signal; without it `tsc` would name every one of these.
 */
const UNIMPLEMENTED_SIMNET_MEMBERS = [
  "clearCache",
  "currentEpoch",
  "deployContract",
  "enablePerformance",
  "executeCommand",
  "generateDeploymentPlan",
  "getBlockTime",
  "getContractAST",
  "getContractSource",
  "getContractsInterfaces",
  "getDataVar",
  "getDefaultClarityVersionForCurrentEpoch",
  "getMapEntry",
  "initEmptySession",
  "mineEmptyBlocks",
  "mineEmptyBurnBlock",
  "mineEmptyBurnBlocks",
  "mineEmptyStacksBlocks",
  "mintFT",
  "mintSTX",
  "setEpoch",
  "setLocalAccounts",
  "transferSTX",
] as const;

it("the debug proxy implements the whole Simnet surface", async () => {
  const { proxy } = await connectProxy();
  const members = proxy as unknown as Record<string, unknown>;

  const absent = UNIMPLEMENTED_SIMNET_MEMBERS.filter((name) => members[name] === undefined);

  expect(
    absent,
    `initSimnet() presents this proxy as a Simnet, but ${absent.length} of its members are missing`,
  ).toEqual([]);
});

/**
 * Finding 20 of the PR #2483 review: `collectReport` traded a crash for a wrong
 * answer. It used to be absent, so `vitest.setup.ts` threw in `afterEach`; it
 * now returns `{ coverage: "", costs: "" }`, which `vitest.setup.ts` pushes
 * straight into the report arrays. A `--coverage` or `--costs` run under
 * `CLARINET_DEBUG_PORT` writes an empty lcov, and a coverage gate reads that as
 * 0% rather than "unsupported".
 *
 * Either fix is fine: throw a named error, or have the server implement it.
 */
it("collectReport does not silently return an empty report", async () => {
  const { proxy } = await connectProxy();

  let report: { coverage: string; costs: string };
  try {
    report = proxy.collectReport(false, "");
  } catch (error) {
    // Acceptable: the proxy says out loud that it cannot produce a report.
    expect((error as Error).message).toMatch(/coverage|cost|report/i);
    return;
  }

  expect(
    report.coverage,
    "an empty lcov reads as 0% coverage, not as an unsupported operation",
  ).not.toBe("");
});

/**
 * Finding 20 of the PR #2483 review: the proxy's `callPrivateFn` sends
 * `method: "callPublicFn"`, so the server has no way to tell the two apart and
 * routes it through an ordinary `contract-call?` — which cannot reach a
 * `define-private` function at all. Under `CLARINET_DEBUG_PORT` an existing
 * `simnet.callPrivateFn(...)` therefore fails with "has no public function".
 */
it("callPrivateFn tells the server it is a private call", async () => {
  const { proxy, server } = await connectProxy();

  proxy.callPrivateFn("counter", "double", [], ACCOUNTS_DEPLOYER);

  const methods = (await server.requests()).map((request) => request.method);
  expect(methods).toContain("callPrivateFn");
});
