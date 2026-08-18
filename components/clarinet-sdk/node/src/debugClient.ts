import * as net from "net";
import { spawn, type ChildProcess } from "child_process";

import { Cl, type ClarityValue } from "@stacks/transactions";

import { connectSyncSocket, syncSend, closeSyncSocket } from "./syncDebugSocket.js";
import { parseEvents, type ParsedTransactionResult, type Tx } from "../../common/src/sdkProxyHelpers.js";

export type DebugCallResult = {
  /** The Clarity return value as a human-readable string, e.g. `"(ok u1)"`. */
  value: string;
};

type PendingRequest = {
  resolve: (r: SdkResponse) => void;
  reject: (e: Error) => void;
};

type SdkResponse = {
  id: number;
  result?: { value: string; hex?: string } | null;
  error?: string;
};

/**
 * A client that connects to a `clarinet dap --dap-port … --sdk-port …` server
 * and evaluates Clarity expressions under DAP debugger control.
 *
 * Breakpoints set in VSCode (or another DAP-capable editor) in `.clar` source
 * files will be hit when the corresponding contract code is reached.
 *
 * @example
 * ```ts
 * const debugger = await startDebugServer();
 * const result = await debugger.callPublicFn("counter", "increment", [], deployer);
 * expect(result.value).toBe("(ok u1)");
 * await debugger.disconnect();
 * ```
 */
export class DebugClient {
  private readonly socket: net.Socket;
  private nextId = 1;
  private readonly pending = new Map<number, PendingRequest>();
  private buffer = "";
  /** Set when this client owns the `clarinet dap` process (via `startDebugServer`). */
  private readonly _process?: ChildProcess;

  constructor(socket: net.Socket, process?: ChildProcess) {
    this.socket = socket;
    this._process = process;

    socket.on("data", (chunk: Buffer) => {
      this.buffer += chunk.toString("utf8");
      const lines = this.buffer.split("\n");
      // Keep any incomplete line in the buffer
      this.buffer = lines.pop() ?? "";
      for (const line of lines) {
        const trimmed = line.trim();
        if (!trimmed) continue;
        try {
          const response = JSON.parse(trimmed) as SdkResponse;
          const pending = this.pending.get(response.id);
          if (pending) {
            this.pending.delete(response.id);
            pending.resolve(response);
          }
        } catch {
          // Ignore malformed lines
        }
      }
    });

    const rejectPending = (err: Error) => {
      for (const { reject } of this.pending.values()) {
        reject(err);
      }
      this.pending.clear();
    };

    socket.on("error", rejectPending);
    socket.on("close", () => {
      rejectPending(new Error("debug server connection closed"));
    });
  }

  private send(request: Record<string, unknown>): Promise<SdkResponse> {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.socket.write(JSON.stringify({ ...request, id }) + "\n");
    });
  }

  /**
   * Call a public contract function through the debug server.
   * Breakpoints in the contract source will pause execution.
   */
  async callPublicFn(
    contract: string,
    method: string,
    args: ClarityValue[],
    sender: string,
  ): Promise<DebugCallResult> {
    const argStrings = args.map((a) => Cl.stringify(a));
    const response = await this.send({
      method: "call",
      contract,
      function: method,
      args: argStrings,
      sender,
    });
    if (response.error) throw new Error(response.error);
    return { value: response.result!.value };
  }

  /**
   * Call a read-only contract function through the debug server.
   * Behaves the same as `callPublicFn` for debugging purposes.
   */
  async callReadOnlyFn(
    contract: string,
    method: string,
    args: ClarityValue[],
    sender: string,
  ): Promise<DebugCallResult> {
    return this.callPublicFn(contract, method, args, sender);
  }

  /**
   * Evaluate an arbitrary Clarity snippet in the simnet session under the debugger.
   */
  async execute(snippet: string): Promise<DebugCallResult> {
    const response = await this.send({ method: "eval", snippet });
    if (response.error) throw new Error(response.error);
    return { value: response.result!.value };
  }

  /** Gracefully disconnect from the debug server. */
  async disconnect(): Promise<void> {
    try {
      await this.send({ method: "disconnect" });
    } finally {
      this.socket.destroy();
      this._process?.kill();
    }
  }
}

function openSocket(port: number): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection({ port, host: "127.0.0.1" }, () => {
      socket.removeListener("error", reject);
      resolve(socket);
    });
    socket.once("error", reject);
  });
}

/**
 * Start or connect to a `clarinet dap` debug server and return a
 * {@link DebugClient}.
 *
 * **Auto-spawn mode** (default): when no `port` is provided and
 * `CLARINET_DEBUG_PORT` is not set, `clarinet dap` is spawned automatically.
 * The returned client owns the process and kills it on
 * {@link DebugClient.disconnect}.
 *
 * **Connect mode**: when `port` is provided (or `CLARINET_DEBUG_PORT` is set),
 * the function connects to a server that is already running — for example one
 * started by the VSCode extension's CodeLens button.
 *
 * @example
 * ```ts
 * // Zero-config — server is spawned automatically
 * const client = await startDebugServer({ manifest: "./Clarinet.toml" });
 * const result = await client.callPublicFn("counter", "increment", [], deployer);
 * await client.disconnect();
 *
 * // With VSCode breakpoints — also open a DAP port for the editor to attach
 * const client = await startDebugServer({ dapPort: 7777 });
 * ```
 */
export async function startDebugServer(options?: {
  /** Path to Clarinet.toml. Defaults to `"./Clarinet.toml"`. */
  manifest?: string;
  /**
   * Connect to this port instead of spawning a new server.
   * Falls back to `CLARINET_DEBUG_PORT` env var, then auto-spawn.
   */
  port?: number;
  /**
   * When auto-spawning, also open a DAP port so a DAP client (e.g. VSCode)
   * can attach and hit breakpoints. Ignored in connect mode.
   */
  dapPort?: number;
}): Promise<DebugClient> {
  const envPort = process.env["CLARINET_DEBUG_PORT"]
    ? Number(process.env["CLARINET_DEBUG_PORT"])
    : undefined;
  const connectPort = options?.port ?? envPort;

  // Connect mode: server is already running externally.
  if (connectPort != null) {
    const socket = await openSocket(connectPort);
    return new DebugClient(socket);
  }

  // Auto-spawn mode: launch clarinet dap ourselves. Ask the OS for a free SDK
  // port so parallel test runs don't contend for a fixed port.
  let sdkPort: number | undefined;
  const manifest = options?.manifest ?? "./Clarinet.toml";

  const args = ["dap", "--sdk-port", "0", "--manifest", manifest];
  if (options?.dapPort != null) {
    args.push("--dap-port", String(options.dapPort));
  }

  const child = spawn("clarinet", args, {
    stdio: ["ignore", "ignore", "pipe"],
  });

  // Wait for the ready signal printed to stderr by run_dap_server.
  await new Promise<void>((resolve, reject) => {
    const readyPattern = /CLARINET_DAP_SDK_READY:(\d+)/;
    let stderrBuf = "";
    const timeout = setTimeout(
      () => reject(new Error("clarinet dap server did not start within 15 s")),
      15_000,
    );

    const cleanup = () => {
      clearTimeout(timeout);
      child.stderr!.removeListener("data", onStderr);
      child.removeListener("error", onError);
      child.removeListener("exit", onExit);
    };

    const onStderr = (chunk: Buffer) => {
      stderrBuf += chunk.toString("utf8");
      const match = readyPattern.exec(stderrBuf);
      if (match) {
        sdkPort = Number(match[1]);
        cleanup();
        resolve();
      }
    };

    const onError = (err: Error) => {
      cleanup();
      reject(new Error(`failed to spawn clarinet: ${err.message}`));
    };

    const onExit = (code: number | null) => {
      cleanup();
      reject(new Error(`clarinet dap exited unexpectedly with code ${code}`));
    };

    child.stderr!.on("data", onStderr);
    child.on("error", onError);
    child.on("exit", onExit);
  });

  if (sdkPort == null) {
    child.kill();
    throw new Error("clarinet dap did not report an SDK port");
  }

  const socket = await openSocket(sdkPort);
  return new DebugClient(socket, child);
}

// ---------------------------------------------------------------------------
// DebugSimnet — synchronous Simnet-shaped wrapper over the debug TCP server.
// ---------------------------------------------------------------------------

type MineBlockResponse = {
  stacksHeight: number;
  burnHeight: number;
  txs: Array<{ result: string; events: string }>;
};

function serializeTx(tx: Tx): unknown {
  if (tx.callPublicFn) {
    return {
      callPublicFn: {
        ...tx.callPublicFn,
        args: tx.callPublicFn.args.map((a) => Cl.stringify(a)),
      },
    };
  }
  if (tx.callPrivateFn) {
    return {
      callPrivateFn: {
        ...tx.callPrivateFn,
        args: tx.callPrivateFn.args.map((a) => Cl.stringify(a)),
      },
    };
  }
  // transferSTX and deployContract need no arg serialization
  return tx;
}

function parseTxResult(raw: { result: string; events: string }): ParsedTransactionResult {
  return {
    result: Cl.deserialize(raw.result),
    events: parseEvents(raw.events),
    costs: null,
    performance: undefined,
  };
}

/**
 * A synchronous, Simnet-shaped client backed by a `clarinet dap` debug server.
 *
 * All method calls are synchronous (they block via `Atomics.wait` on a SAB while
 * a background worker thread forwards them to the debug server over TCP).  This
 * lets existing test code that uses the global `simnet` run unmodified when the
 * VSCode "Debug" CodeLens triggers a debug session.
 *
 * Create with {@link DebugSimnet.connect} from an async vitest hook; all
 * subsequent simnet calls are synchronous and work transparently in test bodies.
 */
export class DebugSimnet {
  readonly deployer: string;
  private readonly _accounts: Map<string, string>;
  private _blockHeight: number;
  private _burnBlockHeight: number;

  private constructor(
    deployer: string,
    accounts: Map<string, string>,
    blockHeight: number,
    burnBlockHeight: number,
  ) {
    this.deployer = deployer;
    this._accounts = accounts;
    this._blockHeight = blockHeight;
    this._burnBlockHeight = burnBlockHeight;
  }

  /**
   * Connect to an already-running `clarinet dap --sdk-port <port>` server and
   * return a `DebugSimnet`.  The TCP connection is established asynchronously;
   * after this resolves all subsequent simnet calls are synchronous.
   */
  static async connect(port: number): Promise<DebugSimnet> {
    await connectSyncSocket(port);

    const accountsRaw = JSON.parse(syncSend({ id: 1, method: "getAccounts" }));
    if (accountsRaw.error) throw new Error(accountsRaw.error);
    const { deployer, accounts } = accountsRaw.result as {
      deployer: string;
      accounts: Record<string, string>;
    };

    const heightRaw = JSON.parse(syncSend({ id: 1, method: "blockHeight" }));
    if (heightRaw.error) throw new Error(heightRaw.error);
    const { stacksHeight, burnHeight } = heightRaw.result as {
      stacksHeight: number;
      burnHeight: number;
    };

    return new DebugSimnet(
      deployer,
      new Map(Object.entries(accounts)),
      stacksHeight,
      burnHeight,
    );
  }

  // --- Block height getters (kept in sync after each mineBlock) ---

  get blockHeight(): number {
    return this._blockHeight;
  }

  get stacksBlockHeight(): number {
    return this._blockHeight;
  }

  get burnBlockHeight(): number {
    return this._burnBlockHeight;
  }

  get currentEpoch(): string {
    return "4.0";
  }

  // --- Account / asset queries ---

  getAccounts(): Map<string, string> {
    return new Map(this._accounts);
  }

  getAssetsMap(): Map<string, Map<string, bigint>> {
    const raw = JSON.parse(syncSend({ id: 1, method: "getAssetsMap" }));
    if (raw.error) throw new Error(raw.error);
    const result = new Map<string, Map<string, bigint>>();
    for (const [asset, balances] of Object.entries(
      raw.result.assets as Record<string, Record<string, string>>,
    )) {
      const balMap = new Map<string, bigint>();
      for (const [addr, amount] of Object.entries(balances)) {
        balMap.set(addr, BigInt(amount));
      }
      result.set(asset, balMap);
    }
    return result;
  }

  // --- Contract calls (each routes through mineBlock to advance the chain) ---

  callPublicFn(
    contract: string,
    method: string,
    args: ClarityValue[],
    sender: string,
  ): ParsedTransactionResult {
    return this._mineBlock([{ callPublicFn: { contract, method, args, sender } }])[0];
  }

  callPrivateFn(
    contract: string,
    method: string,
    args: ClarityValue[],
    sender: string,
  ): ParsedTransactionResult {
    return this._mineBlock([{ callPrivateFn: { contract, method, args, sender } }])[0];
  }

  /**
   * Read-only calls do not advance the block.  They use the existing `call`
   * protocol method and deserialise the hex-encoded result.
   */
  callReadOnlyFn(
    contract: string,
    method: string,
    args: ClarityValue[],
    sender: string,
  ): ParsedTransactionResult {
    const argStrings = args.map((a) => Cl.stringify(a));
    const raw = JSON.parse(
      syncSend({ id: 1, method: "call", contract, function: method, args: argStrings, sender }),
    );
    if (raw.error) throw new Error(raw.error);
    const result = raw.result as { value: string; hex: string };
    return {
      result: Cl.deserialize(result.hex),
      events: [],
      costs: null,
      performance: undefined,
    };
  }

  execute(snippet: string): ParsedTransactionResult {
    const raw = JSON.parse(syncSend({ id: 1, method: "eval", snippet }));
    if (raw.error) throw new Error(raw.error);
    const result = raw.result as { value: string; hex: string };
    return {
      result: Cl.deserialize(result.hex),
      events: [],
      costs: null,
      performance: undefined,
    };
  }

  mineBlock(txs: Tx[]): ParsedTransactionResult[] {
    return this._mineBlock(txs);
  }

  transferSTX(amount: number | bigint, recipient: string, sender: string): ParsedTransactionResult {
    return this._mineBlock([
      { transferSTX: { amount: Number(amount), recipient, sender } },
    ])[0];
  }

  private _mineBlock(txs: Tx[]): ParsedTransactionResult[] {
    const serialized = txs.map(serializeTx);
    const raw = JSON.parse(syncSend({ id: 1, method: "mineBlock", txs: serialized }));
    if (raw.error) throw new Error(raw.error);
    const resp = raw.result as MineBlockResponse;
    this._blockHeight = resp.stacksHeight;
    this._burnBlockHeight = resp.burnHeight;
    return resp.txs.map(parseTxResult);
  }

  // --- Stubs for methods not relevant in debug mode ---

  deployContract(): ParsedTransactionResult {
    throw new Error("deployContract is not supported in debug mode");
  }

  getDataVar(): ClarityValue {
    throw new Error("getDataVar is not supported in debug mode");
  }

  getMapEntry(): ClarityValue {
    throw new Error("getMapEntry is not supported in debug mode");
  }

  getContractsInterfaces() {
    return new Map();
  }

  getContractSource(_contract: string): string | undefined {
    return undefined;
  }

  setEpoch(_epoch: string): void {}

  mineEmptyBlock(): number {
    return this._blockHeight;
  }

  mineEmptyBlocks(_count?: number): number {
    return this._blockHeight;
  }

  getBlockTime(): number {
    return 0;
  }

  executeCommand(_command: string): string {
    return "";
  }

  // Coverage / cost hooks are no-ops in debug mode.
  collectReport(
    _includeBootContracts: boolean,
    _bootContractsPath: string,
  ): { coverage: string; costs: string } {
    return { coverage: "", costs: "" };
  }

  setCurrentTestName(_name: string): void {}

  getLastContractCallTrace(): string | undefined {
    return undefined;
  }

  // initSession is a no-op: the debug server already has its session.
  async initSession(_cwd: string, _manifest: string): Promise<void> {}

  /** Disconnect from the debug server and terminate the worker. */
  disconnect(): void {
    try {
      syncSend({ id: 1, method: "disconnect" });
    } catch {
      // ignore errors during disconnect
    } finally {
      closeSyncSocket();
    }
  }
}
