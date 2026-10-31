// Runs Vlad Tomoiagă's osillyscope (tools/ch32v003-osillyscope) on the
// v86-riscv site in headless Chrome: its trace on the OLED must follow the
// input (a sine wave spans rows; nothing is a flat line), the machine must
// keep to real time, its buttons must work its menu (read from its RAM), the
// "time base for sound" helper must set the slowest time base with them,
// and Chrome's fake microphone, streamed into the ADC, must show on the
// trace.
//
//   SITE=http://localhost:8789 node tests/rv64/osillyscope.mjs [screenshot-prefix]
//
// Needs google-chrome and the site (npx wrangler dev --config
// tools/rv64-site/wrangler.jsonc --port 8789) with the build
// (tools/ch32v003-osillyscope/build.sh, then add-mcu.sh as it says).
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SITE = process.env.SITE || "http://localhost:8789";
const OUT = process.argv[2];
const PORT = +(process.env.PORT || 9361);
const sleep = ms => new Promise(r => setTimeout(r, ms));

const profile = mkdtempSync(path.join(tmpdir(), "v86-osillyscope-"));
const chrome = spawn("google-chrome", ["--headless=new", "--no-first-run", "--user-data-dir=" + profile,
    "--autoplay-policy=no-user-gesture-required", "--use-fake-device-for-media-stream", "--use-fake-ui-for-media-stream",
    "--remote-debugging-port=" + PORT, "about:blank"],
    { stdio: "ignore", detached: true });
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
        const r = (await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true })).result;
        if(r.exceptionDetails) throw new Error(expr.slice(0, 80) + ": " + r.exceptionDetails.exception?.description);
        return r.result.value;
    };
    const key = async (k, code, vk, hold_ms) =>
    {
        await send("Input.dispatchKeyEvent", { type: "keyDown", key: k, code, windowsVirtualKeyCode: vk });
        await sleep(hold_ms);
        await send("Input.dispatchKeyEvent", { type: "keyUp", key: k, code, windowsVirtualKeyCode: vk });
    };
    const shot = async name =>
    {
        if(OUT) writeFileSync(`${OUT}-${name}.png`, Buffer.from((await send("Page.captureScreenshot", { format: "png" })).result.data, "base64"));
    };
    // The trace: in the plot (64 x 64 at the left, its graticule's dotted
    // lines every 16 pixels left out), the lowest and highest rows lit
    const trace = () => ev(`(() => {
        const d = emulator.get_oled_display(-1);
        if(!d || !d.pixels) return null;
        let top = 64, bottom = -1, lit = 0;
        for(let y = 0; y < 64; y++)
        {
            if(y % 16 === 0 || y === 63) continue;
            for(let x = 1; x < 64; x++)
            {
                if(x % 16 === 0 || !d.pixels[y * 128 + x]) continue;
                lit++;
                top = Math.min(top, y);
                bottom = Math.max(bottom, y);
            }
        }
        return { top, bottom, span: bottom - top, lit };
    })()`);
    // The widest trace seen over a while
    const widest = async ms =>
    {
        let best = null;
        for(let t = 0; t < ms; t += 100)
        {
            const s = await trace();
            if(s && (!best || s.span > best.span)) best = s;
            await sleep(100);
        }
        return best;
    };
    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 2, mobile: true });
    await send("Page.navigate", { url: `${SITE}/?build=osillyscope` });
    for(let i = 0; i < 100 && !(await ev("!!(window.emulator && emulator.get_oled_display(0))").catch(() => false)); i++) await sleep(100);
    const symbols = await ev("fetch('images/manifest.json').then(r => r.json()).then(m => m.builds.find(b => b.id === 'osillyscope').stats.symbols)");
    const ram = addr => ev(`emulator.read_memory(${addr}, 1)[0]`);

    // Past the splash screen (2.5 s): the default input, a 50 kHz sine of
    // 1 V, two cycles across the screen at 10 us a division
    await sleep(4000);
    const [t0, v0] = await ev("[performance.now(), emulator.take_gpio_edges().now]");
    const sine = await widest(1500);
    console.log(`sine: rows ${sine?.top} to ${sine?.bottom}`);
    await shot("sine");
    if(!sine || sine.span < 10) throw new Error("the sine wave isn't on the trace");
    const [t1, v1] = await ev("[performance.now(), emulator.take_gpio_edges().now]");
    const speed = (v1 - v0) / (t1 - t0);
    console.log(`machine time at ${speed.toFixed(2)}x real time`);
    if(speed < 0.8 || speed > 1.1) throw new Error("not at real time");

    // Nothing in: a flat line at 0 V
    await ev(`(() => { const s = document.getElementById("scope-input"); s.value = "0"; s.dispatchEvent(new Event("change")); })()`);
    await sleep(1000);
    const flat = await widest(1000);
    console.log(`nothing: rows ${flat?.top} to ${flat?.bottom}`);
    if(!flat || flat.span > 1) throw new Error("no flat line for no input");

    // Select (space) moves the menu's selection
    const sel0 = await ram(symbols.ui_selector);
    await key(" ", "Space", 32, 150);
    await sleep(300);
    const sel1 = await ram(symbols.ui_selector);
    console.log(`select: menu selection ${sel0} -> ${sel1}`);
    if(sel1 !== (sel0 + 1) % 7) throw new Error("the select button didn't move the selection");

    // The time base for sound, set with the scope's buttons
    await ev(`document.getElementById("scope-audio").click()`);
    for(let i = 0; i < 100 && await ev(`document.getElementById("scope-audio").disabled`); i++) await sleep(200);
    const [tdiv, sel2] = [await ram(symbols.tdiv_sel), await ram(symbols.ui_selector)];
    console.log(`time base for sound: tdiv_sel ${tdiv}, selection ${sel2}`);
    if(tdiv !== 11 || sel2 !== 0) throw new Error("the time base wasn't set");

    // The microphone (Chrome's fake one: beeps), streamed into the input
    await ev(`(() => { const s = document.getElementById("scope-input"); s.value = "mic"; s.dispatchEvent(new Event("change")); })()`);
    for(let i = 0; i < 50 && !/Microphone on/.test(await ev(`document.getElementById("scope-status").textContent`)); i++) await sleep(100);
    console.log("status: " + await ev(`document.getElementById("scope-status").textContent`));
    const sound = await widest(4000);
    console.log(`microphone: rows ${sound?.top} to ${sound?.bottom}`);
    await shot("mic");
    if(!sound || sound.span < 4) throw new Error("the microphone's sound isn't on the trace");
    if(exceptions) throw new Error(exceptions + " page exceptions");
    console.log("PASS");
}

main().then(() => { cleanup(); setTimeout(() => process.exit(0), 500); },
    e => { console.error("FAIL:", e.message); cleanup(); setTimeout(() => process.exit(1), 500); });
