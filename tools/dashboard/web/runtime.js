// The page's only bridge to the engine: JSON in, JSON out over the 3-function WASM ABI
// (in_buf, handle, out_ptr). Shared by the page and by the Node smoke test.
async function createEngine(wasmBytes) {
  const { instance } = await WebAssembly.instantiate(wasmBytes, {});
  const ex = instance.exports;
  const encoder = new TextEncoder();
  const decoder = new TextDecoder();
  function call(request) {
    const bytes = encoder.encode(JSON.stringify(request));
    const at = ex.in_buf(bytes.length);
    // Views are made after each export call: the call may have grown (and moved) memory.
    new Uint8Array(ex.memory.buffer, at, bytes.length).set(bytes);
    const n = ex.handle();
    const out = new Uint8Array(ex.memory.buffer, ex.out_ptr(), n);
    return JSON.parse(decoder.decode(out));
  }
  return { call };
}
if (typeof module !== "undefined") module.exports = { createEngine };
