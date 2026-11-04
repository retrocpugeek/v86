// Changes a microcontroller's firmware in the v86-riscv site's debugger
// (?debug) and runs the change: Tiny BASIC's sources are in the gdb-wui
// machine (/root/mcu/basic), which has GCC. The test edits its banner there,
// in that machine's console, then types "rebuild" in gdb's console
// (tools/gdb-wui-guest/S95gdb-wui): it compiles the firmware there, flashes
// it into the CH32V003 through the GDB stub and resets it, halted; Continue,
// and the CH32V003's serial console must show the new banner. Then the
// page's "Save your changes" buttons: the sources as a .zip (with the
// edit), and the firmware as .elf, .bin and .hex; and "Load sources", with
// that .zip (deflated), puts the edit back after it is undone.
//
//   SITE=http://localhost:8789 node tests/guest-web/mcu-rebuild.mjs [screenshot-prefix]
//
// Needs google-chrome and the site from tools/rv64-site (npx wrangler dev
// --config tools/rv64-site/wrangler.jsonc --port 8789).
import { spawn } from "node:child_process";
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const SITE = process.env.SITE || "http://localhost:8789";
const OUT = process.argv[2];
const PORT = +(process.env.PORT || 9351);
const t0 = Date.now();
const sleep = ms => new Promise(r => setTimeout(r, ms));

const profile = mkdtempSync(path.join(process.env.TMPDIR || tmpdir(), "v86-mcurebuild-"));
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
    await send("Page.navigate", { url: SITE + "/?build=ch32v003-basic&debug" });
    await sleep(3000);
    const hint = await ev("document.getElementById('debug-rebuild').hidden ? '' : document.getElementById('debug-rebuild').textContent");
    console.log("page's hint:", hint.slice(0, 120));
    if(!/rebuild/.test(hint)) throw new Error("the page doesn't say how to rebuild the firmware");

    await until("gdb-wui connected", "d.getElementById('conn')?.dataset.state === 'open'", +(process.env.CONNECT_TIMEOUT || 600));
    await until("attached, stopped at the reset vector", `${stopped} && /_start/.test(d.getElementById('stack').innerText)`, 180);

    // The change, made in the debugger's machine
    const linux = "document.getElementById('debug-terminal').textContent";
    await ev("debugger_emulator.serial0_send('root\\n')");
    await sleep(3000);
    await ev("debugger_emulator.serial0_send(\"sed -i 's/TINY BASIC FOR/REBUILT BASIC FOR/' /root/mcu/basic/ch32v003-basic/basic.c && echo EDITED\\n\")");
    const start = Date.now();
    while(!/\nEDITED|EDITED\s*#/.test(await ev(linux)))
    {
        if(Date.now() - start > 60000) throw new Error("the edit didn't happen: " + (await ev(linux)).slice(-400));
        await sleep(500);
    }
    console.log("edited basic.c");
    // (waits for the machine's console to show what a command echoes)
    const until_linux = async pattern =>
    {
        const start = Date.now();
        while(!pattern.test(await ev(linux)))
        {
            if(Date.now() - start > 60000) throw new Error(`not in the console: ${pattern}: ` + (await ev(linux)).slice(-400));
            await sleep(500);
        }
    };

    // A line typed into gdb's console, once it shows a prompt
    const typed = "d.getElementById('gdbconsole').innerText.trimEnd()";
    const type = async (line, answer, seconds = 60) =>
    {
        await until("gdb's prompt", `${typed}.endsWith("(gdb)")`, 60);
        await frame("d.querySelector('#gdbconsole .xterm-helper-textarea').focus()");
        await send("Input.insertText", { text: line });
        await send("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, text: "\r" });
        await send("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
        await until(`gdb answered ${line}`, `${answer}.test(${typed}) && ${typed}.endsWith("(gdb)")`, seconds);
    };
    await type("rebuild", "/Rebuilt, flashed and reset|The build failed/", +(process.env.BUILD_TIMEOUT || 900));
    const out = await frame(typed);
    console.log("  gdb:", out.slice(-700).replace(/\n+/g, "\n    "));
    if(!/Rebuilt, flashed/.test(out)) throw new Error("the build failed");
    if(!/text\s+data\s+bss/.test(out)) throw new Error("the build's output (size) isn't in gdb's console");
    // Halted at the reset vector, with the new machine's registers
    await type("print $pc", "/\\$\\d+ = .*_start/");
    await shot("rebuilt");

    // (the CH32V003's serial output, as it comes: its terminal on the page
    // isn't drawn while scrolled out of view)
    await ev("window.serial_out = ''; emulator.add_listener('serial0-output-byte', b => window.serial_out += String.fromCharCode(b)); 1");
    await frame("d.getElementById('btn-continue').click()");
    const serial = "window.serial_out";
    const ran = Date.now();
    while(!(await ev(serial)).includes("REBUILT BASIC FOR THE CH32V003"))
    {
        if(Date.now() - ran > 30000) throw new Error("no new banner: " + (await ev(serial)).slice(-300));
        await sleep(500);
    }
    console.log("the CH32V003 runs the rebuilt firmware:", (await ev(serial)).trim().slice(0, 80));
    if((await ev(serial)).includes("TINY BASIC FOR")) throw new Error("the old firmware ran too");
    await shot("running");

    // Saved to this computer: Chrome's downloads, into a directory of ours
    const downloads = mkdtempSync(path.join(process.env.TMPDIR || tmpdir(), "v86-mcusave-"));
    try
    {
        await send("Browser.setDownloadBehavior", { behavior: "allow", downloadPath: downloads });
        for(const [what, file] of [["sources", "basic.zip"], ["elf", "basic.elf"], ["bin", "basic.bin"], ["hex", "basic.hex"]])
        {
            await ev(`document.querySelector('#debug-save button[data-save=${what}]').click()`);
            const start = Date.now();
            while(!existsSync(path.join(downloads, file)) || !(await ev("document.getElementById('debug-save-status').textContent")).startsWith("Saved " + file))
            {
                if(Date.now() - start > 60000) throw new Error(`${file} wasn't saved: ${await ev("document.getElementById('debug-save-status').textContent")}`);
                await sleep(500);
            }
            await sleep(500);
            console.log("  " + await ev("document.getElementById('debug-save-status').textContent"));
        }
        const at = f => path.join(downloads, f);
        const listing = execFileSync("unzip", ["-l", at("basic.zip")]).toString();
        for(const f of ["basic/build", "basic/basic.elf", "basic/ch32v003-basic/basic.c", "basic/ch32v003/link.ld"])
        {
            if(!listing.includes(f)) throw new Error(f + " isn't in the .zip: " + listing);
        }
        const source = execFileSync("unzip", ["-p", at("basic.zip"), "basic/ch32v003-basic/basic.c"]).toString();
        if(!source.includes("REBUILT BASIC FOR")) throw new Error("the .zip's basic.c hasn't the edit");
        const elf = readFileSync(at("basic.elf")), bin = readFileSync(at("basic.bin")), hex = readFileSync(at("basic.hex")).toString();
        if(elf.readUInt32BE(0) !== 0x7f454c46) throw new Error("basic.elf isn't an ELF");
        if(!elf.includes(Buffer.from("REBUILT BASIC FOR"))) throw new Error("basic.elf isn't the rebuilt firmware");
        if(!bin.includes(Buffer.from("REBUILT BASIC FOR")) || bin.length < 4096) throw new Error("basic.bin isn't the rebuilt firmware");
        if(!/^:[0-9A-F]+\r?\n/.test(hex) || !hex.trimEnd().endsWith(":00000001FF")) throw new Error("basic.hex isn't Intel HEX");
        console.log(`saved: basic.zip with the edit, basic.elf, basic.bin (${bin.length} bytes), basic.hex`);

        // Loaded back: the edit undone in the machine, then the saved
        // sources loaded over it, as a .zip remade with zip(1) (deflated)
        await ev("debugger_emulator.serial0_send(\"sed -i 's/REBUILT BASIC FOR/UNDONE BASIC FOR/' /root/mcu/basic/ch32v003-basic/basic.c && chmod -x /root/mcu/basic/build && echo UNDONE\\n\")");
        await until_linux(/\nUNDONE|UNDONE\s*#/);
        const remade = path.join(downloads, "remade");
        execFileSync("unzip", ["-q", at("basic.zip"), "-d", remade]);
        execFileSync("zip", ["-qr", "../deflated.zip", "basic"], { cwd: remade });
        if(!execFileSync("unzip", ["-v", at("deflated.zip")]).toString().includes("Defl")) throw new Error("zip didn't deflate");
        const { root } = (await send("DOM.getDocument")).result;
        const { nodeId } = (await send("DOM.querySelector", { nodeId: root.nodeId, selector: "#debug-load-file" })).result;
        await send("DOM.setFileInputFiles", { nodeId, files: [at("deflated.zip")] });
        const loading = Date.now();
        while(!/^Loaded|^Couldn't/.test(await ev("document.getElementById('debug-save-status').textContent")))
        {
            if(Date.now() - loading > 60000) throw new Error("the .zip wasn't loaded: " + await ev("document.getElementById('debug-save-status').textContent"));
            await sleep(500);
        }
        const loaded = await ev("document.getElementById('debug-save-status').textContent");
        console.log("  " + loaded);
        if(!loaded.startsWith("Loaded")) throw new Error(loaded);
        await ev("debugger_emulator.serial0_send(\"grep -q 'REBUILT BASIC FOR' /root/mcu/basic/ch32v003-basic/basic.c && test -x /root/mcu/basic/build && echo LOAD''ED_OK\\n\")");
        await until_linux(/LOADED_OK/);
        console.log("loaded back: basic.c has the saved edit, and build is executable again");
    }
    finally
    {
        rmSync(downloads, { recursive: true, force: true });
    }
}

main().then(() => { cleanup(); setTimeout(() => process.exit(0), 500); },
    e => { console.error("FAIL:", e.message); cleanup(); setTimeout(() => process.exit(1), 500); });
