// Service worker that serves /cgi-bin/ from the guest's web server, so a
// guest's web interface (OpenWrt's LuCI) can be shown in an iframe next to
// the console.
//
// Its scope is /cgi-bin/, so it controls the iframe's documents, and with them
// every request they make (/luci-static/, /ubus, ...). Each request is passed
// to the emulator page (index.html, guest_http), which sends it to the guest's
// port 80 over v86's virtual network. The emulator page that registered last
// (it does so when focused) serves the requests; as the worker's state is lost
// when the browser stops it, otherwise the focused (or any) emulator page.

const REPLY_TIMEOUT = 150000;
let owner_id;

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", event => event.waitUntil(self.clients.claim()));

self.addEventListener("message", event =>
{
    if(event.data && event.data.type === "v86-guest-web-owner" && event.source)
    {
        owner_id = event.source.id;
    }
});

function error_page(status, message)
{
    return new Response(`<!doctype html><meta charset="utf-8"><title>${status}</title>` +
        `<body style="font: 14px system-ui, sans-serif; color: #ccc; background: #111">${message}`,
        { status, headers: { "content-type": "text/html; charset=utf-8" } });
}

async function find_owner()
{
    const scope = new URL(self.registration.scope).pathname;
    const pages = (await self.clients.matchAll({ type: "window", includeUncontrolled: true }))
        .filter(c => c.frameType === "top-level" && !new URL(c.url).pathname.startsWith(scope));
    return pages.find(c => c.id === owner_id) || pages.find(c => c.focused) || pages[0];
}

async function handle(request)
{
    const owner = await find_owner();
    if(!owner)
    {
        return error_page(503, "The emulator page isn't open. Open it (or reload it) and start the web interface from there.");
    }

    const url = new URL(request.url);
    const body = request.method === "GET" || request.method === "HEAD" ? null : await request.arrayBuffer();
    const reply = await new Promise(resolve =>
    {
        const channel = new MessageChannel();
        channel.port1.onmessage = e => resolve(e.data);
        setTimeout(() => resolve({ error: "The emulator page didn't answer." }), REPLY_TIMEOUT);
        owner.postMessage({
            type: "v86-guest-http",
            method: request.method,
            path: url.pathname + url.search,
            headers: [...request.headers],
            body,
        }, [channel.port2].concat(body ? [body] : []));
    });

    if(reply.error)
    {
        return error_page(502, reply.error);
    }
    const null_body = reply.status === 204 || reply.status === 205 || reply.status === 304 || request.method === "HEAD";
    try
    {
        return new Response(null_body ? null : reply.body, {
            status: reply.status,
            statusText: reply.status_text,
            headers: reply.headers,
        });
    }
    catch(e)
    {
        return error_page(502, "The guest's web server sent a response the browser can't use: " + e.message);
    }
}

self.addEventListener("fetch", event =>
{
    // Only same-origin requests go to the guest (the guest's pages may load
    // things from elsewhere)
    if(new URL(event.request.url).origin !== self.location.origin) return;
    event.respondWith(handle(event.request));
});
