#!/usr/bin/env node
// Runs examples/aarch64.html in headless Chrome and checks that it boots to a
// login prompt, gets a DHCP lease over virtio-net, logs in, runs a command and
// fetches a page from the test web server through the guest's network.
//
//   tests/a64/browser.mjs
//
// Expects the images described in examples/aarch64.html, build/libv86.js and
// build/v86-a64.wasm. Set CHROME to override the browser binary.

import { spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import url from "node:url";

const ROOT = path.resolve(url.fileURLToPath(new URL(".", import.meta.url)), "../..");
const HTTP_PORT = 8086;
const DEBUG_PORT = 9333;
const TIMEOUT_MS = 10 * 60 * 1000;

const sleep = ms => new Promise(r => setTimeout(r, ms));

const server = spawn("python3", ["-m", "http.server", "--bind", "127.0.0.1", String(HTTP_PORT)], { cwd: ROOT, stdio: "ignore" });
const profile = mkdtempSync(path.join(tmpdir(), "v86-a64-chrome-"));
const chrome = spawn(process.env.CHROME || "google-chrome", [
    "--headless=new", "--no-first-run", "--no-default-browser-check",
    "--user-data-dir=" + profile, "--remote-debugging-port=" + DEBUG_PORT, "about:blank",
], { stdio: "ignore" });

function cleanup()
{
    chrome.kill();
    server.kill();
    try { rmSync(profile, { recursive: true, force: true }); } catch(e) {}
}

async function main()
{
    let targets;
    for(let i = 0; i < 50 && !targets; i++)
    {
        await sleep(200);
        try { targets = await (await fetch(`http://127.0.0.1:${DEBUG_PORT}/json/list`)).json(); } catch(e) {}
    }
    const page = targets.find(t => t.type === "page");
    const ws = new WebSocket(page.webSocketDebuggerUrl);
    await new Promise(r => ws.addEventListener("open", r));

    let id = 0;
    const pending = new Map();
    ws.addEventListener("message", e =>
    {
        const msg = JSON.parse(e.data);
        if(msg.id && pending.has(msg.id))
        {
            pending.get(msg.id)(msg);
            pending.delete(msg.id);
        }
        else if(msg.method === "Runtime.exceptionThrown")
        {
            console.error("page exception:", JSON.stringify(msg.params.exceptionDetails).slice(0, 500));
        }
    });
    const send = (method, params = {}) => new Promise(r =>
    {
        pending.set(++id, r);
        ws.send(JSON.stringify({ id, method, params }));
    });
    const evaluate = async expr => (await send("Runtime.evaluate", { expression: expr, returnByValue: true })).result.result.value;
    const serial = () => evaluate("document.getElementById('serial').value");

    async function wait_for(text)
    {
        const end = Date.now() + TIMEOUT_MS;
        while(Date.now() < end)
        {
            const s = await serial();
            if(s && s.includes(text)) return s;
            await sleep(500);
        }
        throw new Error("timed out waiting for " + JSON.stringify(text));
    }

    await send("Runtime.enable");
    const start = Date.now();
    // localhost (not 127.0.0.1): <port>.external is fetched from localhost, which
    // must be same-origin since the test server sends no CORS headers.
    await send("Page.navigate", { url: `http://localhost:${HTTP_PORT}/examples/aarch64.html` });

    await wait_for("udhcpc: lease of");
    await wait_for("login:");
    console.log(`[browser] login prompt after ${((Date.now() - start) / 1000).toFixed(1)}s`);
    await evaluate("emulator.serial0_send('root\\n')");
    await wait_for("# ");
    await evaluate("emulator.serial0_send('uname -m; echo browser-$((6*7))\\n')");
    let out = await wait_for("browser-42");
    console.log(out.split("\n").slice(-6).join("\n"));
    await evaluate(`emulator.serial0_send('wget -qO- http://${HTTP_PORT}.external/examples/aarch64.html | grep -c "aarch64 example" | sed s/^/wget-ok-/\\n')`);
    out = await wait_for("wget-ok-1");
    console.log(out.split("\n").slice(-3).join("\n"));
    console.log(`[browser] PASS in ${((Date.now() - start) / 1000).toFixed(1)}s`);
}

main().then(() => { cleanup(); process.exit(0); }, e => { console.error("[browser] FAIL:", e.message); cleanup(); process.exit(1); });
