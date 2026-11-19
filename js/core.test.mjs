// The JavaScript API against the real core and a fake provider.
// CORE_BIN=<path to core> node --test js/core.test.mjs
import assert from "node:assert/strict";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { Core, CoreError } from "./core.mjs";
import { once } from "node:events";

const BIN = process.env.CORE_BIN ?? "core";

/** A fake Responses endpoint: each request gets the next reply (events, or `{ pause: ms }`). */
async function provider(replies) {
  const bodies = [];
  const server = http.createServer((req, res) => {
    let raw = "";
    req.on("data", (chunk) => { raw += chunk; });
    req.on("end", async (t) => {
      bodies.push(JSON.parse(raw));
      const reply = replies.shift() ?? [];
      res.writeHead(200, { "content-type": "text/event-stream" });
      for (const step of reply) {
        if (step.pause) await new Promise((r) => setTimeout(r, step.pause));
        else res.write(`data: ${JSON.stringify(step)}\n\n`);
      }
      res.end();
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return { base: `http://127.0.0.1:${server.address().port}`, bodies, close: () => server.close() };
}

const item = (item) => ({ type: "response.output_item.done", item });
const text = (t) => item({ type: "message", id: "msg_1", content: [{ type: "output_text", text: t }] });
const call = (id, name, args) => item({ type: "function_call", id: `fc_${id}`, call_id: id, name, arguments: JSON.stringify(args) });
const done = { type: "response.completed", response: { usage: { input_tokens: 10, output_tokens: 2 } } };

const ADD = { type: "function", name: "add", description: "Add two numbers",
  parameters: { type: "object", properties: { a: { type: "number" }, b: { type: "number" } }, required: ["a", "b"] } };

// Stopped after the test whatever happened, so a failure cannot leave the run hanging.
function start(base, handlers = {}, t, p) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "core-js-"));
  const core = Core.start({
    bin: BIN, dir: path.join(home, "session"), cwd: home,
    args: ["--auth", path.join(home, "auth.json"), "--catalog", path.join(home, "catalog"), "--agent-dir", home],
    env: { ...process.env, HOME: home },
    prepare: async () => ({ model: { api: "openai-responses", provider: "test", id: "m", baseUrl: base }, instructions: "Be terse.", tools: [] }),
    credentials: async () => ({ token: "t" }),
    ...handlers,
  });
  t?.after(async () => { await core.kill(); p?.close(); });
  return core;
}

test("a turn streams text, settles, and is saved", async (t) => {
  const delta = (d) => ({ type: "response.output_text.delta", delta: d });
  const p = await provider([[delta("hel"), delta("lo"), text("hello"), done]]);
  const core = start(p.base, {}, t, p);
  let streamed = "";
  const ends = [], names = [];
  core.on("event", (e) => names.push(e.ev));
  core.on("text_delta", (e) => { streamed += e.delta; assert.ok(Number.isInteger(e.attempt)); });
  core.on("message_end", (e) => ends.push(e));
  core.send("hi");
  assert.equal(core.busy, true);
  await core.idle();
  assert.equal(core.busy, false);
  assert.equal(streamed, "hello");
  assert.equal(ends[0].stopReason, "stop");
  assert.equal(ends[0].attempt, 1);
  assert.deepEqual(names.filter((n) => ["turn_start", "turn_end", "settled"].includes(n)), ["turn_start", "turn_end", "settled"]);
  const entries = await core.history({});
  assert.deepEqual(entries.map((e) => e.message?.role), ["user", "assistant"]);
  assert.equal(entries[1].message.content.find((b) => b.type === "text").text, "hello");
  await core.close(); p.close();
});

test("runner tools go to the tool handler and their results back to the model", async (t) => {
  const p = await provider([[call("c1", "add", { a: 2, b: 3 }), done], [text("5"), done]]);
  const seen = [];
  const core = start(p.base, {
    prepare: async () => ({ model: { api: "openai-responses", provider: "test", id: "m", baseUrl: p.base }, tools: [ADD] }),
    tool: async ({ name, arguments: args }) => {
      seen.push(name);
      return { content: [{ type: "text", text: String(args.a + args.b) }] };
    },
  }, t, p);
  core.send("add 2 and 3");
  await core.idle();
  assert.deepEqual(seen, ["add"]);
  const output = p.bodies[1].input.find((i) => i.type === "function_call_output");
  assert.equal(output.output, "5");
  await core.close(); p.close();
});

test("ops get their own replies; a refused op rejects without ending the turn", async (t) => {
  const p = await provider([[{ pause: 300 }, text("ok"), done]]);
  const core = start(p.base, {}, t, p);
  core.send("hi");
  // During the turn: a good record and a refused one (no ns).
  const n = await core.record("test.ns", { a: 1 });
  assert.ok(Number.isInteger(n));
  await assert.rejects(core.record(undefined, 1), (e) => e instanceof CoreError && /ns/.test(e.message));
  const turns = [];
  core.on("turn_end", (e) => turns.push(e.stopReason));
  await core.idle();
  assert.deepEqual(turns, ["stop"]);
  const ctx = await core.context();
  assert.equal(ctx.at(-1).role, "assistant");
  await core.close(); p.close();
});

test("idle waits for a message sent while a turn runs", async (t) => {
  const p = await provider([[{ pause: 200 }, text("one"), done], [text("two"), done]]);
  const core = start(p.base, {}, t, p);
  core.send("first");
  core.send("second");
  await core.idle();
  assert.equal(p.bodies.length, 2);
  await core.close(); p.close();
});

test("addContext wakes the model unless wake is false", async (t) => {
  const p = await provider([[text("seen"), done]]);
  const core = start(p.base, {}, t, p);
  const quiet = await core.addContext({ customType: "note", content: "internal" }, { wake: false });
  assert.ok(Number.isInteger(quiet));
  assert.equal(core.busy, false);
  await core.addContext({ customType: "family", content: "a message" });
  await core.idle();
  assert.equal(p.bodies.length, 1);
  await core.close(); p.close();
});

test("onAdded runs when the message is saved, before the turn it wakes is seen", async (t) => {
  const p = await provider([[text("seen"), done]]);
  const core = start(p.base, {}, t, p);
  const order = [];
  core.on("event", (event) => { if (["turn_start", "settled"].includes(event.ev)) order.push(event.ev); });
  const n = await core.addContext({ customType: "family", content: "a message" }, { onAdded: (event) => order.push(`added ${event.n}`) });
  await core.idle();
  assert.deepEqual(order, [`added ${n}`, "turn_start", "settled"]);
  await core.close(); p.close();
});

test("a settle says which sent inputs it covers", async (t) => {
  const p = await provider([[text("one"), done]]);
  const core = start(p.base, {}, t, p);
  const settles = [];
  core.on("settled", (event) => settles.push(event.through));
  const id = core.send("hi");
  assert.equal(core.covered(id, null), false);
  await core.idle();
  assert.equal(core.covered(id, settles.at(-1)), true);
  assert.equal(core.covered(`js${Number(id.slice(2)) + 1}`, settles.at(-1)), false, "a later op is not covered");
  await core.close(); p.close();
});

test("a failing prepare handler aborts the turn instead of hanging it", async (t) => {
  const p = await provider([]);
  const core = start(p.base, { prepare: async () => { throw new Error("no prompt"); } }, t, p);
  const failures = [];
  core.on("handler_error", (e) => failures.push(e.op));
  core.send("hi");
  await core.idle();
  assert.deepEqual(failures, ["prepared"]);
  assert.equal(p.bodies.length, 0);
  await core.close(); p.close();
});

test("an autonomous late-result turn reports busy and idle", async (t) => {
  const background = { ...call("c1", "add", {}), item: { ...call("c1", "add", {}).item, async: true } };
  const p = await provider([[background, done], [text("working"), done], [{ pause: 200 }, text("got result"), done]]);
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  const core = start(p.base, {
    prepare: async () => ({ model: { api: "openai-responses", provider: "test", id: "m", baseUrl: p.base }, tools: [ADD] }),
    tool: async () => { await gate; return { content: [{ type: "text", text: "late-output" }] }; },
  }, t, p);
  core.send("go");
  await core.idle();
  assert.equal(core.busy, false);
  const started = once(core, "turn_start");
  release();
  await started;
  assert.equal(core.busy, true);
  await core.idle();
  assert.equal(core.busy, false);
  const outputs = p.bodies[2].input.filter((item) => item.type === "function_call_output");
  assert.equal(outputs.length, 1);
  assert.equal(outputs[0].output, "late-output");
});
