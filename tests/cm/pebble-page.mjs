// The v86-arm site's Pebble watch (?build=pebble-flint) in headless Chrome:
// PebbleOS must start (the page's status says Running once its log says
// it's ready), the screen must show the launcher, the keyboard's Left arrow
// (Back) the watchface and the on-screen Select button the launcher again;
// the log view must show decoded lines; no page exceptions.
//
//   SITE=http://localhost:8788 node tests/cm/pebble-page.mjs [screenshot-prefix]
//
// Needs google-chrome and the site (npx wrangler dev --config
// tools/a64-site/wrangler.jsonc --port 8788) with the watch (add-watch.sh).
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SITE = process.env.SITE || "http://localhost:8788";
const BUILD = process.env.BUILD || "pebble-flint";
const OUT = process.argv[2];
const PORT = +(process.env.PORT || 9353);
const sleep = ms => new Promise(r => setTimeout(r, ms));

const profile = mkdtempSync(path.join(tmpdir(), "v86-pebble-"));
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
    // The screen canvas: a fingerprint and how many pixels are dark
    const screen = () => ev(`(() => {
        const c = document.getElementById("watch-screen");
        const p = c.getContext("2d").getImageData(0, 0, c.width, c.height).data;
        let dark = 0, hash = 0;
        for(let i = 0; i < p.length; i += 4) { if(p[i] < 64) dark++; hash = (hash * 31 + p[i] + i) | 0; }
        return { dark, hash, total: p.length / 4 };
    })()`);
    const key = async (k, vk) =>
    {
        await send("Input.dispatchKeyEvent", { type: "keyDown", key: k, code: k, windowsVirtualKeyCode: vk });
        await sleep(150);
        await send("Input.dispatchKeyEvent", { type: "keyUp", key: k, code: k, windowsVirtualKeyCode: vk });
    };

    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 2, mobile: true });
    const t0 = Date.now();
    await send("Page.navigate", { url: `${SITE}/?build=${BUILD}` });
    for(;;)
    {
        const status = await ev("document.getElementById('status').textContent").catch(() => "");
        if(status === "Running") break;
        if(Date.now() - t0 > 90000) throw new Error("not running: " + status);
        await sleep(500);
    }
    console.log(`running after ${((Date.now() - t0) / 1000).toFixed(1)} s`);
    await sleep(2500);
    const launcher = await screen();
    await shot("launcher");
    await key("ArrowLeft", 37);
    await sleep(2500);
    const face = await screen();
    await shot("watchface");
    console.log(`launcher: ${launcher.dark} dark of ${launcher.total}; watchface: ${face.dark}`);
    if(face.hash === launcher.hash || face.dark < face.total / 2) throw new Error("Back didn't show the watchface");
    // The on-screen Select button
    const r = await ev(`(() => { const b = document.querySelector('.pebble button[data-button="4"]').getBoundingClientRect(); return [b.x + b.width / 2, b.y + b.height / 2]; })()`);
    await send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x: r[0], y: r[1] }] });
    await sleep(150);
    await send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
    await sleep(2500);
    const again = await screen();
    if(again.hash === face.hash) throw new Error("the Select button didn't open the launcher");
    const lines = await ev("document.getElementById('watch-log').textContent.split('\\n').length");
    const sample = await ev("document.getElementById('watch-log').textContent.split('\\n').find(l => l.includes('PebbleOS')) || ''");
    console.log(`log: ${lines} lines, e.g. "${sample}"`);
    if(lines < 10) throw new Error("no log");
    await send("Emulation.setDeviceMetricsOverride", { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false });
    await sleep(500);
    await shot("desktop");
    if(exceptions) throw new Error(exceptions + " page exceptions");
    console.log("PASS");
}

main().then(() => { cleanup(); setTimeout(() => process.exit(0), 500); },
    e => { console.error("FAIL:", e.message); cleanup(); setTimeout(() => process.exit(1), 500); });
