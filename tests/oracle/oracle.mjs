// Differential check: the official MCP client against the official reference server,
// once directly and once through mcphive with two clients at the same time.
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import assert from "node:assert/strict";
import path from "node:path";
import url from "node:url";

const here = path.dirname(url.fileURLToPath(import.meta.url));
const server = path.join(here, "node_modules/@modelcontextprotocol/server-everything/dist/index.js");
const mcphive = process.argv[2];
const namespace = `oracle_${process.pid}`;
const env = { ...process.env, MCPHIVE_NAMESPACE: namespace };

async function connect(command, args) {
  const transport = new StdioClientTransport({ command, args, env, stderr: "ignore" });
  const client = new Client({ name: "oracle", version: "1.0.0" }, { capabilities: {} });
  await client.connect(transport);
  return client;
}

// What the same session does, written down as plain data so that runs can be compared.
async function session(client) {
  const seen = {};
  seen.server = client.getServerVersion()?.name;
  seen.tools = (await client.listTools()).tools.map((t) => t.name).sort();
  seen.echo = (await client.callTool({ name: "echo", arguments: { message: "hello" } })).content;
  seen.add = (await client.callTool({ name: "get-sum", arguments: { a: 2, b: 40 } })).content;
  const progress = [];
  const long = await client.callTool(
    { name: "trigger-long-running-operation", arguments: { duration: 1, steps: 4 } },
    undefined,
    { onprogress: (p) => progress.push(p.progress), timeout: 20000 },
  );
  seen.long = long.content;
  // The SDK delivers a notification a tick later than a response, so one that is read
  // in the same chunk as the answer is lost (also without mcphive). The exact sequences
  // are checked below with raw messages.
  seen.progress = progress.length > 0 && progress.every((p, i) => p === i + 1);
  seen.resources = (await client.listResources()).resources.length;
  seen.prompts = (await client.listPrompts()).prompts.map((p) => p.name).sort();
  const failure = await client.callTool({ name: "no-such-tool", arguments: {} }).then(
    (r) => ({ isError: r.isError }),
    (e) => ({ error: String(e.code ?? e.message) }),
  );
  seen.failure = failure;
  return seen;
}

setTimeout(() => { console.error("TIMEOUT: the check did not finish in 90 s"); process.exit(3); }, 90000).unref();
const direct = await connect("node", [server, "stdio"]);
const expected = await session(direct);
await direct.close();
assert.equal(expected.progress, true, "the direct run has no progress");
console.log("direct:", JSON.stringify({ ...expected, tools: expected.tools.length }));

const through = () => connect(mcphive, ["run", "--idle", "2", "--", "node", server, "stdio"]);
const [a, b] = await Promise.all([through(), through()]);
const [seenA, seenB] = await Promise.all([session(a), session(b)]);
for (const [name, seen] of [["A", seenA], ["B", seenB]]) {
  assert.deepEqual(seen, expected, `client ${name} saw something different through mcphive`);
}
console.log("two clients through mcphive: identical to the direct run");

// A client that arrives later gets the same answers from the running server.
const c = await through();
assert.deepEqual(await session(c), expected, "a later client saw something different");
console.log("a third client, later: identical");
await Promise.all([a.close(), b.close(), c.close()]);

// Exact progress with raw messages, two clients at once, the same id and the same token.
import { spawn } from "node:child_process";
function rawClient() {
  const child = spawn(mcphive, ["run", "--idle", "2", "--", "node", server, "stdio"], { env, stdio: ["pipe", "pipe", "inherit"] });
  const result = { progress: [], answer: null, wrongToken: 0 };
  let buffer = "";
  const finished = new Promise((resolve, reject) => {
    child.on("exit", (code) => { if (result.answer === null) reject(new Error(`a raw shim exited with ${code} before its answer`)); });
    child.stdout.on("data", (d) => {
      buffer += d;
      let i;
      while ((i = buffer.indexOf("\n")) >= 0) {
        const m = JSON.parse(buffer.slice(0, i));
        buffer = buffer.slice(i + 1);
        if (m.method === "notifications/progress") {
          if (m.params.progressToken !== "tok") result.wrongToken++;
          result.progress.push(m.params.progress);
        }
        if (m.id === 2) { result.answer = m.result?.content?.[0]?.text; setTimeout(resolve, 300); }
      }
    });
  });
  const send = (m) => child.stdin.write(JSON.stringify(m) + "\n");
  send({ jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "raw", version: "0" } } });
  send({ jsonrpc: "2.0", method: "notifications/initialized" });
  send({ jsonrpc: "2.0", id: 2, method: "tools/call", params: { name: "trigger-long-running-operation", arguments: { duration: 1, steps: 4 }, _meta: { progressToken: "tok" } } });
  return { result, finished, close: () => child.stdin.end() };
}
const raws = [rawClient(), rawClient()];
await Promise.all(raws.map((r) => r.finished));
for (const [i, r] of raws.entries()) {
  assert.deepEqual(r.result.progress, [1, 2, 3, 4], `raw client ${i} progress`);
  assert.equal(r.result.wrongToken, 0);
  assert.match(r.result.answer, /Steps: 4/);
  r.close();
}
console.log("raw clients, same id and token at once: exactly [1,2,3,4] each");
console.log("OK");
