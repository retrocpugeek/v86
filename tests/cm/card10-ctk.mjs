// Contiki's desktop on the v86-arm site's card10 (?build=card10-ctk, which
// starts it: tools/a64-site/card10/ctk) in headless Chrome, through the
// touch panel the emulator adds (the mouse on the badge's screen): the
// desktop must come up with the RetroCPUGeek window; tapping its Close button
// closes it, tapping the Calc icon opens the calculator, and tapping 7 and 8
// shows 78; tapping the Play icon opens the player, whose frames core 0
// decodes (they change; the cores' view shows epic_anim_decode), and its
// Close button closes it; tapping the Term icon opens the serial terminal, where what's
// typed in the page's console shows (core 0's UART interrupt to core 1) and
// comes back in the console (the echo), and its Hello button writes to the
// console; holding the top right button (Enter) opens the menus. No page
// exceptions.
//
//   SITE=http://localhost:8788 node tests/cm/card10-ctk.mjs [screenshot-prefix]
//
// Needs google-chrome and the site (npx wrangler dev --config
// tools/a64-site/wrangler.jsonc --port 8788) with card10-ctk (add-card10.sh).
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SITE = process.env.SITE || "http://localhost:8788";
const OUT = process.argv[2];
const PORT = +(process.env.PORT || 9358);
const sleep = ms => new Promise(r => setTimeout(r, ms));

const profile = mkdtempSync(path.join(tmpdir(), "v86-card10ctk-"));
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
    // The display's pixels in a rectangle (display pixels): how many are
    // cream (a window), purple (the menus), dark (text in a window)
    const count = (x0, y0, x1, y1) => ev(`(() => {
        const c = document.getElementById("card10-screen");
        if(!c) return { cream: 0, purple: 0, dark: 0, total: 0 };
        const p = c.getContext("2d").getImageData(${x0}, ${y0}, ${x1 - x0}, ${y1 - y0}).data;
        let cream = 0, purple = 0, dark = 0;
        for(let i = 0; i < p.length; i += 4) {
            const [r, g, b] = [p[i], p[i + 1], p[i + 2]];
            if(r > 220 && g > 210 && b > 200) cream++;
            if(r > 70 && r < 120 && g < 70 && b > 80 && b < 130) purple++;
            if(r < 60 && g < 60 && b < 60) dark++;
        }
        return { cream, purple, dark, total: p.length / 4 };
    })()`);
    const until = async (what, test, seconds = 30) =>
    {
        const start = Date.now();
        for(;;)
        {
            const v = await test();
            if(v) { console.log(`${what}: ${((Date.now() - start) / 1000).toFixed(1)} s`); return v; }
            if(Date.now() - start > seconds * 1000) { await shot("timeout"); throw new Error("timed out: " + what); }
            await sleep(300);
        }
    };
    // A tap on the badge's screen at (x, y) in display pixels
    const tap = async (x, y) =>
    {
        const [cx, cy] = await ev(`(() => { const s = document.getElementById("card10-screen");
            s.scrollIntoView({ block: "center" });
            const r = s.getBoundingClientRect();
            return [r.x + (${x} + 0.5) / 160 * r.width, r.y + (${y} + 0.5) / 80 * r.height]; })()`);
        await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: cx, y: cy });
        await send("Input.dispatchMouseEvent", { type: "mousePressed", x: cx, y: cy, button: "left", buttons: 1, clickCount: 1 });
        await sleep(250);
        await send("Input.dispatchMouseEvent", { type: "mouseReleased", x: cx, y: cy, button: "left", buttons: 0, clickCount: 1 });
        await sleep(400);
    };

    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false });
    const t0 = Date.now();
    await send("Page.navigate", { url: `${SITE}/?build=card10-ctk` });
    // The desktop: the menu bar (purple, the top row), and the window
    await until("the desktop, with the RetroCPUGeek window", async () =>
    {
        const bar = await count(0, 0, 160, 6), mid = await count(40, 20, 120, 50);
        return bar.purple > 500 && mid.cream > 1500;
    }, 120);
    console.log(`  after ${((Date.now() - t0) / 1000).toFixed(1)} s`);
    await shot("desktop");
    if(await ev("document.getElementById('card10-touch').hidden")) throw new Error("the page doesn't offer the touch screen");
    // Its Close button
    await tap(82, 62);
    await until("closed", async () => (await count(40, 20, 120, 50)).cream < 200);
    // The Calc icon: the calculator, its display empty
    await tap(80, 55);
    await until("the calculator", async () => (await count(56, 14, 104, 64)).cream > 1200);
    const empty = await count(90, 18, 104, 24);
    await tap(60, 40);
    await tap(70, 40);
    await until("78 on its display", async () => (await count(90, 18, 104, 24)).dark > empty.dark + 8);
    await shot("calc");
    // The player (the Play icon): its picture changes, frame by frame, as
    // core 0 decodes them (the cores' view counts the decoder's calls and
    // interrupts); its Close button
    const picture = () => ev(`(() => {
        const p = document.getElementById("card10-screen").getContext("2d").getImageData(36, 14, 88, 38).data;
        let h = 0;
        for(let i = 0; i < p.length; i++) h = (h * 31 + p[i]) >>> 0;
        return h;
    })()`);
    await tap(145, 28);
    await until("the player", async () => (await count(110, 56, 126, 66)).cream > 60);
    const seen = new Set();
    await until("frames changing", async () => { seen.add(await picture()); return seen.size >= 5; }, 20);
    await until("the decoder's calls in the cores' view", async () =>
        (await ev("document.querySelector('#cores-top tbody')?.innerText || ''")).includes("epic_anim_decode"), 20);
    await shot("player");
    await tap(108, 62);
    await until("the player closed", async () => (await count(110, 56, 126, 66)).cream < 10);
    // The serial terminal: the Term icon, then text typed in the console
    const log = () => ev("document.querySelector('#terminal .xterm-rows')?.innerText || ''");
    await tap(145, 55);
    await until("the terminal", async () => (await count(8, 18, 150, 53)).cream > 3000);
    const blank = await count(8, 18, 150, 53);
    await ev("document.querySelector('#terminal .xterm-helper-textarea').focus()");
    await send("Input.insertText", { text: "hi from the page" });
    await until("the typing in the terminal", async () => (await count(8, 18, 150, 53)).dark > blank.dark + 40);
    await until("the echo in the console", async () => (await log()).includes("hi from the page"));
    await ev("document.activeElement.blur()");
    await tap(20, 62);
    await until("Hello in the console", async () => (await log()).includes("Hello from core 1"));
    await shot("serial");
    // The menus: Enter held (the top right button)
    await send("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
    await sleep(900);
    await send("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
    await until("the menu open", async () => (await count(4, 6, 60, 30)).purple > 400);
    await shot("menu");
    if(exceptions) throw new Error(exceptions + " page exceptions");
    console.log("PASS");
}

main().then(() => { cleanup(); setTimeout(() => process.exit(0), 500); },
    e => { console.error("FAIL:", e.message); cleanup(); setTimeout(() => process.exit(1), 500); });
