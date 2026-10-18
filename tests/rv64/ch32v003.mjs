#!/usr/bin/env node
// Runs the CH32V003 Tetris firmware (tools/ch32v003) on the ch32v003
// machine through the V86 API: it must draw its screen, start a game on a
// key press and score points for hard drops.
//
//   CH32_FIRMWARE=build/ch32v003/tetris.bin tests/rv64/ch32v003.mjs
import url from "node:url";

const __dirname = url.fileURLToPath(new URL(".", import.meta.url));
process.on("unhandledRejection", exn => { throw exn; });

const TEST_RELEASE_BUILD = +process.env.TEST_RELEASE_BUILD;
const { V86 } = await import(TEST_RELEASE_BUILD ? "../../build/libv86.mjs" : "../../src/main.js");

const firmware = process.env.CH32_FIRMWARE || __dirname + "/../../build/ch32v003/tetris.bin";

const emulator = new V86({
    arch: "ch32v003",
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
const fail = msg => { console.error("[ch32v003] FAIL: " + msg); process.exit(1); };
const score = () => { const m = [...out.matchAll(/\x1b\[10;27H(\d+)/g)]; return m.length ? +m[m.length - 1][1] : -1; };

const start = Date.now();
while(!out.includes("PRESS ANY KEY"))
{
    if(Date.now() - start > 10000) fail("no start screen");
    await sleep(50);
}
emulator.serial0_send("x");
await sleep(300);
if(score() !== 0) fail("no game started (score " + score() + ")");
for(let i = 0; i < 5; i++)
{
    emulator.serial0_send(i % 2 ? "\x1b[D " : "\x1b[C\x1b[A ");
    await sleep(150);
}
await sleep(300);
const s = score();
if(!(s > 0)) fail("no points after hard drops");
console.error(`[ch32v003] PASS: start screen, game, score ${s} after 5 hard drops (${Date.now() - start} ms)`);
emulator.destroy();
process.exit(0);
