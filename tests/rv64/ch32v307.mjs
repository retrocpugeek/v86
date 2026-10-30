#!/usr/bin/env node
// Runs RT-Thread (tools/ch32v307/build.sh) on the ch32v307 machine through
// the V86 API: its msh shell must come up on USART1 and answer commands,
// which needs the PFIC, SysTick (with its software interrupt for thread
// switches), QingKe's hardware register stacking and the USART's
// interrupts.
//
//   tools/ch32v307/build.sh && tests/rv64/ch32v307.mjs
import url from "node:url";

const __dirname = url.fileURLToPath(new URL(".", import.meta.url));
process.on("unhandledRejection", exn => { throw exn; });

const TEST_RELEASE_BUILD = +process.env.TEST_RELEASE_BUILD;
const { V86 } = await import(TEST_RELEASE_BUILD ? "../../build/libv86.mjs" : "../../src/main.js");

const firmware = process.env.CH32_FIRMWARE || __dirname + "/../../build/ch32v307/rtthread.bin";

const emulator = new V86({
    arch: "ch32v307",
    wasm_path: __dirname + "/../../build/v86-rv64.wasm",
    bios: { url: firmware },
    autostart: true,
    disable_keyboard: true,
    disable_mouse: true,
    disable_speaker: true,
    log_level: 0,
});

let out = "";
emulator.add_listener("serial0-output-byte", b => { out += String.fromCharCode(b); });

const sleep = ms => new Promise(r => setTimeout(r, ms));
const fail = msg => { console.error("[ch32v307] FAIL: " + msg + "\n" + out.slice(-600)); process.exit(1); };
const until = async (re, ms, what) =>
{
    const end = Date.now() + ms;
    while(!re.test(out))
    {
        if(Date.now() > end) fail("timed out waiting for " + what);
        await sleep(50);
    }
};

const start = Date.now();
await until(/msh >/, 10000, "the shell prompt");
emulator.serial0_send("ps\r");
await until(/tshell\s+11\s+running/, 5000, "ps");
emulator.serial0_send("free\r");
await until(/available:\s*\d+/, 5000, "free");
// Lua (a demo package), and back to the shell with Ctrl-D
emulator.serial0_send("lua\r");
await until(/Lua 5\.1\.4/, 5000, "the Lua prompt");
emulator.serial0_send("print(string.format('six times seven: %d', 6 * 7))\r");
await until(/six times seven: 42/, 5000, "Lua's answer");
emulator.serial0_send("\x04");
await sleep(300);
// The LED thread (main.c) toggles PB5 every 500 ms
let toggles = 0, last = -1;
for(let i = 0; i < 30; i++)
{
    const v = emulator.get_gpio_out(1) >> 5 & 1;
    if(last >= 0 && v !== last) toggles++;
    last = v;
    await sleep(50);
}
if(toggles < 2) fail("LED1 (PB5) didn't blink");
console.error(`[ch32v307] PASS: msh prompt, ps, free, Lua, LED blinking (${Date.now() - start} ms)`);
emulator.destroy();
process.exit(0);
