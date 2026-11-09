// The core's JavaScript API: one session (`core run`) as an object with plain functions.
// It hides the JSON lines: requests get ids and promises, and the core's questions
// (prepare_request, need_credentials, review_request, runner tool calls) go to handlers.
//
//   const core = Core.start({ bin, dir, args, cwd, env,
//     prepare: async ({ context, selected, contextFiles }) => ({ model, instructions, tools, ... }),
//     credentials: async (provider) => ({ token, ... }),      // when the core holds no sign-in
//     review: async (request) => ({ body, headers }),          // with prepared.reviewRequest
//     tool: async ({ call, name, arguments, async }, signal) => ({ content, isError, details }),
//   });
//   core.on("text_delta", (event) => ...);   // every core event, by its name
//   core.send("hello"); await core.idle();
import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import readline from "node:readline";

/** An op the core refused (its `error` event). */
export class CoreError extends Error {
  constructor(message, op) { super(message); this.name = "CoreError"; this.op = op; }
}

// Events that answer an op and echo its id.
const REPLY_EVENTS = new Set(["recorded", "model_set", "history", "models", "added", "logged_out", "auth_status", "completed"]);
// Ops answered by an event without the op's id, in the order sent.
const ORDERED = { context: "context", checkpoint: "checkpointed" };

export class Core extends EventEmitter {
  #child; #lines; #handlers; #next = 0; #pending = new Map(); #ordered = new Map();
  #tools = new Map(); #turn = null; #exited;

  /** Start `core run <dir>`. `args` are its flags (--models, --auth, --no-tool, ...). */
  static start({ bin = "core", dir, args = [], cwd, env, ...handlers }) {
    if (!dir) throw new TypeError("dir is required");
    const child = spawn(bin, ["run", dir, ...args], { cwd, env, stdio: ["pipe", "pipe", "inherit"] });
    return new Core(child, handlers);
  }

  constructor(child, handlers = {}) {
    super();
    this.#child = child;
    this.#handlers = handlers;
    this.#exited = new Promise((resolve) => child.on("exit", (code, signal) => {
      this.#failAll(new CoreError(`core exited (${signal ?? code})`));
      this.emit("exit", { code, signal });
      resolve({ code, signal });
    }));
    child.on("error", (error) => this.#failAll(error));
    child.stdin.on("error", (error) => this.#failAll(error));
    this.#lines = readline.createInterface({ input: child.stdout });
    this.#lines.on("line", (line) => {
      let event;
      try { event = JSON.parse(line); } catch { this.emit("protocol_error", line); return; }
      this.#receive(event);
    });
  }

  // ---- turns ----

  /** Start a turn with a user message (text, or `{ content }` blocks). */
  send(input) { const id = this.#id(); this.#turnStarted(id); this.#write({ op: "send", id, input: userInput(input) }); }
  /** Add a user message to the running turn at its next safe point (or start one when idle). */
  steer(input) { const id = this.#id(); this.#turnStarted(id); this.#write({ op: "steer", id, input: userInput(input) }); }
  /** Stop the running turn; unfinished tool calls are cancelled. */
  abort() { for (const run of this.#tools.values()) run.controller.abort(); this.#write({ op: "abort", id: this.#id() }); }
  /** Resolves once the core has settled after every turn-starting op sent so far. */
  idle() { return this.#turn?.promise ?? Promise.resolve(); }
  get busy() { return this.#turn !== null; }

  /** pi's custom message: the model sees it. Wakes the model unless `wake: false`. */
  addContext(message, { wake, deliver } = {}) {
    return this.#request("add_context", { message, ...(wake === false ? { wake } : {}), ...(deliver ? { deliver } : {}) },
      (id) => { if (wake !== false) this.#turnStarted(id); })
      .then((event) => event.n);
  }

  /** Wait for earlier handlers of this tool to return and write any results. */
  async waitForToolHandlers(name) {
    const earlier = [...this.#tools.values()].filter((run) => run.name === name).map((run) => run.done);
    await Promise.all(earlier);
  }

  /** Give the result of an async runner call (normal calls return it from the `tool` handler). */
  toolResult(call, result) { this.#write({ op: "tool_result", call, result }); }

  // ---- anytime ----

  /** pi's custom entry: bookkeeping the model never sees. Resolves to its record number. */
  record(ns, data) { return this.#request("record", { ns, data }).then((event) => event.n); }
  /** Choose model (`provider`, `model`) and/or `reasoning`. */
  setModel({ provider, model, reasoning } = {}) {
    return this.#request("set_model", { provider, model, reasoning }).then(({ model, reasoning }) => ({ model, reasoning }));
  }
  /** Complete history entries: the current chunk, or ids `from`..`to` (to continue: last id + 1). */
  history({ from, to } = {}) {
    return this.#request("history", { from, to }).then((event) => event.entries);
  }
  models(provider) { return this.#request("models", { provider }).then((event) => event.models); }
  /** One request outside the conversation, never saved (pi's completeSimple). */
  complete({ model, instructions, messages, reasoning }) {
    return this.#request("complete", { model, instructions, messages, reasoning }).then((event) => {
      if (event.error) throw new CoreError(event.error, "complete");
      return event.message;
    });
  }
  logout(provider) { return this.#request("logout", { provider }).then((event) => event.removed); }
  authStatus() { return this.#request("auth_status", {}).then((event) => event.signedIn); }
  /** Sign in. `onStep` gets each `login` event (device code, page to open, done, failed). */
  login(provider, { method, key, onStep } = {}) {
    const id = this.#id();
    return new Promise((resolve, reject) => {
      const listener = (event) => {
        if (event.id !== id) return;
        onStep?.(event);
        if (event.step === "done") { this.off("login", listener); resolve(event); }
        if (event.step === "failed") { this.off("login", listener); reject(new CoreError(event.error ?? "sign-in failed", "login")); }
      };
      this.on("login", listener);
      this.#write({ op: "login", id, provider, ...(method ? { method } : {}), ...(key ? { key } : {}) });
    });
  }
  /** The address a browser sign-in ended on, pasted by the person. */
  loginInput(id, input) { this.#write({ op: "login_input", id, input }); }

  // ---- idle only (queued until the running turn ends) ----

  /** The context the next request would send. */
  context() { return this.#orderedRequest("context", {}).then((event) => event.messages); }
  /** Make `messages` the context (a checkpoint). Resolves to the checkpoint id. */
  checkpoint(messages) { return this.#orderedRequest("checkpoint", { messages }).then((event) => event.id); }

  /** Close stdin; the core finishes running built-in calls and exits. */
  async close() {
    this.#child.stdin.end();
    return this.#exited;
  }
  kill(signal = "SIGTERM") { this.#child.kill(signal); return this.#exited; }

  // ---- internals ----

  #id() { return `js${++this.#next}`; }
  #write(op) { this.#child.stdin.write(`${JSON.stringify(op)}\n`); }

  #request(op, fields, beforeWrite) {
    const id = this.#id();
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { op, resolve, reject });
      beforeWrite?.(id);
      this.#write({ op, id, ...stripUndefined(fields) });
    });
  }

  #orderedRequest(op, fields) {
    const id = this.#id();
    return new Promise((resolve, reject) => {
      const queue = this.#ordered.get(ORDERED[op]) ?? [];
      queue.push({ id, op, resolve, reject });
      this.#ordered.set(ORDERED[op], queue);
      this.#write({ op, id, ...fields });
    });
  }

  // Busy from a turn-starting op until a `settled` whose `through` reaches that op:
  // ops are taken in the order written, so an earlier `through` means it is still to come.
  #turnStarted(id) {
    if (this.#turn) { this.#turn.waitFor = id; return; }
    let resolve;
    const promise = new Promise((r) => { resolve = r; });
    this.#turn = { promise, resolve, waitFor: id };
  }

  #settled(through) {
    const turn = this.#turn;
    if (!turn || sequence(through) < sequence(turn.waitFor)) return;
    this.#turn = null;
    turn.resolve();
  }

  #failAll(error) {
    for (const [id, waiter] of this.#pending) { this.#pending.delete(id); waiter.reject(error); }
    for (const queue of this.#ordered.values()) for (const waiter of queue.splice(0)) waiter.reject(error);
    for (const run of this.#tools.values()) run.controller.abort();
    const turn = this.#turn; this.#turn = null; turn?.resolve();
  }

  #settle(id, event) {
    const waiter = this.#pending.get(id);
    if (!waiter) return false;
    this.#pending.delete(id);
    waiter.resolve(event);
    return true;
  }

  #receive(event) {
    const name = event.ev;
    switch (name) {
      case "prepare_request": void this.#answer("prepared", event.id, () => this.#need("prepare")(event)); break;
      case "need_credentials":
        void this.#answer("credentials", event.id, () => this.#need("credentials")(event.provider, event));
        break;
      case "review_request": void this.#answer("review_request", event.id, async () => (await this.#handlers.review?.(event.request)) ?? {}); break;
      case "tool_call": if (!event.builtin) this.#runTool(event); break;
      case "cancel": for (const call of event.calls ?? []) this.#tools.get(call)?.controller.abort(); break;
      case "settled": this.#settled(event.through); break;
      case "error":
        if (event.id != null) this.#rejectId(event.id, new CoreError(event.error, event.op));
        break;
      default:
        if (this.#ordered.get(name)?.length) this.#ordered.get(name).shift().resolve(event);
        else if (event.id != null && REPLY_EVENTS.has(name)) this.#settle(event.id, event);
    }
    // `error` events stay on "core_error": an EventEmitter "error" without a listener throws.
    this.emit(name === "error" ? "core_error" : name, event);
    this.emit("event", event);
  }

  #rejectId(id, error) {
    const waiter = this.#pending.get(id);
    if (waiter) { this.#pending.delete(id); waiter.reject(error); return true; }
    for (const queue of this.#ordered.values()) {
      const index = queue.findIndex((item) => item.id === id);
      if (index >= 0) { queue.splice(index, 1)[0].reject(error); return true; }
    }
    return false;
  }

  #need(name) {
    const handler = this.#handlers[name];
    if (typeof handler !== "function") throw new CoreError(`no ${name} handler`, name);
    return handler;
  }

  // The core waits for this answer; a failing handler aborts the turn rather than hang it.
  async #answer(op, id, produce) {
    let answer;
    try { answer = await produce(); }
    catch (error) {
      this.emit("handler_error", { op, error });
      this.abort();
      return;
    }
    this.#write({ ...answer, op, id });
  }

  async #runTool(event) {
    const controller = new AbortController();
    let finished;
    const done = new Promise((resolve) => { finished = resolve; });
    this.#tools.set(event.call, { controller, name: event.name, done });
    let result;
    try {
      const handler = this.#handlers.tool;
      if (typeof handler !== "function") throw new Error(`No tool handler for ${event.name}`);
      result = await handler(event, controller.signal);
    } catch (error) {
      result = { content: [{ type: "text", text: error instanceof Error ? error.message : String(error) }], isError: true };
    }
    try {
      // The pipe is FIFO: a waiting handler may answer only after this write.
      // A cancelled call already has its result ("Cancelled.").
      if (!controller.signal.aborted && result !== undefined) this.toolResult(event.call, result);
    } finally { this.#tools.delete(event.call); finished(); }
  }
}

function userInput(input) {
  if (typeof input === "string") return { text: input };
  if (input && typeof input === "object") return input;
  throw new TypeError("input must be text or { content }");
}

// The library's op ids are "js<N>", written in increasing N.
function sequence(id) {
  const match = typeof id === "string" ? /^js(\d+)$/.exec(id) : null;
  return match ? Number(match[1]) : -1;
}

function stripUndefined(fields) {
  return Object.fromEntries(Object.entries(fields).filter(([, value]) => value !== undefined));
}
