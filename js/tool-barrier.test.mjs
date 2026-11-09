// Exercise the JavaScript handoff without a provider or core process.
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import test from "node:test";
import { Core } from "./core.mjs";

const tick = () => new Promise((resolve) => setImmediate(resolve));

test("a waiting handler writes its result only after earlier Python results", async () => {
  const child = new EventEmitter();
  child.stdin = new PassThrough(); child.stdout = new PassThrough();
  const writes = []; let raw = "";
  child.stdin.on("data", (chunk) => {
    raw += chunk;
    const lines = raw.split("\n"); raw = lines.pop();
    for (const line of lines) writes.push(JSON.parse(line));
  });
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  let core;
  core = new Core(child, { tool: async ({ name }) => {
    if (name === "ipython") { await pending; return { content: [{ type: "text", text: "real output" }] }; }
    await core.waitForToolHandlers("ipython");
    return { content: [{ type: "text", text: "waited" }] };
  } });
  const send = (event) => child.stdout.write(`${JSON.stringify(event)}\n`);
  send({ ev: "tool_call", call: "python", name: "ipython" });
  send({ ev: "tool_call", call: "wait", name: "wait_for_ipython" });
  await tick();
  assert.deepEqual(writes, [], "the barrier cannot answer while Python is running");
  release();
  await tick();
  assert.deepEqual(writes.map((entry) => entry.call), ["python", "wait"]);
  assert.equal(writes[0].result.content[0].text, "real output");
  assert.equal(writes[1].result.content[0].text, "waited");
  child.emit("exit", 0, null);
  child.stdin.destroy(); child.stdout.destroy();
});

test("aborting a waiting barrier cancels both handlers without a false result", async () => {
  const child = new EventEmitter();
  child.stdin = new PassThrough(); child.stdout = new PassThrough();
  const writes = []; let raw = "";
  child.stdin.on("data", (chunk) => {
    raw += chunk; const lines = raw.split("\n"); raw = lines.pop();
    for (const line of lines) writes.push(JSON.parse(line));
  });
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  const signals = [];
  let core;
  core = new Core(child, { tool: async ({ name }, signal) => {
    signals.push(signal);
    if (name === "ipython") await pending;
    else await core.waitForToolHandlers("ipython");
    return { content: [{ type: "text", text: "done" }] };
  } });
  const send = (event) => child.stdout.write(`${JSON.stringify(event)}\n`);
  send({ ev: "tool_call", call: "python", name: "ipython" });
  send({ ev: "tool_call", call: "wait", name: "wait_for_ipython" });
  await tick(); core.abort();
  assert.equal(signals.length, 2);
  assert(signals.every((signal) => signal.aborted));
  release(); await tick();
  assert.deepEqual(writes.map((entry) => entry.op), ["abort"]);
  child.emit("exit", 0, null);
  child.stdin.destroy(); child.stdout.destroy();
});
