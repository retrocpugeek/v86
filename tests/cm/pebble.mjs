#!/usr/bin/env node
// Boots PebbleOS (built for the qemu_flint emulator board) on the Cortex-M
// core (src/cm, build/v86-cm.wasm) through the V86 API, in real time: its
// display must come on with the launcher, the Back button must show the
// watchface and Down then Select must open an app.
//
//   PEBBLE_ELF=.../pebbleos.elf PEBBLE_SPI=.../qemu_spi_flash.bin[.gz] node tests/cm/pebble.mjs [screenshot-prefix]
//
// Needs build/libv86.mjs and build/v86-cm.wasm (make build/libv86.mjs build/v86-cm.wasm).
import fs from "node:fs";
import url from "node:url";
import zlib from "node:zlib";

const __dirname = url.fileURLToPath(new URL(".", import.meta.url));
process.on("unhandledRejection", exn => { throw exn; });
const { V86 } = await import("../../build/libv86.mjs");

const OUT = process.argv[2];
const read = f => { const b = fs.readFileSync(f); return f.endsWith(".gz") ? zlib.gunzipSync(b) : b; };
const elf = read(process.env.PEBBLE_ELF);
const spi = read(process.env.PEBBLE_SPI);
const ab = b => b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);

const emulator = new V86({
    arch: "pebble-flint",
    wasm_path: __dirname + "/../../build/v86-cm.wasm",
    bios: { buffer: ab(elf) },
    spi_flash: { buffer: ab(spi) },
    autostart: true,
    disable_keyboard: true,
    disable_mouse: true,
    disable_speaker: true,
    log_level: 0,
});

let log = "";
emulator.add_listener("cortex-m-uart2", bytes => { log += Buffer.from(bytes).toString("latin1"); });

const sleep = ms => new Promise(r => setTimeout(r, ms));
const fail = msg => { console.error("[pebble] FAIL: " + msg); process.exit(1); };
const t0 = Date.now();
const elapsed = () => ((Date.now() - t0) / 1000).toFixed(1) + " s";

// The screen: a fingerprint, and how many pixels are dark
const screen = name =>
{
    const d = emulator.get_watch_display(-1);
    if(!d || !d.pixels) return null;
    let dark = 0, hash = 0;
    for(let i = 0; i < d.pixels.length; i += 4)
    {
        if(d.pixels[i] < 64) dark++;
        hash = (hash * 31 + d.pixels[i] + i) | 0;
    }
    if(OUT && name)
    {
        const rgb = Buffer.alloc(d.width * d.height * 3);
        for(let i = 0; i < d.width * d.height; i++) rgb.set(d.pixels.subarray(4 * i, 4 * i + 3), 3 * i);
        fs.writeFileSync(`${OUT}-${name}.ppm`, Buffer.concat([Buffer.from(`P6 ${d.width} ${d.height} 255\n`), rgb]));
    }
    return { dark, hash, frame: d.frame };
};
const press = async (button, settle = 1500) =>
{
    emulator.set_buttons(button);
    await sleep(150);
    emulator.set_buttons(0);
    await sleep(settle);
};
const BACK = 1, SELECT = 4, DOWN = 8;

while(!log.includes("Ready for communication"))
{
    if(Date.now() - t0 > 90000) fail("no \"Ready for communication\" in the log");
    await sleep(250);
}
console.log(`[pebble] ready after ${elapsed()} (${(emulator.get_instruction_counter() / 1e6).toFixed(0)} M instructions)`);
await sleep(2000);
const launcher = screen("launcher");
if(!launcher || !launcher.frame) fail("the display is off");
console.log(`[pebble] launcher: ${launcher.dark} dark pixels`);

await press(BACK, 3000);
const face = screen("watchface");
console.log(`[pebble] watchface: ${face.dark} dark pixels`);
if(face.hash === launcher.hash || face.dark < 144 * 168 / 2) fail("Back didn't show the (dark) watchface");

await press(SELECT, 2000);
const back_in = screen("launcher-again");
if(back_in.hash === face.hash) fail("Select didn't open the launcher");
await press(DOWN, 1000);
await press(SELECT, 2500);
const app = screen("app");
if(app.hash === back_in.hash) fail("Down, Select didn't open an app");
console.log(`[pebble] PASS: launcher, watchface, an app (${elapsed()})`);
emulator.destroy();
process.exit(0);
