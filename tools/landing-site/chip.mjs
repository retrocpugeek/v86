// Generates the chip illustrations (arm64.svg, riscv.svg, x86.svg):
//   node tools/landing-site/chip.mjs
// A package with pins on all four sides, a die with circuit traces, and a
// label. Run again after changing it; the SVGs are committed.
import fs from "node:fs";

function chip({ name, sub, die, die2, pin, trace, text })
{
    const size = 240, body = 160, off = (size - body) / 2;
    const pins = [];
    const n = 12, step = body / n;
    for(let i = 0; i < n; i++)
    {
        const p = off + step * (i + 0.5) - 3;
        pins.push(`<rect x="${p}" y="${off - 22}" width="6" height="22" rx="1.5"/>`);
        pins.push(`<rect x="${p}" y="${off + body}" width="6" height="22" rx="1.5"/>`);
        pins.push(`<rect x="${off - 22}" y="${p}" width="22" height="6" rx="1.5"/>`);
        pins.push(`<rect x="${off + body}" y="${p}" width="22" height="6" rx="1.5"/>`);
    }
    // Circuit traces from the die to the package edge
    const traces = [];
    const d0 = off + 28, d1 = off + body - 28;
    for(let i = 0; i < 5; i++)
    {
        const t = d0 + 8 + i * ((d1 - d0 - 16) / 4);
        traces.push(`M${t} ${d0} V${off + 10 + i * 2} H${t + (i - 2) * 6}`);
        traces.push(`M${t} ${d1} V${off + body - 10 - i * 2} H${t - (i - 2) * 6}`);
        traces.push(`M${d0} ${t} H${off + 10 + i * 2}`);
        traces.push(`M${d1} ${t} H${off + body - 10 - i * 2}`);
    }
    return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${size} ${size}" role="img" aria-label="${name} processor illustration">
  <defs>
    <linearGradient id="die" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="${die}"/>
      <stop offset="1" stop-color="${die2}"/>
    </linearGradient>
  </defs>
  <g fill="${pin}">${pins.join("")}</g>
  <rect x="${off}" y="${off}" width="${body}" height="${body}" rx="10" fill="#1b1f27" stroke="#3a4150" stroke-width="2"/>
  <circle cx="${off + 12}" cy="${off + 12}" r="4" fill="#3a4150"/>
  <path d="${traces.join(" ")}" fill="none" stroke="${trace}" stroke-width="2" stroke-linecap="round" opacity="0.8"/>
  <rect x="${d0}" y="${d0}" width="${d1 - d0}" height="${d1 - d0}" rx="6" fill="url(#die)"/>
  <text x="${size / 2}" y="${size / 2 + 2}" text-anchor="middle" font-family="system-ui, sans-serif" font-size="${name.length > 6 ? 19 : 24}" font-weight="700" fill="${text}">${name}</text>
  <text x="${size / 2}" y="${size / 2 + 22}" text-anchor="middle" font-family="ui-monospace, monospace" font-size="10.5" fill="${text}" opacity="0.85">${sub}</text>
</svg>
`;
}

const dir = new URL("public/", import.meta.url);
fs.writeFileSync(new URL("arm64.svg", dir), chip({
    name: "AArch64", sub: "ARMv8-A · 64-bit",
    die: "#0b7a8a", die2: "#0a4d6e", pin: "#b9c2cc", trace: "#38c6d6", text: "#ffffff",
}));
fs.writeFileSync(new URL("riscv.svg", dir), chip({
    name: "RISC-V", sub: "RV64GC · RV32GC",
    die: "#283a8c", die2: "#1a2559", pin: "#d8b65a", trace: "#f2c94c", text: "#ffffff",
}));
fs.writeFileSync(new URL("x86.svg", dir), chip({
    name: "x86", sub: "32-bit x86 · PC",
    die: "#5a6170", die2: "#343a46", pin: "#b9c2cc", trace: "#9aa3b2", text: "#ffffff",
}));
