// Builds the single self-contained page: runtime.js and the base64 wasm inlined into the template.
// Usage: node web/inline.mjs <wasm> <template.html> <runtime.js> <out.html>
import { readFileSync, writeFileSync } from "node:fs";

const [wasmPath, templatePath, runtimePath, outPath] = process.argv.slice(2);
const RUNTIME_MARKER = "/*__RUNTIME_JS__*/";
const WASM_MARKER = "__WASM_BASE64__";

let html = readFileSync(templatePath, "utf8");
for (const marker of [RUNTIME_MARKER, WASM_MARKER]) {
  if (!html.includes(marker)) throw new Error(`template is missing ${marker}`);
}
const runtime = readFileSync(runtimePath, "utf8");
// runtime.js sits inside a <script> tag: a closing tag in it would end the script early.
if (/<\/script/i.test(runtime)) throw new Error("runtime.js contains </script and cannot be inlined");
const b64 = readFileSync(wasmPath).toString("base64");
// The wasm goes first so the runtime text, inserted afterwards, can never be hit by its marker.
// Function replacers: the inlined text is used verbatim (no `$&` patterns).
html = html.replace(WASM_MARKER, () => b64);
html = html.replace(RUNTIME_MARKER, () => runtime);
// Each marker must have been used exactly once: a leftover means a duplicate or an echo in the inlined text.
for (const marker of [RUNTIME_MARKER, WASM_MARKER]) {
  if (html.includes(marker)) throw new Error(`${marker} still occurs in the built page`);
}
writeFileSync(outPath, html);
console.log(`wrote ${outPath}: ${Math.round(html.length / 1024)} KiB (wasm ${Math.round(b64.length / 1024)} KiB as base64)`);
