// Plays the CH32V003 game console's games (tools/ch32v003-gameconsole) on the
// v86-riscv site in headless Chrome: each must draw a title screen on the
// OLED, start on the fire button and then change the display as it runs;
// the buzzer's edges are counted, and the machine must keep to real time.
//
//   SITE=http://localhost:8788 node tests/rv64/gameconsole.mjs [games] [screenshot-prefix]
//
// games: comma-separated build ids without "gameconsole-" (default: all twelve).
// Needs google-chrome and the site (npx wrangler dev --config
// tools/rv64-site/wrangler.jsonc --port 8788).
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SITE = process.env.SITE || "http://localhost:8788";
const GAMES = (process.argv[2] || "tris,invaders,arkanoid,pacman,lander,conway,doom,boulder,bubble,karate,run,shooter").split(",");
const OUT = process.argv[3];
const PORT = +(process.env.PORT || 9349);
const sleep = ms => new Promise(r => setTimeout(r, ms));

const profile = mkdtempSync(path.join(tmpdir(), "v86-gameconsole-"));
const chrome = spawn("google-chrome", ["--headless=new", "--no-first-run", "--user-data-dir=" + profile,
    "--autoplay-policy=no-user-gesture-required", "--remote-debugging-port=" + PORT, "about:blank"], { stdio: "ignore" });
const cleanup = () => { chrome.kill(); try { rmSync(profile, { recursive: true, force: true }); } catch(e) {} };

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
    const key = async (k, code, hold_ms) =>
    {
        await send("Input.dispatchKeyEvent", { type: "keyDown", key: k, code, windowsVirtualKeyCode: code === "Space" ? 32 : 37 });
        await sleep(hold_ms);
        await send("Input.dispatchKeyEvent", { type: "keyUp", key: k, code, windowsVirtualKeyCode: code === "Space" ? 32 : 37 });
    };
    // A fingerprint of the display, and how many pixels are lit
    const screen = () => ev(`(() => {
        const d = emulator.get_oled_display(-1);
        if(!d || !d.pixels) return null;
        let lit = 0, hash = 0;
        for(let i = 0; i < d.pixels.length; i++) { lit += d.pixels[i]; hash = (hash * 31 + d.pixels[i] * (i + 1)) | 0; }
        return { lit, hash };
    })()`);
    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 2, mobile: true });

    let failed = 0;
    for(const game of GAMES)
    {
        await send("Page.navigate", { url: `${SITE}/?build=gameconsole-${game}` });
        for(let i = 0; i < 100 && !(await ev("!!(window.emulator && emulator.get_oled_display(0))").catch(() => false)); i++) await sleep(100);
        // Count the buzzer's edges as the page takes them
        await ev(`(() => { window.__edges = 0; const take = emulator.take_gpio_edges.bind(emulator);
            emulator.take_gpio_edges = () => { const r = take(); window.__edges += r.edges.length; window.__vnow = r.now; return r; }; })()`);
        await sleep(1500);
        const title = await screen();
        const [t0, v0] = await ev("[performance.now(), window.__vnow]");
        await key(" ", "Space", 300);
        await sleep(1500);
        await key("ArrowLeft", "ArrowLeft", 400);
        await sleep(1500);
        const a = await screen();
        await sleep(700);
        const b = await screen();
        const [t1, v1] = await ev("[performance.now(), window.__vnow]");
        const speed = (v1 - v0) / (t1 - t0);
        const edges = await ev("window.__edges");
        if(OUT) writeFileSync(`${OUT}-${game}.png`, Buffer.from((await send("Page.captureScreenshot", { format: "png" })).result.data, "base64"));
        const ok = title && title.lit > 100 && a && a.hash !== title.hash && b && speed > 0.8 && speed < 1.1;
        if(!ok) failed++;
        console.log(`${ok ? "PASS" : "FAIL"} ${game}: title ${title?.lit} px lit, then ${a?.lit} and ${b?.lit} (${a?.hash !== b?.hash ? "moving" : "still"}), ` +
            `${edges} buzzer edges, machine time at ${speed.toFixed(2)}x real time`);
    }
    if(failed) throw new Error(`${failed} of ${GAMES.length} games failed`);
}

main().then(() => { cleanup(); process.exit(0); }, e => { console.error("FAIL:", e.message); cleanup(); process.exit(1); });
