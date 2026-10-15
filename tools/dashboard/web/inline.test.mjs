// Tests of the inliner's markers and guards (no wasm, no files). Usage: node web/inline.test.mjs
import assert from "node:assert/strict";
import { inline } from "./inline.mjs";

let failed = 0;
function test(name, fn) {
  try {
    fn();
    console.log(`ok   ${name}`);
  } catch (e) {
    failed++;
    console.log(`FAIL ${name}: ${e.message}`);
  }
}
const scripts = (obj) => new Map(Object.entries(obj));
const page = '<script>/*__A_JS__*/</script><script>/*__B_JS__*/</script><i>"__WASM_BASE64__"</i>';

test("any number of scripts and the wasm are inlined, each where its marker was", () => {
  const html = inline(page, "QUJD", scripts({ A: "const a = 1;", B: "const b = 2;" }));
  assert.equal(html, '<script>const a = 1;</script><script>const b = 2;</script><i>"QUJD"</i>');
});

test("inlined text is verbatim: $ patterns are not expanded", () => {
  const html = inline(page, "x", scripts({ A: "'$&' + '$1' + '$$'", B: "" }));
  assert.ok(html.includes("<script>'$&' + '$1' + '$$'</script>"), html);
});

test("a template with a marker twice is refused", () => {
  assert.throws(() => inline(page + "/*__A_JS__*/", "x", scripts({ A: "1", B: "2" })), /\/\*__A_JS__\*\/ 2 times/);
  assert.throws(() => inline(page + "__WASM_BASE64__", "x", scripts({ A: "1", B: "2" })), /__WASM_BASE64__ 2 times/);
});

test("a file whose marker is not in the template is refused", () => {
  assert.throws(() => inline(page, "x", scripts({ A: "1", B: "2", C: "3" })), /missing \/\*__C_JS__\*\//);
  assert.throws(() => inline("<p></p>", "x", scripts({})), /missing __WASM_BASE64__/);
});

test("a marker in the template with no file is refused", () => {
  assert.throws(() => inline(page, "x", scripts({ A: "1" })), /\/\*__B_JS__\*\/ but no file/);
});

test("a script with a closing script tag is refused", () => {
  assert.throws(() => inline(page, "x", scripts({ A: "x = '</SCRIPT>'", B: "" })), /A contains <\/script/);
});

test("a marker echoed by a script's text is caught, in the later and in the earlier script", () => {
  assert.throws(() => inline(page, "x", scripts({ A: "'/*__B_JS__*/'", B: "2" })), /\/\*__B_JS__\*\/ still occurs/);
  assert.throws(() => inline(page, "x", scripts({ A: "1", B: "'/*__A_JS__*/'" })), /\/\*__A_JS__\*\/ still occurs/);
  assert.throws(() => inline(page, "x", scripts({ A: "'__WASM_BASE64__'", B: "2" })), /__WASM_BASE64__ still occurs/);
});

test("a bad script name is refused", () => {
  assert.throws(() => inline(page, "x", scripts({ a: "1" })), /bad script name a/);
});

console.log(failed ? `${failed} FAILED` : "all inliner tests passed");
process.exit(failed ? 1 : 0);
