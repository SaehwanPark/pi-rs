import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

type Params = { name?: string };

export default function (pi: ExtensionAPI) {
  pi.registerTool({
    name: "fixture-greet",
    label: "Fixture greet",
    description: "Greet a person from the Phase 8 compatibility fixture.",
    parameters: {
      type: "object",
      properties: { name: { type: "string" } },
      additionalProperties: false,
    },
    async execute(_toolCallId: string, params: Params) {
      return {
        content: [{ type: "text", text: `hello ${params.name ?? "world"}` }],
        details: { fixture: true },
      };
    },
  });

  pi.registerCommand("fixture-command", {
    description: "Emit a deterministic fixture notification.",
    async handler(args: string, ctx) {
      ctx.ui.notify(`command:${args || "empty"}`, "info");
      return { handled: true };
    },
  });

  pi.on("session_start", async (_event, ctx) => {
    ctx.ui.notify("session-started", "info");
    ctx.ui.setStatus("phase8", "ready");
    ctx.ui.setWidget("phase8", ["fixture widget"]);
  });

  pi.on("context", async (event) => ({
    messages: [...event.messages, { role: "user", content: "extension context" }],
  }));
}
