import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  pi.registerTool({
    name: "fixture-failing-tool",
    description: "A deliberately failing mutating fixture tool.",
    parameters: { type: "object", properties: {} },
    async execute() {
      throw new Error("fixture tool failure");
    },
  });

  pi.on("turn_end", async () => {
    throw new Error("fixture lifecycle failure");
  });
}
