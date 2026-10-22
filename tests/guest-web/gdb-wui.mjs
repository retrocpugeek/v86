// Boots a "Buildroot + gdb-wui" image (tools/gdb-wui-guest) on the v86-arm or
// v86-riscv site in headless Chrome and debugs a program with the gdb-wui
// panel: load /root/demo/hello, run to main, step over a line; open it in a
// new tab; then debug hello under a gdbserver started on the guest's console
// (gdb-wui's remote target).
//
//   SITE=http://localhost:8788 BUILD=gdb-wui node tests/guest-web/gdb-wui.mjs [screenshot-prefix]
//
// Needs google-chrome. The site comes from tools/a64-site or tools/rv64-site
// (e.g. npx wrangler dev --config tools/a64-site/wrangler.jsonc --port 8788).
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SITE = process.env.SITE || "http://localhost:8788";
const BUILD = process.env.BUILD || "gdb-wui";
const OUT = process.argv[2];
const PORT = +(process.env.PORT || 9348);
const t0 = Date.now();
const sleep = ms => new Promise(r => setTimeout(r, ms));

const profile = mkdtempSync(path.join(tmpdir(), "v86-gdbwui-"));
const chrome = spawn("google-chrome", ["--headless=new", "--no-first-run", "--user-data-dir=" + profile,
    "--remote-debugging-port=" + PORT, "about:blank"], { stdio: "ignore" });
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
        else if(m.method === "Runtime.consoleAPICalled" && /warn|error/.test(m.params.type))
            console.error("console:", m.params.args.map(a => a.value ?? a.description).join(" ").slice(0, 300));
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
    // Evaluated in gdb-wui's document (the iframe is same-origin)
    const doc = "document.getElementById('guest-web').contentDocument";
    const frame = expr => ev(`(() => { const d = ${doc}; return d && (${expr}); })()`);
    const progress = async () =>
    {
        const web = await ev("document.getElementById('web-status').textContent");
        const out = (await ev("window.__o || ''")).trim().split("\n").pop().slice(0, 100);
        console.log(`  … ${((Date.now() - t0) / 1000).toFixed(0)} s: panel "${web}", console "${out}"`);
    };
    const until = async (what, expr, seconds) =>
    {
        const t0 = Date.now();
        let next_progress = Date.now() + 30000;
        for(;;)
        {
            if(Date.now() > next_progress) { next_progress += 30000; await progress().catch(e => console.log("  … " + e.message)); }
            const v = await frame(expr).catch(() => null);
            if(v) { console.log(`${what}: ${((Date.now() - t0) / 1000).toFixed(1)} s`); return v; }
            if(Date.now() - t0 > seconds * 1000)
            {
                if(OUT) await shot("timeout");
                const log = (await ev("window.__o || ''")).split("\n").filter(l => /random|crng|gdb-wui|error|fail/i.test(l));
                console.log(log.map(l => "  | " + l.trim()).join("\n"));
                // gdb-wui's own log (the Log tab), and what it says about the program
                const wui = await frame("[d.getElementById('log').innerText, d.getElementById('exe-name').textContent, d.getElementById('run-state').dataset.state, d.getElementById('status-message')?.textContent].join(' | ')").catch(e => e.message);
                console.log("  gdb-wui log:", String(wui).replace(/\n+/g, "\n    ").slice(-3000));
                throw new Error(`timed out: ${what} (web status: ${await ev("document.getElementById('web-status').textContent")})`);
            }
            await sleep(500);
        }
    };
    const shot = async name =>
    {
        writeFileSync(`${OUT}-${name}.png`, Buffer.from((await send("Page.captureScreenshot", { format: "png" })).result.data, "base64"));
    };

    await send("Runtime.enable");
    // MOBILE=1: a phone-sized page
    await send("Emulation.setDeviceMetricsOverride", process.env.MOBILE ?
        { width: 390, height: 844, deviceScaleFactor: 2, mobile: true } :
        { width: 1400, height: 1500, deviceScaleFactor: 1, mobile: false });
    await send("Page.navigate", { url: SITE + "/?build=" + BUILD });
    const hook = "window.emulator && !window.__o && (window.__o = '', emulator.add_listener('serial0-output-byte', b => window.__o += String.fromCharCode(b)))";
    for(let i = 0; i < 100 && !(await ev(hook + "; typeof window.__o === 'string'")); i++) await sleep(100);

    await until("gdb-wui connected", "d.getElementById('conn')?.dataset.state === 'open'", +(process.env.CONNECT_TIMEOUT || 600));
    console.log(`  (${((Date.now() - t0) / 1000).toFixed(0)} s since the page opened)`);
    await until("demo files listed", "d.querySelector('.tree-row[data-path=\"hello\"]')", 60);
    await frame("d.querySelector('.tree-row[data-path=\"hello\"]').click()");
    await until("hello loaded", "d.getElementById('exe-name').textContent.includes('hello')", 120);
    await frame("d.getElementById('btn-run-main').click()");
    await until("stopped in main", "d.getElementById('run-state').dataset.state === 'stopped' && d.getElementById('source-path').textContent.includes('hello.c')", 180);
    const where = () => frame("(d.getElementById('stack').textContent.match(/hello\\.c:\\d+/) || [''])[0]");
    console.log("  at:", await where());
    await frame("d.getElementById('btn-next').click()");
    await until("stepped over a line", "d.getElementById('run-state').dataset.state === 'stopped' && /hello\\.c:14\\b/.test(d.getElementById('stack').textContent)", 120);
    console.log("  at:", await where());
    console.log("  locals:", (await frame("d.getElementById('variables').textContent")).replace(/\s+/g, " ").slice(0, 120));

    // The Decompiled tab, from the image's sidecars (no Ghidra in the guest)
    if(!process.env.NO_DECOMP)
    {
        await frame("d.querySelector('button.tab[data-center=decomp]').click()");
        await until("hello's main decompiled", "/main\\s*\\(/.test(d.getElementById('decomp').textContent)", 120);
        // busybox: a few thousand functions, read from its sidecar
        await frame("d.getElementById('btn-kill').click()");
        await sleep(1000);
        await frame("!d.getElementById('confirm').classList.contains('is-hidden') && d.getElementById('confirm-yes').click()");
        await frame("d.querySelector('.tree-row[data-path=\"busybox\"]').click()");
        // (the page asks before replacing a program it still thinks is live)
        await sleep(300);
        await frame("!d.getElementById('confirm').classList.contains('is-hidden') && d.getElementById('confirm-yes').click()");
        await until("busybox loaded", "d.getElementById('exe-name').textContent.includes('busybox')", 120);
        // As the try-online page says. busybox is the image's /bin/busybox,
        // stripped: every name here is the decompiler's.
        // A line typed into gdb's console, once it has answered the last one
        // and shows a fresh prompt (a line typed before that can be lost)
        const typed = "d.getElementById('gdbconsole').innerText.trimEnd()";
        const type = async (line, answer) => {
            await until("gdb's prompt", `${typed}.endsWith("(gdb)")`, 60);
            await frame("d.querySelector('#gdbconsole .xterm-helper-textarea').focus()");
            await send("Input.insertText", { text: line });
            await send("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, text: "\r" });
            await send("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
            await until(`gdb answered ${line}`, `${answer}.test(${typed}) && ${typed}.endsWith("(gdb)")`, 60);
        };
        // A right-click on an element, and its menu's breakpoint item
        const breakVia = async (what, el) => {
            await frame(`(() => { const el = ${el}; const b = el.getBoundingClientRect();
                el.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: b.left + 3, clientY: b.top + 3, button: 2 })); })()`);
            await until(`menu for ${what}`, "!d.getElementById('ctxmenu').classList.contains('is-hidden')", 30);
            await frame("[...d.querySelectorAll('#ctxmenu button, #ctxmenu [role=menuitem]')].find(e => /breakpoint/i.test(e.textContent)).click()");
        };
        const breakOnSymbol = async name => {
            await frame(`(() => { const i = d.getElementById('symbols-search'); i.value = ${JSON.stringify(name)}; i.dispatchEvent(new Event('input', { bubbles: true })); })()`);
            await until(`the decompiler's ${name} listed`, `[...d.querySelectorAll('#symbols .sym-row .list-main')].some(e => e.textContent === ${JSON.stringify(name)})`, 300);
            await breakVia(name, `[...d.querySelectorAll('#symbols .sym-row')].find(r => r.querySelector('.list-main').textContent === ${JSON.stringify(name)})`);
        };
        await type("set args ls -l /", "/\\(gdb\\) set args ls -l \\//");
        // Started first: busybox is position-independent, and the decompiler's
        // addresses mean nothing until it has been loaded somewhere
        await frame("d.getElementById('btn-run-entry').click()");
        await until("busybox started", "d.getElementById('run-state').dataset.state === 'stopped'", 300);
        await breakOnSymbol("entry");
        await frame("d.getElementById('btn-continue').click()");
        await until("busybox stopped at entry", "d.getElementById('run-state').dataset.state === 'stopped' && /entry/.test(d.getElementById('stack').innerText)", 300);
        // (a stop with no source shows the disassembly)
        await frame("d.querySelector('button.tab[data-center=decomp]').click()");
        await until("entry decompiled", "/__libc_start_main\\(/.test(d.getElementById('decomp').textContent)", 300);
        // main is what entry hands __libc_start_main, by whatever name Ghidra gave it
        const mainName = await frame("d.getElementById('decomp').textContent.match(/__libc_start_main\\(([A-Za-z_]\\w*)/)[1]");
        console.log("  entry calls __libc_start_main(" + mainName + ", ...)");
        await breakOnSymbol(mainName);
        await frame("d.getElementById('btn-continue').click()");
        await until("busybox stopped in its main", `d.getElementById('run-state').dataset.state === 'stopped' && d.getElementById('stack').innerText.includes(${JSON.stringify(mainName)})`, 300);
        await frame("d.querySelector('button.tab[data-center=decomp]').click()");
        await until("busybox's main decompiled", `d.querySelector('#center').innerText.includes(${JSON.stringify(mainName + " ·")}) && /"busybox"/.test(d.getElementById('decomp').textContent)`, 120);
        console.log("  busybox:", (await frame("d.getElementById('decomp').innerText")).replace(/\s+/g, " ").slice(0, 200));
        // busybox's breakpoints mean nothing in the programs after it
        await type("delete", "/\\(gdb\\) delete/");
        await until("breakpoints deleted", "!d.querySelector('#breakpoints .list-row')", 30);
        if(OUT) await shot("busybox");
        await frame("d.getElementById('btn-kill').click()");
        await sleep(1000);
        await frame("!d.getElementById('confirm').classList.contains('is-hidden') && d.getElementById('confirm-yes').click()");
        await frame("d.querySelector('.tree-row[data-path=\"hello\"]').click()");
        // (the page asks before replacing a program it still thinks is live)
        await sleep(300);
        await frame("!d.getElementById('confirm').classList.contains('is-hidden') && d.getElementById('confirm-yes').click()");
        await until("hello loaded again", "d.getElementById('exe-name').textContent.includes('hello')", 120);
    }
    if(OUT) await shot("stopped");

    // "Open in new tab": the tab's WebSocket goes through this page (its opener)
    await send("Runtime.evaluate", { expression: "window.__tab = window.open('/gdb-wui/', '_blank')", userGesture: true });
    const t1 = Date.now();
    for(;;)
    {
        const state = await ev("(() => { try { return window.__tab.document.getElementById('conn').dataset.state; } catch(e) { return ''; } })()");
        if(state === "open") break;
        if(Date.now() - t1 > 120000)
        {
            const info = await ev("(() => { const w = window.__tab; if(!w) return 'no window'; try { return [w.location.href, w.document.readyState, w.document.title, (w.document.body?.innerText || '').slice(0, 200)].join(' | '); } catch(e) { return 'error: ' + e.message; } })()");
            throw new Error("the new tab didn't connect (" + state + "): " + info);
        }
        await sleep(500);
    }
    console.log(`new tab connected: ${((Date.now() - t1) / 1000).toFixed(1)} s, showing`,
        await ev("window.__tab.document.getElementById('exe-name').textContent"));
    await ev("window.__tab.close()");

    // A gdbserver in the guest, started on the console, as gdb-wui's remote target
    await frame("d.getElementById('btn-kill').click()");
    await sleep(1000);
    await frame("!d.getElementById('confirm').classList.contains('is-hidden') && d.getElementById('confirm-yes').click()");
    await ev("emulator.serial0_send('root\\n')");
    await sleep(2000);
    await ev("emulator.serial0_send('gdbserver :1234 /root/demo/hello &\\n')");
    for(let i = 0; i < 60 && !(await ev("window.__o.includes('Listening on port 1234')")); i++) await sleep(500);
    await frame("d.getElementById('remote-connect').click()");
    await until("connected to gdbserver", "d.getElementById('remote-state').dataset.remote !== 'off' && d.getElementById('run-state').dataset.state === 'stopped'", 120);
    console.log("  remote:", await frame("d.getElementById('remote-state').textContent"),
        "| stack:", (await frame("d.getElementById('stack').textContent")).replace(/\s+/g, " ").slice(0, 100));
    await frame("d.getElementById('btn-continue').click()");
    await until("program ran to the end under gdbserver", "/total=3 argc=1/.test(window.__o)", 120);
}

main().then(() => { cleanup(); process.exit(0); }, e => { console.error("FAIL:", e.message); cleanup(); process.exit(1); });
