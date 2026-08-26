/**
 * Finding 29 of the PR #2483 review: the relay answers `initialize` itself, but
 * nests the capabilities one level too deep.
 *
 * DAP defines the body of an `InitializeResponse` as the `Capabilities` object
 * itself:
 *
 *   { "type": "response", "command": "initialize",
 *     "body": { "supportsConfigurationDoneRequest": true, ... } }
 *
 * `INITIALIZE_BODY` in `debug/debug.ts` wraps them in an extra `capabilities`
 * property, so a client sees no advertised capabilities at all. That matters
 * most for `supportsConfigurationDoneRequest`: `init_attach` waits for
 * `configurationDone`, but the client is never told the adapter supports it —
 * which is the mechanism behind finding 5's reliance on a `threads` request.
 *
 * The Rust adapter emits the same nested shape through
 * `debug_types::InitializeResponse`; see the clarity-repl test
 * `the_initialize_response_advertises_capabilities_where_dap_expects_them`.
 * Both paths need fixing together so launch and attach advertise the same valid
 * response.
 *
 *   pnpm run build:dap && pnpm run test:dap
 *
 * https://microsoft.github.io/debug-adapter-protocol/specification#Requests_Initialize
 */

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const ADAPTER = fileURLToPath(new URL("../dist/debug.js", import.meta.url));
const REPLY_TIMEOUT_MS = 2_000;

if (!existsSync(ADAPTER)) {
  throw new Error(
    `debug adapter bundle not found at ${ADAPTER}\n` +
      `Build it first: pnpm --dir components/clarity-vscode run build:dap`,
  );
}

function frame(message) {
  const body = JSON.stringify(message);
  return `Content-Length: ${Buffer.byteLength(body, "utf8")}\r\n\r\n${body}`;
}

function firstMessage(buffer) {
  const headerEnd = buffer.indexOf("\r\n\r\n");
  if (headerEnd === -1) return undefined;
  const match = /Content-Length: (\d+)/i.exec(buffer.subarray(0, headerEnd).toString("ascii"));
  if (!match) return undefined;
  const bodyStart = headerEnd + 4;
  const bodyEnd = bodyStart + Number(match[1]);
  if (buffer.length < bodyEnd) return undefined;
  return JSON.parse(buffer.subarray(bodyStart, bodyEnd).toString("utf8"));
}

async function waitFor(predicate, ms) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    const value = predicate();
    if (value !== undefined) return value;
    await new Promise((r) => setTimeout(r, 25));
  }
  return undefined;
}

test("the initialize response advertises capabilities where DAP expects them", async (t) => {
  const child = spawn(process.execPath, [ADAPTER], { stdio: ["pipe", "pipe", "pipe"] });
  let stdout = Buffer.alloc(0);
  let stderr = "";
  child.stdout.on("data", (chunk) => (stdout = Buffer.concat([stdout, chunk])));
  child.stderr.on("data", (chunk) => (stderr += chunk.toString("utf8")));
  t.after(() => child.kill("SIGKILL"));

  child.stdin.write(
    frame({
      seq: 1,
      type: "request",
      command: "initialize",
      arguments: { adapterID: "clarinet", clientID: "vscode", pathFormat: "path" },
    }),
  );

  const response = await waitFor(() => firstMessage(stdout), REPLY_TIMEOUT_MS);
  assert.ok(response, `the adapter did not answer initialize; stderr: ${stderr}`);

  assert.equal(
    response.body?.supportsConfigurationDoneRequest,
    true,
    "capabilities must sit directly in `body`, not nested under " +
      `\`body.capabilities\`; the relay sent ${JSON.stringify(response.body)}`,
  );
});
