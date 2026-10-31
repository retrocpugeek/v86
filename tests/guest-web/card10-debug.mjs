// Debugs the card10's two cores on the v86-arm site in headless Chrome
// (?build=card10&debug): the page boots the gdb-wui image next to the badge,
// with a gdb and a gdb-wui per core, each connecting to its core's GDB stub
// through the page (tools/a64-site/index.html, start_card10_debug). Both
// must come up attached, each core stopped; then the AMP at work:
// - a breakpoint on epic_disp_update in each: Pycardium's (core 1, the API
//   call's caller) and Epicardium's (core 0, its implementation)
// - both continue: core 1 stops in its caller; it continues, and core 0 stops
//   in the implementation, called by its API dispatcher
// - with core 0 held there, core 1 waits in the API call: paused, it's in
//   _api_call_transact
// - the diagram shows the cores halted, and both run on at the end.
//
//   SITE=http://localhost:8788 node tests/guest-web/card10-debug.mjs [screenshot-prefix]
//
// Needs google-chrome and the site (npx wrangler dev --config
// tools/a64-site/wrangler.jsonc --port 8788) with the card10 and the gdb-wui
// image built with its firmware (tools/a64-site/gdb-wui/build.sh).
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SITE = process.env.SITE || "http://localhost:8788";
const OUT = process.argv[2];
const PORT = +(process.env.PORT || 9367);
const t0 = Date.now();
const sleep = ms => new Promise(r => setTimeout(r, ms));

const profile = mkdtempSync(path.join(process.env.TMPDIR || tmpdir(), "v86-card10debug-"));
const chrome = spawn("google-chrome", ["--headless=new", "--no-first-run", "--user-data-dir=" + profile,
    "--remote-debugging-port=" + PORT, "about:blank"], { stdio: "ignore", detached: true });
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
        const timer = setTimeout(() => reject(new Error(method + ": no answer from Chrome")), 60000);
        pending.set(++id, m => { clearTimeout(timer); resolve(m); });
        ws.send(JSON.stringify({ id, method, params }));
    });
    const ev = async expr =>
    {
        const r = (await send("Runtime.evaluate", { expression: expr, returnByValue: true })).result;
        if(r.exceptionDetails) throw new Error(expr.slice(0, 80) + ": " + r.exceptionDetails.exception?.description);
        return r.result.value;
    };
    // core n's gdb-wui
    const frame = (n, expr) => ev(`(() => { const d = document.getElementById('guest-web-core${n}').contentDocument; return d && (${expr}); })()`);
    const shot = async name =>
    {
        if(OUT) writeFileSync(`${OUT}-${name}.png`, Buffer.from((await send("Page.captureScreenshot", { format: "png" })).result.data, "base64"));
    };
    const until = async (what, n, expr, seconds) =>
    {
        const start = Date.now();
        let next_progress = Date.now() + 30000;
        for(;;)
        {
            if(Date.now() > next_progress)
            {
                next_progress += 30000;
                console.log(`  … ${((Date.now() - t0) / 1000).toFixed(0)} s: core 0 "${await ev("document.getElementById('debug-status-0').textContent")}", core 1 "${await ev("document.getElementById('debug-status-1').textContent")}"`);
            }
            const v = await frame(n, expr).catch(() => null);
            if(v) { console.log(`core ${n}: ${what}: ${((Date.now() - start) / 1000).toFixed(1)} s`); return v; }
            if(Date.now() - start > seconds * 1000)
            {
                await shot("timeout");
                const wui = await frame(n, "[d.getElementById('log').innerText, d.getElementById('run-state').dataset.state].join(' | ')").catch(e => e.message);
                console.log("  gdb-wui log:", String(wui).replace(/\n+/g, "\n    ").slice(-2000));
                throw new Error(`timed out: core ${n}: ${what}`);
            }
            await sleep(500);
        }
    };
    const stopped = "d.getElementById('run-state').dataset.state === 'stopped'";
    const running = "d.getElementById('run-state').dataset.state === 'running'";
    const stack = n => frame(n, "d.getElementById('stack').innerText.replace(/\\s+/g, ' ').slice(0, 200)");
    const typed = "d.getElementById('gdbconsole').innerText.trimEnd()";
    // A line typed into a core's gdb console; typed again if gdb-wui's
    // connection dropped meanwhile (it reconnects by itself)
    const type = async (n, line, answer) =>
    {
        for(let attempt = 1; ; attempt++)
        {
            await until("gdb's prompt", n, `${typed}.endsWith("(gdb)") && d.getElementById('conn')?.dataset.state === 'open'`, 120);
            await frame(n, "d.querySelector('#gdbconsole .xterm-helper-textarea').focus()");
            await send("Input.insertText", { text: line });
            await send("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, text: "\r" });
            await send("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
            try
            {
                await until(`gdb answered ${line}`, n, `${answer}.test(${typed}) && ${typed}.endsWith("(gdb)")`, 60);
                return;
            }
            catch(e)
            {
                if(attempt === 3) throw e;
                console.log(`  core ${n}: no answer to ${line}; again`);
            }
        }
    };
    const click = (n, button) => frame(n, `d.getElementById('${button}').click()`);
    const arch = () => ev("[...document.querySelectorAll('#debug-arch-svg text')].map(t => t.textContent).filter(t => /running|halted|packets/.test(t)).join(' | ')");

    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 1600, height: 2400, deviceScaleFactor: 1, mobile: false });
    await send("Page.navigate", { url: SITE + "/?build=card10&debug" });

    for(const n of [0, 1])
    {
        await until("gdb-wui connected", n, "d.getElementById('conn')?.dataset.state === 'open'", +(process.env.CONNECT_TIMEOUT || 900));
    }
    for(const n of [0, 1])
    {
        await until("attached, stopped", n, `${stopped} && d.getElementById('stack').innerText.trim().length > 0`, 240);
        console.log(`  core ${n}: ${await stack(n)}`);
    }
    console.log("  diagram:", await arch());
    // an M-profile core: xPSR (the stub's target description)
    await type(0, "info registers xpsr", "/xpsr\\s+0x/");
    await shot("attached");

    // The breakpoints, and both run
    await type(1, "break epic_disp_update", "/Breakpoint 1 at/");
    await type(0, "break epic_disp_update", "/Breakpoint 1 at/");
    await click(0, "btn-continue");
    await click(1, "btn-continue");
    await until("stopped in Pycardium's epic_disp_update (the caller)", 1, `${stopped} && d.getElementById('stack').innerText.includes('epic_disp_update')`, 120);
    console.log(`  core 1: ${await stack(1)}`);
    // core 1 makes the call: core 0 runs it
    await click(1, "btn-continue");
    await until("stopped in Epicardium's epic_disp_update (the call)", 0, `${stopped} && /epic_disp_update/.test(d.getElementById('stack').innerText) && /dispatch/.test(d.getElementById('stack').innerText)`, 120);
    console.log(`  core 0: ${await stack(0)}`);
    // core 1, meanwhile, waits for the call to return
    await sleep(1500);
    await click(1, "btn-pause");
    await until("paused, waiting in the API call", 1, `${stopped} && /_api_call_transact|SEMA_GetSema/.test(d.getElementById('stack').innerText)`, 60);
    console.log(`  core 1: ${await stack(1)}`);
    await sleep(1200);
    const diagram = await arch();
    console.log("  diagram:", diagram);
    if((diagram.match(/halted/g) || []).length < 2) throw new Error("the diagram doesn't show both cores halted");
    await shot("amp");

    // Both run on, the breakpoints gone
    await type(0, "delete", "/\\(gdb\\) delete/");
    await type(1, "delete", "/\\(gdb\\) delete/");
    await click(0, "btn-continue");
    await click(1, "btn-continue");
    await until("running", 0, running, 30);
    await until("running", 1, running, 30);
    const before = await ev("emulator.get_instruction_counter()");
    await sleep(3000);
    const ran = await ev("emulator.get_instruction_counter()") - before;
    console.log(`  ran ${(ran / 1e6).toFixed(1)} M instructions in 3 s`);
    if(ran < 1e6) throw new Error("the badge didn't run on");
    if(exceptions) throw new Error(exceptions + " page exceptions");
    console.log("PASS");
}

main().then(() => { cleanup(); setTimeout(() => process.exit(0), 500); },
    e => { console.error("FAIL:", e.message); cleanup(); setTimeout(() => process.exit(1), 500); });
