// HTTP proxy for the guest's network (v86's "fetch" adapter, cors_proxy option).
//
// The page is served over https, so the browser blocks the adapter's fetches of
// guest http:// URLs (mixed content), and other sites mostly don't allow CORS.
// /proxy?url=<url> fetches the URL from the Worker instead; it's same-origin
// for the page, so neither applies. Everything else is a static asset.
//
// Only the page's own fetch() calls are served (Sec-Fetch-Site: same-origin,
// not a navigation), so other sites can't use it and proxied content is never
// rendered as a page of this origin. Free-tier limits (100k requests/day) cap
// the cost of misuse.

const MAX_URL_LENGTH = 8192;

// Headers that must not be forwarded upstream: they belong to the page's
// request to this Worker, not to the guest's request.
const DROP_REQUEST_HEADERS = ["cookie", "host", "origin", "referer", "cf-connecting-ip", "x-forwarded-for", "x-real-ip"];

function error(status, message)
{
    return new Response(message + "\n", { status, headers: { "content-type": "text/plain", "cache-control": "no-store" } });
}

function is_private_host(hostname)
{
    if(hostname === "localhost" || hostname.endsWith(".localhost") || hostname.endsWith(".local") || hostname.endsWith(".internal")) return true;
    const m = hostname.match(/^(\d+)\.(\d+)\.(\d+)\.(\d+)$/);
    if(!m) return hostname.startsWith("[");  // IPv6 literals: not needed by the guest
    const [a, b] = [+m[1], +m[2]];
    return a === 0 || a === 10 || a === 127 || a >= 224 || (a === 169 && b === 254) ||
        (a === 172 && b >= 16 && b < 32) || (a === 192 && b === 168) || (a === 100 && b >= 64 && b < 128);
}

async function proxy(request)
{
    const fetch_site = request.headers.get("sec-fetch-site");
    if(fetch_site !== "same-origin" || request.headers.get("sec-fetch-mode") === "navigate")
    {
        return error(403, "This proxy only serves the emulator page.");
    }

    const target = new URL(request.url).searchParams.get("url");
    if(!target || target.length > MAX_URL_LENGTH) return error(400, "Missing or overlong url parameter.");
    let url;
    try { url = new URL(target); } catch(e) { return error(400, "Invalid url."); }
    if(url.protocol !== "http:" && url.protocol !== "https:") return error(400, "Only http and https are proxied.");
    if(url.port && url.port !== "80" && url.port !== "443") return error(400, "Only ports 80 and 443 are proxied.");
    if(is_private_host(url.hostname)) return error(400, "Private addresses are not proxied.");

    const headers = new Headers(request.headers);
    for(const name of [...headers.keys()])
    {
        if(DROP_REQUEST_HEADERS.includes(name) || name.startsWith("sec-") || name.startsWith("cf-")) headers.delete(name);
    }

    let upstream;
    try
    {
        upstream = await fetch(url, {
            method: request.method,
            headers,
            body: request.method === "GET" || request.method === "HEAD" ? null : request.body,
            // Follow redirects here: the guest can't follow one to https://
            // (the adapter only carries plain HTTP).
            redirect: "follow",
        });
    }
    catch(e)
    {
        return error(502, `Fetching ${url.href} failed: ${e.message}`);
    }

    const out = new Headers(upstream.headers);
    out.delete("set-cookie");
    out.set("cache-control", "no-store");
    // Never let a proxied response act as a document of this origin
    out.set("content-security-policy", "sandbox");
    out.set("x-content-type-options", "nosniff");
    return new Response(upstream.body, { status: upstream.status, statusText: upstream.statusText, headers: out });
}

export default {
    async fetch(request, env)
    {
        const path = new URL(request.url).pathname;
        if(path === "/proxy")
        {
            return proxy(request);
        }
        return env.ASSETS.fetch(request);
    },
};
