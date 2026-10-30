// The v86-arm site's card10 badge (?build=card10) in headless Chrome: its
// firmware must start (the page's status says Running once Pycardium loads
// main.py), the display must show the clock, the cores' view its API calls
// (named), the on-screen MENU button the
// menu, the keyboard's Esc and Enter must work, and MicroPython's REPL must
// answer through the console (UART0, both ways); no page exceptions.
//
//   SITE=http://localhost:8788 node tests/cm/card10-page.mjs [screenshot-prefix]
//
// Needs google-chrome and the site (npx wrangler dev --config
// tools/a64-site/wrangler.jsonc --port 8788) with the card10 (add-card10.sh).
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SITE = process.env.SITE || "http://localhost:8788";
const OUT = process.argv[2];
const PORT = +(process.env.PORT || 9357);
const sleep = ms => new Promise(r => setTimeout(r, ms));

const profile = mkdtempSync(path.join(tmpdir(), "v86-card10-"));
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
    let id = 0, exceptions = 0;
    const pending = new Map();
    ws.addEventListener("message", e =>
    {
        const m = JSON.parse(e.data);
        if(m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
        else if(m.method === "Runtime.exceptionThrown")
        {
            exceptions++;
            console.error("page exception:", JSON.stringify(m.params.exceptionDetails).slice(0, 400));
        }
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
    // The screen: a fingerprint, how many pixels are lit, and how many blue
    // (the menu's background)
    const screen = () => ev(`(() => {
        const c = document.getElementById("card10-screen");
        const p = c.getContext("2d").getImageData(0, 0, c.width, c.height).data;
        let lit = 0, blue = 0, hash = 0;
        for(let i = 0; i < p.length; i += 4) {
            if(p[i] + p[i + 1] + p[i + 2] > 96) lit++;
            if(p[i + 2] > 100 && p[i] < 60) blue++;
            hash = (hash * 31 + p[i] + p[i + 1] * 7 + p[i + 2] * 13) | 0;
        }
        return { lit, blue, hash, total: p.length / 4 };
    })()`);
    const key = async (k, code, vk) =>
    {
        await send("Input.dispatchKeyEvent", { type: "keyDown", key: k, code, windowsVirtualKeyCode: vk });
        await sleep(150);
        await send("Input.dispatchKeyEvent", { type: "keyUp", key: k, code, windowsVirtualKeyCode: vk });
    };
    const log = () => ev("document.querySelector('#terminal .xterm-rows')?.innerText || ''");

    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false });
    const t0 = Date.now();
    await send("Page.navigate", { url: `${SITE}/?build=card10` });
    for(;;)
    {
        const status = await ev("document.getElementById('status').textContent").catch(() => "");
        if(status === "Running") break;
        if(Date.now() - t0 > 90000) throw new Error("not running: " + status);
        await sleep(500);
    }
    console.log(`running after ${((Date.now() - t0) / 1000).toFixed(1)} s`);
    // the clock's face: some lit pixels, and it moves
    await sleep(4000);
    const clock = await screen();
    await shot("clock");
    await sleep(2500);
    const clock2 = await screen();
    console.log(`clock: ${clock.lit} lit of ${clock.total}, ${clock.hash !== clock2.hash ? "moving" : "still"}`);
    if(clock.lit < 100 || clock.lit > clock.total / 2) throw new Error("no clock face");
    // The cores' view: API calls seen, by name
    const calls = +(await ev("document.getElementById('cores-calls').textContent")) || 0;
    const busiest = await ev("document.querySelector('#cores-top tbody td')?.innerText || ''");
    console.log(`cores: ${calls} API calls/s, the busiest ${busiest}`);
    if(calls < 10 || !busiest.startsWith("epic_")) throw new Error("the cores' view shows no named API calls");
    // The on-screen MENU button: the menu (a blue list)
    const r = await ev(`(() => { const b = document.querySelector('.card10 button[data-button="8"]').getBoundingClientRect(); return [b.x + b.width / 2, b.y + b.height / 2]; })()`);
    await send("Input.dispatchMouseEvent", { type: "mousePressed", x: r[0], y: r[1], button: "left", clickCount: 1 });
    await sleep(200);
    await send("Input.dispatchMouseEvent", { type: "mouseReleased", x: r[0], y: r[1], button: "left", clickCount: 1 });
    let menu;
    for(let i = 0; i < 30; i++)
    {
        await sleep(500);
        menu = await screen();
        if(menu.blue > menu.total / 3) break;
    }
    await shot("menu");
    console.log(`menu: ${menu.blue} blue pixels of ${menu.total}`);
    if(menu.blue < menu.total / 3) throw new Error("MENU didn't show the menu");
    // the keyboard: the right arrow moves the menu's selection
    await key("ArrowRight", "ArrowRight", 39);
    await sleep(1500);
    const moved = await screen();
    if(moved.hash === menu.hash) throw new Error("the right arrow didn't move the selection");
    console.log("keyboard: the right arrow moved the selection");
    // The REPL: Ctrl-C twice into the console; then a sum
    await ev("document.querySelector('#terminal .xterm-helper-textarea').focus()");
    const ctrl_c = async () =>
    {
        await send("Input.dispatchKeyEvent", { type: "keyDown", key: "c", code: "KeyC", windowsVirtualKeyCode: 67, modifiers: 2 });
        await send("Input.dispatchKeyEvent", { type: "keyUp", key: "c", code: "KeyC", windowsVirtualKeyCode: 67, modifiers: 2 });
    };
    // (the first stops the app, the second the menu that follows)
    await ctrl_c();
    await sleep(2000);
    await ctrl_c();
    let text = "";
    for(let i = 0; i < 40 && !text.includes(">>>"); i++) { await sleep(500); text = await log(); }
    if(!text.includes(">>>")) throw new Error("no REPL prompt after Ctrl-C; the console ends: " + JSON.stringify(text.slice(-400)));
    await send("Input.insertText", { text: "print(6*7)" });
    await key("Enter", "Enter", 13);
    for(let i = 0; i < 20 && !/\n42\b/.test(text); i++) { await sleep(500); text = await log(); }
    await shot("repl");
    if(!/\n42\b/.test(text)) throw new Error("the REPL didn't answer print(6*7)");
    console.log("REPL: print(6*7) printed 42");
    if(exceptions) throw new Error(exceptions + " page exceptions");
    console.log("PASS");
}

main().then(() => { cleanup(); setTimeout(() => process.exit(0), 500); },
    e => { console.error("FAIL:", e.message); cleanup(); setTimeout(() => process.exit(1), 500); });
