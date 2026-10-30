// Plays Maksym Curul's CH32V003 Mini Game Console (tools/ch32v003-minigameconsole)
// on the v86-riscv site in headless Chrome: its menu must come up on the LCD
// (green text on black), A must start Snake (a white score line) and the
// game must move on; the buzzer's tones are counted, and the machine must
// keep to real time. Then the flash's high score page: the first start
// initialises it (a 4-note tune), the page keeps it in local storage, and a
// reload starts with it (the 8-note tune).
//
//   SITE=http://localhost:8789 node tests/rv64/minigameconsole.mjs [screenshot-prefix]
//
// Needs google-chrome and the site (npx wrangler dev --config
// tools/rv64-site/wrangler.jsonc --port 8789).
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SITE = process.env.SITE || "http://localhost:8789";
const OUT = process.argv[2];
const PORT = +(process.env.PORT || 9351);
const sleep = ms => new Promise(r => setTimeout(r, ms));

const profile = mkdtempSync(path.join(tmpdir(), "v86-minigameconsole-"));
const chrome = spawn("google-chrome", ["--headless=new", "--no-first-run", "--user-data-dir=" + profile,
    "--autoplay-policy=no-user-gesture-required", "--remote-debugging-port=" + PORT, "about:blank"],
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
        const r = (await send("Runtime.evaluate", { expression: expr, returnByValue: true })).result;
        if(r.exceptionDetails) throw new Error(expr.slice(0, 80) + ": " + r.exceptionDetails.exception?.description);
        return r.result.value;
    };
    const key = async (k, code, vk, hold_ms) =>
    {
        await send("Input.dispatchKeyEvent", { type: "keyDown", key: k, code, windowsVirtualKeyCode: vk });
        await sleep(hold_ms);
        await send("Input.dispatchKeyEvent", { type: "keyUp", key: k, code, windowsVirtualKeyCode: vk });
    };
    // The LCD: a fingerprint, and how many pixels are mostly green, white
    // or red
    const screen = () => ev(`(() => {
        const d = emulator.get_lcd_display(-1);
        if(!d || !d.pixels) return null;
        const p = d.pixels;
        let green = 0, white = 0, red = 0, hash = 0;
        for(let i = 0; i < p.length; i += 4)
        {
            const [r, g, b] = [p[i], p[i + 1], p[i + 2]];
            if(g > 150 && r < 100 && b < 100) green++;
            if(r > 200 && g > 200 && b > 200) white++;
            if(r > 150 && g < 80 && b < 80) red++;
            hash = (hash * 31 + r * 7 + g * 3 + b + i) | 0;
        }
        return { green, white, red, hash };
    })()`);
    // Count TIM2's tone changes as the page takes them, and the machine's time
    const count_tones = () => ev(`(() => { window.__tones = []; const take = emulator.take_tones.bind(emulator);
        emulator.take_tones = () => { const r = take(); window.__tones.push(...r.tones); window.__vnow = r.now; return r; }; })()`);
    const open = async () =>
    {
        await send("Page.navigate", { url: `${SITE}/?build=minigameconsole` });
        for(let i = 0; i < 100 && !(await ev("!!(window.emulator && emulator.get_lcd_display(0))").catch(() => false)); i++) await sleep(100);
        await count_tones();
    };
    const tune = () => ev("window.__tones.filter(t => t.hz).length");
    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 2, mobile: true });

    // A first visit: no high scores kept
    await send("Page.navigate", { url: `${SITE}/?build=nothing` });
    await sleep(500);
    await ev(`localStorage.removeItem("v86-riscv-flash:minigameconsole")`);
    await open();
    await sleep(2500);
    const menu = await screen();
    const first_tune = await tune();
    const [t0, v0] = await ev("[performance.now(), window.__vnow]");
    console.log(`menu: ${menu?.green} green px; start-up tune ${first_tune} notes`);
    if(!menu || menu.green < 1000) throw new Error("no menu on the LCD");
    if(OUT) writeFileSync(`${OUT}-menu.png`, Buffer.from((await send("Page.captureScreenshot", { format: "png" })).result.data, "base64"));

    // A starts Snake (the first entry): its score line, then it moves
    await key("Enter", "Enter", 13, 200);
    await sleep(1500);
    const a = await screen();
    await sleep(700);
    const b = await screen();
    const [t1, v1] = await ev("[performance.now(), window.__vnow]");
    const speed = (v1 - v0) / (t1 - t0);
    console.log(`snake: ${a?.white} white px, ${a?.red} red px, ${a?.hash !== b?.hash ? "moving" : "still"}; ` +
        `machine time at ${speed.toFixed(2)}x real time`);
    if(OUT) writeFileSync(`${OUT}-snake.png`, Buffer.from((await send("Page.captureScreenshot", { format: "png" })).result.data, "base64"));
    if(!a || a.white < 100 || a.hash === menu.hash) throw new Error("Snake didn't start");
    if(a.hash === b.hash) throw new Error("Snake isn't moving");
    if(speed < 0.8 || speed > 1.1) throw new Error("not at real time");

    // The high score page, initialised on the first start, is kept; a
    // reload starts from it
    await sleep(1500);
    const kept = await ev(`localStorage.getItem("v86-riscv-flash:minigameconsole")`);
    console.log(`kept: ${kept?.slice(0, 16)}...`);
    if(!kept || !kept.startsWith("42000000")) throw new Error("the high score page wasn't kept");
    await open();
    await sleep(2500);
    const second_tune = await tune();
    console.log(`reloaded: start-up tune ${second_tune} notes`);
    if(!(first_tune === 4 && second_tune === 8)) throw new Error("the kept flash didn't come back");
    if(exceptions) throw new Error(exceptions + " page exceptions");
    console.log("PASS");
}

main().then(() => { cleanup(); setTimeout(() => process.exit(0), 500); },
    e => { console.error("FAIL:", e.message); cleanup(); setTimeout(() => process.exit(1), 500); });
