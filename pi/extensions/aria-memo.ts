import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { registerAriaMemo } from "../src/memo.ts";

export default function ariaMemoExtension(pi: ExtensionAPI): void {
  const autoInject = process.env.ARIA_MEMO_AUTO_INJECT === "1";
  registerAriaMemo(pi, { autoInject });
}
