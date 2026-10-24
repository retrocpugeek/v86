// The guest's web interface next to the console, for the emulator pages of
// tools/a64-site and tools/rv64-site (start_guest_web; served with sw.js).
"use strict";

// The guest's web interface (build.web, e.g. OpenWrt's /cgi-bin/luci/) in an
// iframe. sw.js serves build.web's first directory (e.g. /cgi-bin/) by passing
// each request here; guest_http sends it to the guest's port 80 over v86's
// virtual network (the "fetch" adapter's TCP stack).

// The kinds of web interface (build.web_app; default: a plain web server
// serving build.web, like OpenWrt's LuCI)
const WEB_APPS = {
    default: {
        title: "Web interface",
        static: path => path.startsWith("/luci-static/"),
    },
    // gdb-wui (tools/rv64-gdb): served at / in the guest and at build.web
    // (/gdb-wui/) here. Its WebSocket can't go through sw.js, so its page gets
    // a script that replaces WebSocket with guest_websocket; and it's logged
    // in with a link that the guest's port 81 hands out (gdb-wui -print-url).
    "gdb-wui": {
        title: "gdb-wui",
        prefix: "/gdb-wui",
        static: path => /^\/(css|img|js|vendor)\//.test(path),
        websocket: true,
        login_port: 81,
        // its CSP's frame-ancestors 'none' would keep it out of the iframe, and
        // its COOP would cut a new tab off from this page (its opener)
        drop_headers: ["content-security-policy", "cross-origin-opener-policy"],
        height: "760px",
        wide: true,
        // its decompiler: the guest's gdb-wui -kuna tcp:192.168.86.1:5717
        // (tools/gdb-wui-guest/S95gdb-wui) is answered by serve_kuna
        kuna_port: 5717,
        // a debugger wants a wide screen and a keyboard
        desktop_note: "gdb-wui is made for a desktop browser: it needs a wide screen, a mouse and a keyboard, and on a phone most of it is out of reach.",
    },
};
let guest_web = WEB_APPS.default;

const FORWARD_HEADERS = new Set(["accept", "accept-language", "authorization", "content-type", "x-requested-with"]);
const DROP_RESPONSE_HEADERS = new Set(["connection", "content-length", "keep-alive", "set-cookie", "transfer-encoding"]);

// Responses from sw.js can't set cookies, so LuCI's session cookie is kept here
const guest_cookies = new Map();
// The guest's static files, kept for this page's lifetime (they're slow to serve)
const guest_static_cache = new Map();

function concat(chunks)
{
    const out = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
    let offset = 0;
    for(const c of chunks)
    {
        out.set(c, offset);
        offset += c.length;
    }
    return out;
}

function decode_chunked(data)
{
    const chunks = [];
    let i = 0;
    for(;;)
    {
        const eol = data.indexOf(13, i);
        if(eol < 0) break;
        const size = parseInt(new TextDecoder().decode(data.subarray(i, eol)), 16);
        if(!(size > 0)) break;
        chunks.push(data.subarray(eol + 2, eol + 2 + size));
        i = eol + 2 + size + 2;
    }
    return concat(chunks);
}

function update_cookie(set_cookie)
{
    const [pair, ...attributes] = set_cookie.split(";");
    const eq = pair.indexOf("=");
    if(eq < 1) return;
    const name = pair.slice(0, eq).trim(), value = pair.slice(eq + 1).trim();
    const expired = attributes.some(a =>
    {
        const [k, v = ""] = a.split("=").map(x => x.trim());
        return k.toLowerCase() === "max-age" && +v <= 0 ||
            k.toLowerCase() === "expires" && Date.parse(v) < Date.now();
    });
    if(expired || value === "") guest_cookies.delete(name);
    else guest_cookies.set(name, value);
}

function parse_response(data, app)
{
    let end = -1;
    for(let i = 0; i + 3 < data.length; i++)
    {
        if(data[i] === 13 && data[i + 1] === 10 && data[i + 2] === 13 && data[i + 3] === 10) { end = i; break; }
    }
    if(end < 0) return { error: "The guest's web server sent an incomplete response." };

    const [status_line, ...lines] = new TextDecoder().decode(data.subarray(0, end)).split("\r\n");
    const m = status_line.match(/^HTTP\/1\.[01] (\d{3}) ?(.*)$/);
    if(!m) return { error: "The guest's web server sent an invalid response." };

    let body = data.subarray(end + 4);
    const headers = [];
    for(const line of lines)
    {
        const colon = line.indexOf(":");
        if(colon < 1) continue;
        const name = line.slice(0, colon).trim().toLowerCase();
        let value = line.slice(colon + 1).trim();
        if(name === "set-cookie") update_cookie(value);
        if(name === "transfer-encoding" && value.toLowerCase() === "chunked") body = decode_chunked(body);
        if(name === "location")
        {
            value = value.replace(/^https?:\/\/[^/]*/i, "");
            if(app.prefix && value.startsWith("/")) value = app.prefix + value;
        }
        if(!DROP_RESPONSE_HEADERS.has(name) && !(app.drop_headers || []).includes(name)) headers.push([name, value]);
    }
    if(app.websocket && headers.some(([name, value]) => name === "content-type" && value.startsWith("text/html")))
    {
        // Before the page's own scripts (classic scripts in <head> run first)
        const html = new TextDecoder().decode(body);
        const tag = `<script src="${app.prefix}/${WEBSOCKET_SHIM}"><\/script>`;
        body = new TextEncoder().encode(/<head[^>]*>/i.test(html) ? html.replace(/<head[^>]*>/i, m => m + tag) : tag + html);
    }
    return { status: +m[1], status_text: m[2], headers, body: body.slice().buffer };
}

// The path in the guest for a path here: build.web's prefix removed
function guest_path(path)
{
    const prefix = guest_web.prefix;
    if(prefix && (path === prefix || path.startsWith(prefix + "/") || path.startsWith(prefix + "?")))
    {
        path = path.slice(prefix.length);
        return path.startsWith("/") ? path : "/" + path;
    }
    return path;
}

function guest_http(adapter, request)
{
    const path = guest_path(request.path);
    if(guest_web.websocket && path === "/" + WEBSOCKET_SHIM)
    {
        return Promise.resolve({
            status: 200, status_text: "OK",
            headers: [["content-type", "text/javascript"], ["cache-control", "no-store"]],
            body: new TextEncoder().encode(WEBSOCKET_SHIM_SOURCE).buffer,
        });
    }
    const cacheable = request.method === "GET" && guest_web.static(path);
    if(cacheable && guest_static_cache.has(path))
    {
        return Promise.resolve(guest_static_cache.get(path));
    }

    // HTTP/1.0, so the server closes the connection after the response
    const head = [`${request.method} ${path} HTTP/1.0`, `Host: ${location.host}`, "Connection: close"];
    // (gdb-wui wants an Origin on requests other than GET)
    if(request.method !== "GET" && request.method !== "HEAD") head.push(`Origin: ${location.origin}`);
    for(const [name, value] of request.headers)
    {
        if(FORWARD_HEADERS.has(name)) head.push(`${name}: ${value}`);
    }
    if(guest_cookies.size) head.push("Cookie: " + [...guest_cookies].map(([k, v]) => k + "=" + v).join("; "));
    if(request.body) head.push("Content-Length: " + request.body.byteLength);
    const data = [new TextEncoder().encode(head.join("\r\n") + "\r\n\r\n")];
    if(request.body) data.push(new Uint8Array(request.body));

    // v86's TCP stack doesn't retransmit, so a packet the guest drops (its
    // receive ring is full when many requests arrive at once) stalls the
    // connection: limit concurrent connections, and retry failed attempts
    // (those that may have reached the server only for GET and HEAD).
    const idempotent = request.method === "GET" || request.method === "HEAD";
    return with_guest_connection_slot(async () =>
    {
        for(let attempt = 1; ; attempt++)
        {
            const result = await guest_http_attempt(adapter, data);
            if(result.reply)
            {
                if(cacheable && result.reply.status === 200) guest_static_cache.set(path, result.reply);
                return result.reply;
            }
            console.warn(`guest http: ${request.method} ${path}: ${result.error} (attempt ${attempt})`);
            if(attempt === GUEST_HTTP_ATTEMPTS || result.sent && !idempotent)
            {
                return { error: result.error };
            }
            await new Promise(resolve => setTimeout(resolve, 500 * attempt));
        }
    });
}

const GUEST_HTTP_ATTEMPTS = 3;
const GUEST_CONNECT_TIMEOUT = 5000;
const GUEST_IDLE_TIMEOUT = 30000;
const GUEST_MAX_CONNECTIONS = 4;
let guest_connections = 0;
const guest_connection_queue = [];

async function with_guest_connection_slot(f)
{
    if(guest_connections >= GUEST_MAX_CONNECTIONS)
    {
        await new Promise(resolve => guest_connection_queue.push(resolve));
    }
    else
    {
        guest_connections++;
    }
    try
    {
        return await f();
    }
    finally
    {
        // Hand the slot to the next waiting request, if any
        const next = guest_connection_queue.shift();
        if(next) next();
        else guest_connections--;
    }
}

// One connection to the guest's port 80: resolves to { reply } or
// { error, sent } (sent: the request may have reached the server)
function guest_http_attempt(adapter, data)
{
    return new Promise(resolve =>
    {
        const chunks = [];
        let done = false, sent = false;
        const conn = adapter.connect(80);
        let timer = setTimeout(() => fail("no connection to the guest's web server"), GUEST_CONNECT_TIMEOUT);

        function fail(error)
        {
            finish({ error, sent });
        }
        function finish(result)
        {
            if(done) return;
            done = true;
            clearTimeout(timer);
            conn.close();
            resolve(result);
        }
        function complete()
        {
            const reply = parse_response(concat(chunks), guest_web);
            if(reply.error) fail(reply.error);
            else finish({ reply });
        }
        function wait_for_data()
        {
            clearTimeout(timer);
            timer = setTimeout(() => fail("the guest's web server stopped answering"), GUEST_IDLE_TIMEOUT);
        }

        conn.on("connect", () =>
        {
            sent = true;
            wait_for_data();
            for(const d of data) conn.write(d);
        });
        conn.on("data", d =>
        {
            chunks.push(d.slice());
            wait_for_data();
        });
        conn.on("shutdown", complete);
        conn.on("close", () => chunks.length ? complete() : fail("the guest refused the connection"));
    });
}

// Resolves to whether the guest accepts connections on a port
function guest_port_open(adapter, port = 80)
{
    return new Promise(resolve =>
    {
        const conn = adapter.connect(port);
        const timer = setTimeout(() => { conn.close(); resolve(false); }, 4000);
        conn.on("connect", () => { clearTimeout(timer); conn.close(); resolve(true); });
        conn.on("close", () => { clearTimeout(timer); resolve(false); });
    });
}

function active_worker(registration)
{
    return new Promise(resolve =>
    {
        if(registration.active) return resolve(registration.active);
        const worker = registration.installing || registration.waiting;
        worker.addEventListener("statechange", () =>
        {
            if(worker.state === "activated") resolve(worker);
        });
    });
}

// A WebSocket to the guest's port 80 (RFC 6455 over the virtual network's
// TCP): calls on.open(), on.message(data, binary) with a string or a
// Uint8Array, and on.close(code, reason, clean) once; returns { send, close }.
function guest_websocket(adapter, path, on)
{
    const key = btoa(String.fromCharCode(...crypto.getRandomValues(new Uint8Array(16))));
    const head = [`GET ${guest_path(path)} HTTP/1.1`, `Host: ${location.host}`, "Upgrade: websocket", "Connection: Upgrade",
        `Sec-WebSocket-Key: ${key}`, "Sec-WebSocket-Version: 13", `Origin: ${location.origin}`];
    if(guest_cookies.size) head.push("Cookie: " + [...guest_cookies].map(([k, v]) => k + "=" + v).join("; "));

    const conn = adapter.connect(80);
    let open = false, closed = false, close_sent = false;
    let buffer = new Uint8Array(0);
    let message = [], message_opcode = 0;
    const timer = setTimeout(() => finish(1006, "", false), GUEST_CONNECT_TIMEOUT);

    function finish(code, reason, clean)
    {
        if(closed) return;
        closed = true;
        clearTimeout(timer);
        conn.close();
        on.close(code, reason, clean);
    }
    // Client frames are masked (RFC 6455 5.3)
    function send_frame(opcode, payload)
    {
        const n = payload.length;
        const length_bytes = n < 126 ? 0 : n < 65536 ? 2 : 8;
        const frame = new Uint8Array(2 + length_bytes + 4 + n);
        frame[0] = 0x80 | opcode;
        frame[1] = 0x80 | (n < 126 ? n : n < 65536 ? 126 : 127);
        if(length_bytes === 2) new DataView(frame.buffer).setUint16(2, n);
        if(length_bytes === 8) new DataView(frame.buffer).setUint32(6, n);
        const mask = crypto.getRandomValues(new Uint8Array(4));
        const at = 2 + length_bytes + 4;
        frame.set(mask, at - 4);
        for(let i = 0; i < n; i++) frame[at + i] = payload[i] ^ mask[i & 3];
        conn.write(frame);
    }
    function read_frames()
    {
        for(;;)
        {
            if(buffer.length < 2) return;
            const view = new DataView(buffer.buffer, buffer.byteOffset, buffer.length);
            const fin = buffer[0] & 0x80, opcode = buffer[0] & 15, masked = buffer[1] & 0x80;
            let n = buffer[1] & 127, at = 2;
            if(n === 126)
            {
                if(buffer.length < 4) return;
                n = view.getUint16(2);
                at = 4;
            }
            else if(n === 127)
            {
                if(buffer.length < 10) return;
                n = view.getUint32(2) * 2 ** 32 + view.getUint32(6);
                at = 10;
            }
            if(buffer.length < at + (masked ? 4 : 0) + n) return;
            const mask = masked ? buffer.slice(at, at + 4) : null;
            if(masked) at += 4;
            const payload = buffer.slice(at, at + n);
            if(mask) for(let i = 0; i < n; i++) payload[i] ^= mask[i & 3];
            buffer = buffer.subarray(at + n);

            if(opcode === 8)
            {
                const code = n >= 2 ? payload[0] << 8 | payload[1] : 1005;
                if(!close_sent) send_frame(8, payload.subarray(0, Math.min(n, 2)));
                finish(code, new TextDecoder().decode(payload.subarray(2)), true);
                return;
            }
            if(opcode === 9) { send_frame(10, payload); continue; }
            if(opcode === 10) continue;
            if(opcode !== 0)
            {
                message_opcode = opcode;
                message = [];
            }
            message.push(payload);
            if(fin)
            {
                const data = concat(message);
                message = [];
                if(message_opcode === 1) on.message(new TextDecoder().decode(data), false);
                else on.message(data, true);
            }
        }
    }

    conn.on("connect", () => conn.write(new TextEncoder().encode(head.join("\r\n") + "\r\n\r\n")));
    conn.on("data", d =>
    {
        if(closed) return;
        buffer = concat([buffer, d]);
        if(!open)
        {
            let end = -1;
            for(let i = 0; i + 3 < buffer.length; i++)
            {
                if(buffer[i] === 13 && buffer[i + 1] === 10 && buffer[i + 2] === 13 && buffer[i + 3] === 10) { end = i; break; }
            }
            if(end < 0) return;
            const status_line = new TextDecoder().decode(buffer.subarray(0, end)).split("\r\n")[0];
            if(!/^HTTP\/1\.1 101\b/.test(status_line))
            {
                console.warn(`guest websocket: ${path}: ${status_line}`);
                finish(1006, "", false);
                return;
            }
            buffer = buffer.subarray(end + 4);
            open = true;
            clearTimeout(timer);
            on.open();
        }
        read_frames();
    });
    conn.on("shutdown", () => finish(1006, "", false));
    conn.on("close", () => finish(1006, "", false));

    return {
        send(data)
        {
            if(closed) return;
            if(typeof data === "string") send_frame(1, new TextEncoder().encode(data));
            else send_frame(2, ArrayBuffer.isView(data) ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength) : new Uint8Array(data));
        },
        close(code = 1000, reason = "")
        {
            if(closed || close_sent) return;
            close_sent = true;
            const text = new TextEncoder().encode(reason);
            const payload = new Uint8Array(2 + text.length);
            payload[0] = code >> 8;
            payload[1] = code & 255;
            payload.set(text, 2);
            send_frame(8, payload);
            // The server answers with its close frame; don't wait forever
            setTimeout(() => finish(code, reason, true), 2000);
        },
    };
}

// Served to the guest's pages (WEB_APPS[...].websocket) as the first script:
// WebSocket, for this site's own host, goes through the emulator page that
// opened the page (the iframe's parent, or the opener of a new tab).
const WEBSOCKET_SHIM = "__v86-websocket.js";
const WEBSOCKET_SHIM_SOURCE = `"use strict";
(function()
{
    const host = [window.parent, window.opener].find(w =>
    {
        try { return w && w !== window && typeof w.v86_guest_websocket === "function"; }
        catch(e) { return false; }
    });
    if(!host) return;
    const NativeWebSocket = window.WebSocket;
    const STATES = ["CONNECTING", "OPEN", "CLOSING", "CLOSED"];

    class GuestWebSocket extends EventTarget
    {
        constructor(url, protocols)
        {
            const u = new URL(url, location.href);
            if(u.host !== location.host) return new NativeWebSocket(url, protocols);
            super();
            this.url = u.href;
            this.readyState = 0;
            this.protocol = "";
            this.extensions = "";
            this.bufferedAmount = 0;
            this.binaryType = "blob";
            this.onopen = this.onmessage = this.onerror = this.onclose = null;
            this._socket = host.v86_guest_websocket(u.pathname + u.search, {
                open: () =>
                {
                    if(this.readyState !== 0) return;
                    this.readyState = 1;
                    this._fire(new Event("open"));
                },
                message: (data, binary) =>
                {
                    if(this.readyState !== 1) return;
                    if(binary)
                    {
                        // A copy made here, not in the emulator page's realm
                        const copy = new Uint8Array(data.length);
                        copy.set(data);
                        data = this.binaryType === "arraybuffer" ? copy.buffer : new Blob([copy]);
                    }
                    this._fire(new MessageEvent("message", { data, origin: location.origin }));
                },
                close: (code, reason, clean) =>
                {
                    if(this.readyState === 3) return;
                    this.readyState = 3;
                    if(!clean) this._fire(new Event("error"));
                    this._fire(new CloseEvent("close", { code, reason, wasClean: clean }));
                },
            });
        }
        _fire(event)
        {
            this.dispatchEvent(event);
            const handler = this["on" + event.type];
            if(typeof handler === "function") handler.call(this, event);
        }
        send(data)
        {
            if(this.readyState === 0) throw new DOMException("WebSocket is still connecting", "InvalidStateError");
            if(this.readyState === 1) this._socket.send(data);
        }
        close(code, reason)
        {
            if(this.readyState >= 2) return;
            this.readyState = 2;
            this._socket.close(code, reason);
        }
    }
    STATES.forEach((name, i) =>
    {
        GuestWebSocket[name] = i;
        GuestWebSocket.prototype[name] = i;
    });
    window.WebSocket = GuestWebSocket;
})();
`;

// Everything the guest sends on a connection to a port, as text
function guest_read_port(adapter, port)
{
    return new Promise((resolve, reject) =>
    {
        const chunks = [];
        let done = false;
        const conn = adapter.connect(port);
        const timer = setTimeout(() => finish(new Error(`no answer on the guest's port ${port}`)), GUEST_IDLE_TIMEOUT);
        function finish(error)
        {
            if(done) return;
            done = true;
            clearTimeout(timer);
            conn.close();
            if(error) reject(error);
            else resolve(new TextDecoder().decode(concat(chunks)));
        }
        conn.on("data", d => chunks.push(d.slice()));
        conn.on("shutdown", () => finish());
        conn.on("close", () => chunks.length ? finish() : finish(new Error(`the guest refused a connection on port ${port}`)));
    });
}

// kuna, the decompiler gdb-wui in the guest uses when told to (gdb-wui -kuna
// tcp:HOST:PORT; its internal/ghidra/kuna.go has the protocol): the guest
// connects to this page on the port, and the page runs kuna as WebAssembly in
// a worker (tools/kuna-web/build.sh put it in kuna/). Lines of JSON, one
// request at a time; an upload is followed by the binary's bytes.
function serve_kuna(adapter, port)
{
    let worker = null, next_id = 0;
    const pending = new Map();
    async function call(message, transfer)
    {
        if(!worker)
        {
            // Fetched on the first connection, not with the page: most visits
            // never open the decompiler, and kuna is megabytes.
            const manifest = await fetch("kuna/manifest.json", { cache: "no-cache" }).then(r =>
            {
                if(!r.ok) throw new Error("this site has no kuna (kuna/manifest.json: " + r.status + ")");
                return r.json();
            });
            worker = new Worker(`kuna/${manifest.dir}/kuna-bridge-worker.js`, { type: "module" });
            worker.onmessage = e =>
            {
                const p = pending.get(e.data.id);
                pending.delete(e.data.id);
                if(!p) return;
                if(e.data.error !== undefined) p.reject(new Error(e.data.error));
                else p.resolve(e.data.result);
            };
            worker.onerror = e =>
            {
                for(const p of pending.values()) p.reject(new Error("kuna's worker failed: " + (e.message || "error")));
                pending.clear();
                worker = null;
            };
        }
        const id = ++next_id;
        return new Promise((resolve, reject) =>
        {
            pending.set(id, { resolve, reject });
            worker.postMessage({ ...message, id }, transfer || []);
        });
    }

    adapter.bus.register("tcp-connection", conn =>
    {
        if(conn.sport !== port) return;
        let buffer = new Uint8Array(0);
        let upload = null;   // { request, size } while a binary's bytes arrive
        let queue = Promise.resolve();
        const reply = obj => conn.write(new TextEncoder().encode(JSON.stringify(obj) + "\n"));
        async function request(req, bytes)
        {
            try
            {
                switch(req.op)
                {
                    case "load":
                    {
                        const have = await call({ op: "has", sha256: req.sha256 });
                        const version = await call({ op: "version" }).then(v => v.kuna, () => "");
                        return reply({ ok: true, have, version });
                    }
                    case "upload":
                        await call({ op: "put", sha256: req.sha256, bytes: bytes.buffer }, [bytes.buffer]);
                        return reply({ ok: true });
                    case "run":
                        return reply({ ok: true, result: await call({ op: "run", sha256: req.sha256, args: req.args }) });
                    default:
                        return reply({ ok: false, error: "unknown op " + req.op });
                }
            }
            catch(e)
            {
                reply({ ok: false, error: e.message || String(e) });
            }
        }
        conn.on("data", data =>
        {
            buffer = concat([buffer, data.slice()]);
            for(;;)
            {
                if(upload)
                {
                    if(buffer.length < upload.size) return;
                    const bytes = buffer.slice(0, upload.size);
                    buffer = buffer.slice(upload.size);
                    const req = upload.request;
                    upload = null;
                    queue = queue.then(() => request(req, bytes));
                    continue;
                }
                const eol = buffer.indexOf(10);
                if(eol < 0) return;
                let req;
                try
                {
                    req = JSON.parse(new TextDecoder().decode(buffer.subarray(0, eol)));
                }
                catch(e)
                {
                    req = { op: "unparseable" };
                }
                buffer = buffer.slice(eol + 1);
                if(req.op === "upload") upload = { request: req, size: req.size | 0 };
                else queue = queue.then(() => request(req));
            }
        });
        conn.accept();
    });
}

// A microcontroller's GDB stub (src/rv64/src/gdb.rs, in another emulator on
// this page) for the guest's gdb, as a debug probe would offer it on a port:
// the guest connects to this page, its gateway, on `port` (S95gdb-wui does,
// with gdb-wui -gdb-command 'target remote ...'), and the bytes go between
// gdb and target's stub (V86.gdb_attach). One debugger at a time: a new
// connection takes over. on_change(true or false) says whether one is
// attached.
function serve_gdb_stub(adapter, port, target, on_change)
{
    let current = null;
    adapter.bus.register("tcp-connection", conn =>
    {
        if(conn.sport !== port) return;
        if(current)
        {
            const old = current;
            current = null;
            target.gdb_detach();
            old.close();
        }
        if(!target.gdb_attach(bytes => conn === current && conn.write(bytes))) return;
        current = conn;
        conn.on("data", data => { if(conn === current) target.gdb_input(data.slice()); });
        const gone = () =>
        {
            if(conn !== current) return;
            current = null;
            target.gdb_detach();
            on_change(false);
        };
        conn.on("shutdown", () => { gone(); conn.close(); });
        conn.on("close", gone);
        conn.accept();
        on_change(true);
    });
}

// options.gdb_stub: { port, target, on_change } for serve_gdb_stub
async function start_guest_web(emulator, build, options = {})
{
    const section = document.getElementById("web");
    const status = document.getElementById("web-status");
    const frame = document.getElementById("guest-web");
    const open_link = document.getElementById("web-open");
    const path = build.web;
    guest_web = WEB_APPS[build.web_app] || WEB_APPS.default;
    document.getElementById("web-title").textContent = guest_web.title;
    if(guest_web.height) frame.style.minHeight = guest_web.height;
    section.classList.toggle("wide", !!guest_web.wide);
    const note = document.getElementById("web-note");
    if(note)
    {
        note.textContent = guest_web.desktop_note || "";
        note.hidden = !guest_web.desktop_note;
    }
    section.hidden = false;

    if(!("serviceWorker" in navigator)) throw new Error("this browser has no service workers");
    // The network adapter is created once the emulator has loaded
    while(!emulator.network_adapter && !emulator.cpu_is_running)
    {
        await new Promise(resolve => setTimeout(resolve, 200));
    }
    const adapter = emulator.network_adapter;
    if(!adapter || typeof adapter.connect !== "function") throw new Error("needs the default network (no ?relay=)");

    if(guest_web.kuna_port) serve_kuna(adapter, guest_web.kuna_port);
    if(options.gdb_stub)
    {
        const { port, target, on_change } = options.gdb_stub;
        serve_gdb_stub(adapter, port, target, on_change || (() => {}));
    }
    if(guest_web.websocket)
    {
        window.v86_guest_websocket = (ws_path, on) => guest_websocket(adapter, ws_path, on);
        // A new tab finds this page as its opener
        open_link.rel = "opener";
    }

    status.textContent = "Starting …";
    // sw.js controls build.web's first directory (e.g. /cgi-bin/ or /gdb-wui/)
    const scope = new URL(path.split("/").slice(0, 2).join("/") + "/", location).pathname;
    const registration = await navigator.serviceWorker.register("sw.js", { scope });
    navigator.serviceWorker.onmessage = e =>
    {
        if(e.data && e.data.type === "v86-guest-http")
        {
            guest_http(adapter, e.data).then(reply => e.ports[0].postMessage(reply));
        }
    };
    const claim = () => registration.active && registration.active.postMessage({ type: "v86-guest-web-owner" });
    await active_worker(registration);
    claim();
    window.addEventListener("focus", claim);

    status.textContent = "Waiting for the guest's web server …";
    while(!await guest_port_open(adapter))
    {
        await new Promise(resolve => setTimeout(resolve, 5000));
    }

    // A single-use login link, when the interface needs one: its token, here.
    // (gdb-wui listens before its gdb has started, and hands out links after.)
    async function url()
    {
        if(!guest_web.login_port) return path;
        for(let attempt = 1; ; attempt++)
        {
            const link = await guest_read_port(adapter, guest_web.login_port).catch(() => "");
            const token = link.match(/[?&]t=([\w-]+)/);
            if(token) return path + "?t=" + token[1];
            if(attempt === 100) throw new Error("no login link from the guest");
            status.textContent = `Waiting for ${guest_web.title} to start …`;
            await new Promise(resolve => setTimeout(resolve, 3000));
        }
    }

    status.textContent = "Loading (the first page takes a while) …";
    frame.onload = () => { status.textContent = ""; };
    frame.src = await url();
    open_link.href = path;
    document.getElementById("web-reload").onclick = async () =>
    {
        status.textContent = "Loading …";
        try
        {
            frame.src = await url();
        }
        catch(e)
        {
            status.textContent = "Unavailable: " + e.message;
        }
    };
}
