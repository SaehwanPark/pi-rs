import { registerTool, registerCommand } from "@pi/agent";
import { internalCore } from "@pi/internal";

export function activate(context) {
  context.on("beforeTurn", () => {});
  registerTool("extract-table", {
    description: "Extract tables from documents"
  });
  registerCommand("custom-cmd", () => {});
}
