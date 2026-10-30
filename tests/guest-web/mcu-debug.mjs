// Debugs a microcontroller's firmware on the v86-riscv site in headless
// Chrome (?debug): the page boots the gdb-wui image next to the CH32V003,
// whose gdb connects to the microcontroller's GDB stub through the page
// (tools/rv64-site/index.html, setup_debugging). gdb-wui must come up
// stopped at the reset vector with the firmware loaded; then a breakpoint on
// a function, Continue, and it stops there in the firmware's source; Next
// steps a line; Continue lets the firmware run (it draws), and Pause stops it.
//
//   SITE=http://localhost:8789 node tests/guest-web/mcu-debug.mjs [screenshot-prefix]
//   SITE=... BUILD=gameconsole-tris FUNC=ADC_read SOURCE=gpio.h node tests/guest-web/mcu-debug.mjs
//
// Needs google-chrome and the site from tools/rv64-site (npx wrangler dev
// --config tools/rv64-site/wrangler.jsonc --port 8789).
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SITE = process.env.SITE || "http://localhost:8789";
const BUILD = process.env.BUILD || "ch32v003-tetris";
const FUNC = process.env.FUNC || "draw";
const SOURCE = process.env.SOURCE || "tetris.c";
const OUT = process.argv[2];
const PORT = +(process.env.PORT || 9349);
const t0 = Date.now();
const sleep = ms => new Promise(r => setTimeout(r, ms));

const profile = mkdtempSync(path.join(process.env.TMPDIR || tmpdir(), "v86-mcudebug-"));
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
    const doc = "document.getElementById('guest-web').contentDocument";
    const frame = expr => ev(`(() => { const d = ${doc}; return d && (${expr}); })()`);
    const page_status = () => ev("document.getElementById('status').textContent");
    const shot = async name =>
    {
        if(OUT) writeFileSync(`${OUT}-${name}.png`, Buffer.from((await send("Page.captureScreenshot", { format: "png" })).result.data, "base64"));
    };
    const until = async (what, expr, seconds) =>
    {
        const start = Date.now();
        let next_progress = Date.now() + 30000;
        for(;;)
        {
            if(Date.now() > next_progress)
            {
                next_progress += 30000;
                console.log(`  … ${((Date.now() - t0) / 1000).toFixed(0)} s: page "${await page_status()}", panel "${await ev("document.getElementById('web-status').textContent")}"`);
            }
            const v = await frame(expr).catch(() => null);
            if(v) { console.log(`${what}: ${((Date.now() - start) / 1000).toFixed(1)} s`); return v; }
            if(Date.now() - start > seconds * 1000)
            {
                await shot("timeout");
                const wui = await frame("[d.getElementById('log').innerText, d.getElementById('exe-name').textContent, d.getElementById('run-state').dataset.state].join(' | ')").catch(e => e.message);
                console.log("  gdb-wui log:", String(wui).replace(/\n+/g, "\n    ").slice(-3000));
                throw new Error(`timed out: ${what} (page: ${await page_status()})`);
            }
            await sleep(500);
        }
    };
    const stopped = "d.getElementById('run-state').dataset.state === 'stopped'";
    const stack = () => frame("d.getElementById('stack').innerText.replace(/\\s+/g, ' ').slice(0, 160)");

    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 1400, height: 1800, deviceScaleFactor: 1, mobile: false });
    await send("Page.navigate", { url: SITE + "/?build=" + BUILD + "&debug" });
    await sleep(3000);
    const held = await page_status();
    console.log("page:", held);
    if(!/Held at reset/.test(held)) throw new Error("the microcontroller isn't held for the debugger");

    await until("gdb-wui connected", "d.getElementById('conn')?.dataset.state === 'open'", +(process.env.CONNECT_TIMEOUT || 600));
    // (the reset vector: Tetris's _start, the game console's jump_reset)
    await until("attached, stopped at the reset vector", `${stopped} && /_start|jump_reset/.test(d.getElementById('stack').innerText)`, 180);
    console.log("  page:", await page_status(), "| program:", await frame("d.getElementById('exe-name').textContent"), "| stack:", await stack());
    if(!/attached/.test(await page_status())) throw new Error("the page doesn't say gdb-wui is attached");
    await shot("reset");

    // A line typed into gdb's console, once it shows a prompt
    const typed = "d.getElementById('gdbconsole').innerText.trimEnd()";
    const type = async (line, answer) =>
    {
        await until("gdb's prompt", `${typed}.endsWith("(gdb)")`, 60);
        await frame("d.querySelector('#gdbconsole .xterm-helper-textarea').focus()");
        await send("Input.insertText", { text: line });
        await send("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, text: "\r" });
        await send("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
        await until(`gdb answered ${line}`, `${answer}.test(${typed}) && ${typed}.endsWith("(gdb)")`, 60);
    };
    // RV32E: a5 but no a6 (the stub's target description says so)
    await type("info registers a5", "/a5\\s+0x/");
    await type("info registers a6", "/Invalid register .a6./");

    // A breakpoint, and Continue
    await type("break " + FUNC, "/Breakpoint 1 at/");
    await frame("d.getElementById('btn-continue').click()");
    await until(`stopped in ${FUNC}`, `${stopped} && d.getElementById('source-path').textContent.includes(${JSON.stringify(SOURCE)}) && d.getElementById('stack').innerText.includes(${JSON.stringify(FUNC)})`, 120);
    console.log("  source:", await frame("d.getElementById('source-path').textContent"), "| stack:", await stack());
    const line = "(d.getElementById('stack').innerText.match(/:(\\d+)/) || [])[1]";
    const before_next = await frame(line);
    await frame("d.getElementById('btn-next').click()");
    await until("stepped a line", `${stopped} && ${line} !== ${JSON.stringify(before_next)}`, 60);
    console.log("  after next:", await stack());
    await shot("stopped");

    // Free running, with the breakpoint gone: the firmware draws; then Pause
    await type("delete", "/\\(gdb\\) delete/");
    const before = await ev("emulator.get_instruction_counter()");
    await frame("d.getElementById('btn-continue').click()");
    await sleep(3000);
    if(await frame(stopped)) throw new Error("stopped again: " + await stack());
    const ran = await ev("emulator.get_instruction_counter()") - before;
    console.log(`  ran ${(ran / 1e6).toFixed(1)} M instructions in 3 s`);
    if(ran < 1e5) throw new Error("the firmware didn't run");
    await frame("d.getElementById('btn-pause').click()");
    await until("paused", stopped, 60);
    console.log("  paused at:", await stack());
    await shot("paused");
}

main().then(() => { cleanup(); setTimeout(() => process.exit(0), 500); },
    e => { console.error("FAIL:", e.message); cleanup(); setTimeout(() => process.exit(1), 500); });
