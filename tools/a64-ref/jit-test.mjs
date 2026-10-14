#!/usr/bin/env node
// JIT self-test (src/a64/src/jit.rs selftest): random code run compiled and
// interpreted from the same random state for the same number of steps must
// end in the same state.
//
//   make build/v86-a64.wasm && node tools/a64-ref/jit-test.mjs [iterations] [seed]
//
// A module that fails to compile is written to jit-bad.wasm (try wasm2wat).
import fs from "node:fs";

const iterations = +(process.argv[2] || 20000);
const seed = +(process.argv[3] || 1);
const bytes = fs.readFileSync(new URL("../../build/v86-a64.wasm", import.meta.url));

let ex;
const env = {
    a64_mmio_read: () => 0,
    a64_mmio_write: () => {},
    a64_jit_compile(ptr, len, index, cookie)
    {
        const code = new Uint8Array(ex.memory.buffer, ptr, len).slice();
        try
        {
            const instance = new WebAssembly.Instance(new WebAssembly.Module(code),
                { "e": { "m": ex.memory, "r": ex.a64_jit_rd, "w": ex.a64_jit_wr, "t": ex.__indirect_function_table } });
            const table = ex.__indirect_function_table;
            if(table.length <= index) table.grow(index + 1 - table.length);
            table.set(index, instance.exports.f);
            return 1;
        }
        catch(e)
        {
            fs.writeFileSync("jit-bad.wasm", code);
            console.error("compile failed (module in jit-bad.wasm):", e.message);
            return 2;
        }
    },
};
ex = new WebAssembly.Instance(new WebAssembly.Module(bytes), { env }).exports;

const t0 = Date.now();
const len = ex.a64_jit_selftest(seed, iterations, ex.__indirect_function_table.length);
if(len)
{
    console.log(new TextDecoder().decode(new Uint8Array(ex.memory.buffer, ex.a64_jit_selftest_msg(), len)));
    console.log("FAIL");
    process.exit(1);
}
console.log(`PASS: ${iterations} runs (seed ${seed}) in ${Date.now() - t0} ms`);
