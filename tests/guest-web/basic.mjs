// Tiny BASIC on the v86-riscv site's CH32V003 in headless Chrome
// (?build=ch32v003-basic, tools/ch32v003-basic): a program typed in runs
// and prints, POKEs light the page's LEDs (PC0 to PC3), the Ctrl-C button
// stops a loop, the monitor shows and stores memory, a program SAVEd is
// there to LOAD after a reload (the flash's last 1 KB, kept in the
// browser), and an example program's button types it in and runs it, from
// the monitor too.
//
//   SITE=http://localhost:8789 node tests/guest-web/basic.mjs [screenshot-prefix]
//
// Needs google-chrome and the site from tools/rv64-site (npx wrangler dev
// --config tools/rv64-site/wrangler.jsonc --port 8789) with the build added
// (tools/rv64-site/add-mcu.sh's usage).

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SITE = process.env.SITE || "http://localhost:8789";
const OUT = process.argv[2];
const PORT = +(process.env.PORT || 9353);
const sleep = ms => new Promise(r => setTimeout(r, ms));

const profile = mkdtempSync(path.join(process.env.TMPDIR || tmpdir(), "v86-basic-"));
const chrome = spawn("google-chrome", ["--headless=new", "--no-first-run", "--user-data-dir=" + profile,
    "--remote-debugging-port=" + PORT, "about:blank"], { stdio: "ignore", detached: true });
// (google-chrome is a script: the browser is its child, in its process group)
const cleanup = () => { try { process.kill(-chrome.pid); } catch(e) {} setTimeout(() => { try { rmSync(profile, { recursive: true, force: true }); } catch(e) {} }, 300); };

async function main()
{
    let targets;
    for(let i = 0; i < 50 && !targets; i++)
    {
        await sleep(200);
        try { targets = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); } catch(e) {}
    }
    const ws = new WebSocket(targets.find(t => t.type === "page").webSocketDebuggerUrl);
    await new Promise(r => ws.addEventListener("open", r));
    let id = 0;
    const pending = new Map();
    ws.addEventListener("message", e =>
    {
        const m = JSON.parse(e.data);
        if(m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
        else if(m.method === "Runtime.exceptionThrown") console.error("page exception:", JSON.stringify(m.params.exceptionDetails).slice(0, 400));
    });
    const send = (method, params = {}) => new Promise((resolve, reject) =>
    {
        const timer = setTimeout(() => reject(new Error(method + ": no answer from Chrome")), 30000);
        pending.set(++id, m => { clearTimeout(timer); resolve(m); });
        ws.send(JSON.stringify({ id, method, params }));
    });
    const ev = async expr =>
    {
        const r = (await send("Runtime.evaluate", { expression: expr, returnByValue: true })).result;
        if(r.exceptionDetails) throw new Error(expr.slice(0, 80) + ": " + r.exceptionDetails.exception?.description);
        return r.result.value;
    };
    const shot = async name =>
    {
        if(OUT) writeFileSync(`${OUT}-${name}.png`, Buffer.from((await send("Page.captureScreenshot", { format: "png" })).result.data, "base64"));
    };
    const until = async (what, expr, seconds) =>
    {
        const start = Date.now();
        for(;;)
        {
            const v = await ev(expr).catch(() => null);
            if(v) { console.log(`${what}: ${((Date.now() - start) / 1000).toFixed(1)} s`); return v; }
            if(Date.now() - start > seconds * 1000)
            {
                await shot("timeout");
                throw new Error(`${what}: not within ${seconds} s; the terminal: ${(await ev(terminal).catch(() => "?")).slice(-600)}`);
            }
            await sleep(250);
        }
    };
    const terminal = "document.getElementById('terminal').textContent";
    await send("Runtime.enable");
    await send("Page.enable");
    // Lines typed, each once the last has been echoed
    const type = async lines =>
    {
        for(const line of lines)
        {
            await ev(`emulator.serial0_send(${JSON.stringify(line + "\r")})`);
            await until(`  typed ${line}`, `${terminal}.includes(${JSON.stringify(line)})`, 10);
            await sleep(100);
        }
    };
    const open = async () =>
    {
        await send("Page.navigate", { url: SITE + "/?build=ch32v003-basic" });
        await until("BASIC's prompt", `${terminal}.includes("BYTES FREE") && ${terminal}.trimEnd().endsWith(">")`, 30);
    };
    await open();
    await ev("localStorage.removeItem('v86-riscv-flash:ch32v003-basic')");
    await open();
    if((await ev(terminal)).includes("SAVED")) throw new Error("nothing should be saved yet");

    await type([
        "10 POKE $40021018, PEEK($40021018) OR $10",
        "20 POKE $40011000, PEEK($40011000) AND NOT $FFFF OR $1111",
        "30 FOR I=1 TO 5: PRINT I*I;\" \";: NEXT",
        "40 PRINT: POKE $4001100C, 5",
        "RUN",
    ]);
    await until("its output", `${terminal}.includes("1 4 9 16 25")`, 10);
    const leds = await until("PC0 and PC2 lit", "(emulator.get_gpio_out(2) & 15) === 5 && document.querySelectorAll('#leds .led.on').length === 2", 10);
    console.log("  LEDs:", await ev("[...document.querySelectorAll('#leds .led')].map(l => l.classList.contains('on') ? 1 : 0).join('')"));
    await shot("run");

    // A loop, stopped with the page's Ctrl-C button
    await type(["50 GOTO 50", "RUN"]);
    await sleep(500);
    await ev("[...document.querySelectorAll('#shell-pad button')].find(b => b.textContent === 'Ctrl-C').click()");
    await until("stopped", `${terminal}.includes("?BREAK IN 50")`, 10);
    await type(["50"]);

    // The monitor: a store, and the word read back
    await type(["MON", "20000004: 12345678", "20000004"]);
    await until("the monitor's word", `${terminal}.includes("20000004: 12345678")`, 10);
    await type(["Q"]);

    // SAVE, and LOAD after a reload
    await type(["SAVE"]);
    await until("saved", `${terminal}.includes("BYTES SAVED")`, 10);
    await sleep(1500);   // (the page keeps the flash every second)
    await open();
    if(!(await ev(terminal)).includes("A PROGRAM SAVED")) throw new Error("the saved program should be offered after a reload");
    await type(["LOAD", "LIST"]);
    await until("the program back", `${terminal}.includes("40 PRINT: POKE $4001100C, 5")`, 10);
    await shot("load");

    // An example program, from the monitor: its button leaves it (Ctrl-C),
    // types the program in and runs it
    await type(["MON"]);
    await ev("[...document.querySelectorAll('#program-list button')].find(b => b.textContent === 'Primes').click()");
    await until("the primes", `${terminal}.includes("283 293")`, 30);
    await shot("primes");
    console.log("PASS");
}

main().then(() => { cleanup(); process.exit(0); }, e => { console.error("FAIL:", e.message); cleanup(); process.exit(1); });
