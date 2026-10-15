// Builds the single self-contained page: the base64 wasm and any number of script files inlined
// into the template.
//
// Usage: node web/inline.mjs <wasm | -> <template.html> <out.html> [NAME=script.js ...]
//
// `NAME=script.js` replaces the marker `/*__NAME_JS__*/` (NAME in A-Z, 0-9, _) with the file's text,
// for example `RUNTIME=web/runtime.js` fills `/*__RUNTIME_JS__*/`. The wasm replaces `__WASM_BASE64__`;
// `-` instead of a wasm path builds a page without one (the live page), whose template must not carry
// that marker.
// The match is strict in both directions: every marker the template names has exactly one file, every
// file has exactly one marker, and no marker is left in the page.
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const WASM_MARKER = "__WASM_BASE64__";
const markerOf = (name) => `/*__${name}_JS__*/`;
const NAME = /^[A-Z][A-Z0-9_]*$/;
const ANY_SCRIPT_MARKER = /\/\*__([A-Z][A-Z0-9_]*)_JS__\*\//g;

const occurrences = (text, part) => text.split(part).length - 1;

/** The page for `template`, with the wasm (base64, or null for none) and the `scripts` (name -> text) inlined. */
export function inline(template, b64, scripts) {
  const markers = new Map(b64 == null ? [] : [[WASM_MARKER, b64]]);
  if (b64 == null && template.includes(WASM_MARKER)) throw new Error(`template has ${WASM_MARKER} but no wasm was given`);
  for (const [name, text] of scripts) {
    if (!NAME.test(name)) throw new Error(`bad script name ${name}: use A-Z, 0-9 and _`);
    // A script sits inside a <script> tag: a closing tag in it would end the tag early.
    if (/<\/script/i.test(text)) throw new Error(`${name} contains </script and cannot be inlined`);
    markers.set(markerOf(name), text);
  }
  // Each marker is used exactly once.
  for (const marker of markers.keys()) {
    const n = occurrences(template, marker);
    if (n === 0) throw new Error(`template is missing ${marker}`);
    if (n > 1) throw new Error(`template has ${marker} ${n} times, expected once`);
  }
  // A marker in the template with no file would end up in the page as a comment: name it instead.
  for (const [marker] of template.matchAll(ANY_SCRIPT_MARKER)) {
    if (!markers.has(marker)) throw new Error(`template has ${marker} but no file was given for it`);
  }
  // One pass over the template, so inlined text is never scanned for markers. A function replacer:
  // the text is used verbatim (no `$&` patterns).
  const pattern = new RegExp([...markers.keys()].map((m) => m.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|"), "g");
  const html = template.replace(pattern, (marker) => markers.get(marker));
  // A marker still in the page was echoed by a script's text (or the wasm's): it would be left unfilled.
  for (const marker of new Set([...markers.keys(), WASM_MARKER])) {
    if (html.includes(marker)) throw new Error(`${marker} still occurs in the built page`);
  }
  return html;
}

function main() {
  const [wasmPath, templatePath, outPath, ...pairs] = process.argv.slice(2);
  if (!wasmPath || !templatePath || !outPath) {
    throw new Error("usage: node web/inline.mjs <wasm | -> <template.html> <out.html> [NAME=script.js ...]");
  }
  const scripts = new Map();
  for (const pair of pairs) {
    const eq = pair.indexOf("=");
    if (eq < 1) throw new Error(`expected NAME=script.js, got ${pair}`);
    const name = pair.slice(0, eq);
    if (scripts.has(name)) throw new Error(`${name} is given twice`);
    scripts.set(name, readFileSync(pair.slice(eq + 1), "utf8"));
  }
  const b64 = wasmPath === "-" ? null : readFileSync(wasmPath).toString("base64");
  const html = inline(readFileSync(templatePath, "utf8"), b64, scripts);
  writeFileSync(outPath, html);
  const wasm = b64 == null ? "no wasm" : `wasm ${Math.round(b64.length / 1024)} KiB as base64`;
  console.log(`wrote ${outPath}: ${Math.round(html.length / 1024)} KiB (${wasm}, ${scripts.size} script(s) inlined)`);
}

// Run as a program, not when the tests import `inline`.
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) main();
