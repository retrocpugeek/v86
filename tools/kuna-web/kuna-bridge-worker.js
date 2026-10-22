// kuna for gdb-wui in the guest (tools/kuna-web/build.sh): guest-web.js
// passes on what the guest's gdb-wui asks (gdb-wui's internal/ghidra/kuna.go,
// DialKuna) and this runs it, with kuna's own loader for its WebAssembly.
//
// Messages: {id, op: "has", sha256} -> {id, result: bool}
//           {id, op: "put", sha256, bytes} -> {id, result: true}
//           {id, op: "run", sha256, args: [command, ...]} -> {id, result: document}
//           ... or {id, error}
import { loadKuna } from "./kuna-web.js";

let kuna = null;
const binaries = new Map();

async function engine()
{
    kuna = kuna || loadKuna({
        wasmUrl: new URL("kuna_wasm.wasm", import.meta.url).href,
        specRoot: new URL("specs", import.meta.url).href,
    });
    return kuna;
}

// What gdb-wui reads of an inspect, and not the rest: each instruction's
// bytes and text, and every token, would be most of what goes to the guest.
function trim(doc)
{
    const f = doc && doc.function;
    if(!f) return doc;
    if(f.instructions) f.instructions = f.instructions.map(i => ({ address: i.address, size: i.size }));
    if(f.tokens) f.tokens = f.tokens.filter(t => t.kind === "variable" && t.address !== undefined)
        .map(t => ({ line: t.line, text: t.text, kind: t.kind, address: t.address }));
    for(const m of f.line_mappings || []) delete m.addresses_hex;
    for(const v of f.variables || []) delete v.addresses_hex;
    return doc;
}

async function handle({ op, sha256, bytes, args })
{
    switch(op)
    {
        case "has":
            return binaries.has(sha256);
        case "put":
            binaries.set(sha256, new Uint8Array(bytes));
            return true;
        case "run":
        {
            const binary = binaries.get(sha256);
            if(!binary) throw new Error("no binary " + sha256);
            const k = await engine();
            const [command, target] = args || [];
            switch(command)
            {
                case "list": return k.list(binary);
                case "inspect": return trim(await k.inspect(binary, target));
                case "decompile": return k.decompile(binary, target);
                default: throw new Error("not a command here: " + command);
            }
        }
        case "version":
            return fetch(new URL("version.json", import.meta.url)).then(r => r.json());
        default:
            throw new Error("unknown op " + op);
    }
}

self.onmessage = e =>
{
    const { id } = e.data;
    handle(e.data).then(
        result => self.postMessage({ id, result }),
        error => self.postMessage({ id, error: String(error && error.message || error) }));
};
