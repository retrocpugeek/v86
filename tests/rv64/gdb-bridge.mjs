#!/usr/bin/env node
// A CH32V microcontroller through the V86 API with its GDB stub on a TCP
// port, the way the site's pages relay a guest's gdb to it (gdb_attach,
// gdb_input): the hart is held at reset until gdb connects, like rv64run
// --gdb ADDR --gdb-wait. tests/rv64/gdb-stub.sh runs it with STUB=node.
//
//   tests/rv64/gdb-bridge.mjs ch32v003 build/ch32v003/tetris.bin 3333
import net from "node:net";
import url from "node:url";

const __dirname = url.fileURLToPath(new URL(".", import.meta.url));
process.on("unhandledRejection", exn => { throw exn; });

const TEST_RELEASE_BUILD = +process.env.TEST_RELEASE_BUILD;
const { V86 } = await import(TEST_RELEASE_BUILD ? "../../build/libv86.mjs" : "../../src/main.js");

const [arch, firmware, port] = process.argv.slice(2);
const emulator = new V86({
    arch,
    wasm_path: __dirname + "/../../build/v86-rv64.wasm",
    bios: { url: firmware },
    autostart: false,
    disable_keyboard: true,
    disable_mouse: true,
    disable_speaker: true,
    log_level: 0,
});

await new Promise(resolve => emulator.add_listener("emulator-loaded", resolve));
let started = false;
const server = net.createServer(socket =>
{
    socket.setNoDelay(true);
    if(!emulator.gdb_attach(bytes => socket.write(bytes)))
    {
        console.error("gdb-bridge: no GDB stub for " + arch);
        process.exit(1);
    }
    console.error("gdb-bridge: gdb connected");
    if(!started)
    {
        started = true;
        emulator.run();
    }
    socket.on("data", data => emulator.gdb_input(new Uint8Array(data)));
    socket.on("close", () =>
    {
        console.error("gdb-bridge: gdb disconnected");
        emulator.gdb_detach();
    });
});
server.listen(+port, "127.0.0.1", () => console.error("gdb-bridge: GDB stub on 127.0.0.1:" + port));
