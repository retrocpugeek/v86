// Runs your own firmware on the v86-riscv site's CH32V003 in headless Chrome
// (?build=upload, tools/rv64-site/index.html, start_upload): a .bin, an ELF
// and an Intel HEX picked on the page, each on its board, and firmware from
// a URL (pasted into the page, then from ?firmware=), whose use of TIM1 (not
// modelled) the page must point out; and each board's list of examples.
//
//   SITE=http://localhost:8789 node tests/guest-web/mcu-upload.mjs [screenshot-prefix]
//
// Needs google-chrome, the site from tools/rv64-site (npx wrangler dev
// --config tools/rv64-site/wrangler.jsonc --port 8789) and the firmware
// built by tools/ch32v003/build.sh, tools/ch32v003-gameconsole/build.sh and
// tools/ch32v003-osillyscope/build.sh.

import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SITE = process.env.SITE || "http://localhost:8789";
const OUT = process.argv[2];
const PORT = +(process.env.PORT || 9351);
const sleep = ms => new Promise(r => setTimeout(r, ms));

const profile = mkdtempSync(path.join(process.env.TMPDIR || tmpdir(), "v86-mcuupload-"));
const chrome = spawn("google-chrome", ["--headless=new", "--no-first-run", "--user-data-dir=" + profile,
    "--remote-debugging-port=" + PORT, "about:blank"], { stdio: "ignore", detached: true });
// (google-chrome is a script: the browser is its child, in its process group)
const cleanup = () => { try { process.kill(-chrome.pid); } catch(e) {} setTimeout(() => { try { rmSync(profile, { recursive: true, force: true }); } catch(e) {} }, 300); };

// Tetris as Intel HEX: 16-byte data records from 0x0800 0000
function to_hex(bin)
{
    const rec = (type, addr, data) =>
    {
        const b = [data.length, addr >> 8 & 0xff, addr & 0xff, type, ...data];
        const sum = -b.reduce((s, x) => s + x, 0) & 0xff;
        return ":" + [...b, sum].map(x => x.toString(16).padStart(2, "0").toUpperCase()).join("");
    };
    const lines = [rec(4, 0, [0x08, 0x00])];
    for(let i = 0; i < bin.length; i += 16) lines.push(rec(0, i, [...bin.subarray(i, i + 16)]));
    lines.push(rec(1, 0, []));
    return lines.join("\n") + "\n";
}

// RV32EC at the reset vector: TIM1's CTLR1 = 1 (lui a0, 0x40013; addi a0,
// a0, -1024; li a1, 1; sw a1, 0(a0)), then j .
const TIM1_PROGRAM = new Uint8Array(new Uint32Array([0x40013537, 0xc0050513, 0x00100593, 0x00b52023, 0x0000006f]).buffer);

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
                throw new Error(`${what}: not within ${seconds} s (upload note "${await ev("document.getElementById('upload-status').textContent").catch(() => "?")}")`);
            }
            await sleep(250);
        }
    };
    await send("Runtime.enable");
    await send("Page.enable");
    const open = async url =>
    {
        await send("Page.navigate", { url });
        await until("page loaded", "document.readyState === 'complete' && !!document.getElementById('upload-status').textContent", 30);
    };
    // Pick a file on the page (it reloads, running it)
    const pick = async (file, board) =>
    {
        await ev(`document.getElementById('upload-board').value = ${JSON.stringify(board)}`);
        const root = (await send("DOM.getDocument")).result.root.nodeId;
        const node = (await send("DOM.querySelector", { nodeId: root, selector: "#upload-file" })).result.nodeId;
        await send("DOM.setFileInputFiles", { nodeId: node, files: [path.resolve(file)] });
        await until(`${path.basename(file)} running`, `window.emulator && document.getElementById('upload-status').textContent.startsWith('Running ${path.basename(file)}')`, 30);
    };
    const terminal = "document.getElementById('terminal').textContent";
    const lit = "(() => { const d = window.emulator.get_oled_display(0); return d && d.pixels && d.pixels.some(p => p); })()";
    const flash_row = () => ev("document.getElementById('mcu-flash').textContent");
    const unmodelled = () => ev("document.getElementById('upload-unmodelled').hidden ? '' : document.getElementById('upload-unmodelled').textContent");

    await open(`${SITE}/?build=upload`);
    if(!(await ev("document.getElementById('upload-status').textContent")).includes("No firmware yet"))
    {
        throw new Error("the page should ask for firmware first");
    }

    // A .bin on the game console: Tiny Tris draws on the OLED
    await pick("build/ch32v003-gameconsole/tiny_tris.bin", "gameconsole");
    await until("the game console's OLED lit", lit, 30);
    console.log("  flash:", await flash_row());
    await shot("bin");

    // An ELF on the osillyscope: it draws, and its sizes are shown
    await pick("build/ch32v003-osillyscope/osillyscope.elf", "osillyscope");
    await until("the osillyscope's OLED lit", lit, 30);
    if(!(await flash_row()).includes("code and constants")) throw new Error("an ELF's sizes should be shown: " + await flash_row());
    console.log("  flash:", await flash_row());
    await shot("elf");

    // Intel HEX on the serial console: Tetris draws there
    const hex = path.join(profile, "tetris.hex");
    writeFileSync(hex, to_hex(readFileSync("build/ch32v003/tetris.bin")));
    await pick(hex, "ch32v003");
    await until("Tetris on the serial console", `${terminal}.includes('TETRIS')`, 30);
    if(await unmodelled()) throw new Error("Tetris uses nothing unmodelled: " + await unmodelled());
    await shot("hex");

    // Each board's examples, linked to run and to debug (shown open with no
    // firmware: the one picked last is forgotten)
    await ev("localStorage.removeItem('v86-riscv-upload')");
    for(const board of ["ch32v003", "gameconsole", "minigameconsole", "osillyscope"])
    {
        await open(`${SITE}/?build=upload&board=${board}`);
        if(!await ev("document.getElementById('upload-examples').open")) throw new Error("the examples should show with no firmware running");
        const links = await ev("[...document.querySelectorAll('#upload-examples a')].map(a => a.getAttribute('href'))");
        const runs = links.filter(h => !h.endsWith("&debug"));
        if(!runs.length || runs.length * 2 !== links.length || !runs.every(h => h.includes("board=" + board) && h.includes("firmware=")))
        {
            throw new Error(`${board}'s examples: ${JSON.stringify(links).slice(0, 300)}`);
        }
        console.log(`  ${board}: ${runs.length} examples`);
        // This site's own run from their links
        for(const href of runs.filter(h => /firmware=examples%2F/.test(h)))
        {
            const name = decodeURIComponent(href.match(/firmware=examples%2F([^&]*)/)[1]);
            await open(`${SITE}/${href}`);
            await until(`  ${name} running`, `!!window.emulator && document.getElementById('upload-status').textContent.startsWith('Running ${name}')`, 30);
            await ev("localStorage.removeItem('v86-riscv-upload')");
        }
    }

    // A URL pasted into the page (a data: URL), firmware that uses TIM1: the
    // page says so, and has it in its address (?firmware=) for a reload
    const url = "data:application/octet-stream;base64," + Buffer.from(TIM1_PROGRAM).toString("base64");
    await ev(`document.getElementById('upload-url').value = ${JSON.stringify(url)}; document.getElementById('upload-url-form').requestSubmit()`);
    await until("the pasted URL in the address", `new URLSearchParams(location.search).get('firmware') === ${JSON.stringify(url)} && document.readyState === 'complete'`, 30);
    const text = await until("TIM1 pointed out", "(() => { const b = document.getElementById('upload-unmodelled'); return !b.hidden && b.textContent; })()", 15);
    if(!text.includes("TIM1")) throw new Error("expected TIM1: " + text);
    if(await ev("document.getElementById('upload-url').value") !== url) throw new Error("the URL box should show the firmware's URL");
    console.log("  " + text);
    await shot("url");

    // A file that isn't firmware for it
    await open(`${SITE}/?build=upload&firmware=${encodeURIComponent("data:application/octet-stream;base64," + Buffer.alloc(20000).toString("base64"))}`);
    await until("an oversized file refused", "document.getElementById('upload-status').textContent.includes('the flash holds')", 15);
    console.log("PASS");
}

main().then(() => { cleanup(); process.exit(0); }, e => { console.error("FAIL:", e.message); cleanup(); process.exit(1); });
