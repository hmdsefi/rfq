// Builds the single self-contained page: runtime.js and the base64 wasm inlined into the template.
// Usage: node web/inline.mjs <wasm> <template.html> <runtime.js> <out.html>
import { readFileSync, writeFileSync } from "node:fs";

const [wasmPath, templatePath, runtimePath, outPath] = process.argv.slice(2);
let html = readFileSync(templatePath, "utf8");
for (const marker of ["/*__RUNTIME_JS__*/", "__WASM_BASE64__"]) {
  if (!html.includes(marker)) throw new Error(`template is missing ${marker}`);
}
const b64 = readFileSync(wasmPath).toString("base64");
// Function replacers: the inlined text is used verbatim (no `$&` patterns).
html = html.replace("/*__RUNTIME_JS__*/", () => readFileSync(runtimePath, "utf8"));
html = html.replace("__WASM_BASE64__", () => b64);
writeFileSync(outPath, html);
console.log(`wrote ${outPath}: ${Math.round(html.length / 1024)} KiB (wasm ${Math.round(b64.length / 1024)} KiB as base64)`);
