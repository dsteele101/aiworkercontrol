// Runs one task through the Codebuff SDK, for a Freebuff provider that has an
// API key. Run as: bun -e <this file> with the prompt on stdin and
// CODEBUFF_API_KEY and AIWC_CWD set. Prints one JSON object per line.
import { CodebuffClient } from "@codebuff/sdk";

const emit = (event) => console.log(JSON.stringify(event));
const prompt = (await Bun.stdin.text()).trim();
const client = new CodebuffClient({
  apiKey: process.env.CODEBUFF_API_KEY,
  cwd: process.env.AIWC_CWD,
});
try {
  const run = await client.run({
    agent: "base",
    prompt,
    handleEvent: (event) => {
      if (event?.type === "text" && event.text) {
        emit({ type: "text", text: event.text });
      } else if (event?.type === "tool_call") {
        emit({ type: "tool", name: String(event.toolName ?? ""), detail: "" });
      }
    },
  });
  const failed = run.output?.type === "error";
  emit({ type: "result", ok: !failed, summary: failed ? run.output.message : null });
  process.exit(failed ? 1 : 0);
} catch (error) {
  emit({ type: "result", ok: false, summary: String(error?.message ?? error) });
  process.exit(1);
}
