// Minimal Pi-compatible bridge. Deliberately uses only stable Node APIs.
import { createInterface } from "node:readline";

// Extension logs must not corrupt the parent JSON-lines protocol.
console.log = (...args) => console.error(...args);

const tools = new Map();
const commands = new Map();
const listeners = new Map();

function collectUi() {
  const ui = [];
  const ctx = {
    ui: {
      notify(text, level = "info") { ui.push({ kind: "notify", text: String(text), value: level }); },
      setStatus(key, text) { ui.push({ kind: "status", key: String(key), text: String(text) }); },
      setWidget(key, value) { ui.push({ kind: "widget", key: String(key), value }); },
    },
  };
  return { ui, ctx };
}

const pi = {
  registerTool(definition) {
    if (!definition || typeof definition.name !== "string" || typeof definition.execute !== "function")
      throw new Error("registerTool requires name and execute");
    tools.set(definition.name, {
      name: definition.name,
      label: definition.label ?? null,
      description: String(definition.description ?? ""),
      parameters: definition.parameters ?? { type: "object", properties: {} },
      // Pi does not require these metadata fields; conservative defaults keep
      // unknown completion for extension tools that may have side effects.
      read_only: definition.readOnly === true || definition.read_only === true,
      idempotent: definition.idempotent === true,
      execute: definition.execute,
    });
  },
  registerCommand(name, definition = {}) {
    if (typeof name !== "string" || !definition || typeof definition.handler !== "function")
      throw new Error("registerCommand requires name and handler");
    commands.set(name, { name, description: definition.description ?? null, handler: definition.handler });
  },
  on(event, handler) {
    const selected = ["session_start", "session_shutdown", "turn_start", "turn_end", "tool_call", "tool_result", "context"];
    if (!selected.includes(event)) return;
    if (typeof handler !== "function") throw new Error(`handler for ${event} is not a function`);
    const current = listeners.get(event) ?? [];
    current.push(handler); listeners.set(event, current);
  },
};

function normalizeResult(result) {
  if (result === undefined || result === null) return {};
  if (typeof result === "string") return { text: result };
  if (typeof result === "object") {
    // Pi tools return content blocks. Keep only the model-visible text at this
    // boundary and avoid leaking arbitrary extension details into core state.
    if (Array.isArray(result.content)) {
      const text = result.content.filter((block) => block?.type === "text")
        .map((block) => String(block.text ?? "")).join("");
      return { ...result, text };
    }
    return { value: result, ...result };
  }
  return { value: result };
}

async function invoke(handler, arg, ctx, toolArgs = undefined) {
  const result = toolArgs === undefined ? await handler(arg, ctx) : await handler(arg, toolArgs, ctx);
  return normalizeResult(result);
}

async function handle(method, params) {
  if (method === "load") {
    for (const moduleUrl of params.modules ?? []) {
      const loaded = await import(moduleUrl);
      const factory = loaded.default ?? loaded.activate ?? loaded;
      if (typeof factory === "function") await factory(pi);
    }
    return {
      tools: [...tools.values()].map(({ execute, ...metadata }) => metadata),
      commands: [...commands.values()].map(({ handler, ...metadata }) => metadata),
    };
  }
  if (method === "call_tool") {
    const tool = tools.get(params.name);
    if (!tool) throw Object.assign(new Error(`unknown extension tool '${params.name}'`), { code: "UNKNOWN_TOOL" });
    const { ui, ctx } = collectUi();
    const onUpdate = (update) => ui.push({ kind: "tool_update", value: update });
    const result = await tool.execute(
      params.tool_call_id ?? "",
      params.arguments ?? {},
      null,
      onUpdate,
      ctx,
    );
    return { ...normalizeResult(result), ui };
  }
  if (method === "call_command") {
    const command = commands.get(params.name);
    if (!command) throw Object.assign(new Error(`unknown extension command '${params.name}'`), { code: "UNKNOWN_COMMAND" });
    const { ui, ctx } = collectUi();
    const commandArgs = typeof params.arguments === "string" ? params.arguments : (params.arguments ?? "");
    const result = await invoke(command.handler, commandArgs, ctx);
    return { ...result, ui };
  }
  if (method === "lifecycle" || method === "context") {
    const event = method === "context" ? "context" : params.event;
    const value = method === "context" ? params : (params.value ?? {});
    const { ui, ctx } = collectUi();
    let output = {};
    for (const handler of listeners.get(event) ?? []) {
      const result = await invoke(handler, value, ctx);
      if (result && typeof result === "object") output = { ...output, ...result };
    }
    return { ...output, ui };
  }
  throw Object.assign(new Error(`unknown method '${method}'`), { code: "UNKNOWN_METHOD" });
}

const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
for await (const line of input) {
  if (!line.trim()) continue;
  let request;
  try {
    request = JSON.parse(line);
    const result = await handle(request.method, request.params ?? {});
    process.stdout.write(JSON.stringify({ id: request.id, ok: true, result }) + "\n");
  } catch (error) {
    process.stdout.write(JSON.stringify({ id: request?.id ?? 0, ok: false,
      error: { message: String(error?.message ?? error), code: error?.code ?? null } }) + "\n");
  }
}
