#!/usr/bin/env node
// Boot an arm64 buildroot image (qemu_aarch64_virt_defconfig) on the aarch64
// machine through the V86 API, log in on the serial console, run commands,
// and power off.
//
//   A64_KERNEL=.../images/Image A64_ROOTFS=.../images/rootfs.ext4 tests/a64/boot.mjs
//
// With A64_NET=fetch, a virtio-net device backed by v86's fetch network
// adapter is added and the test checks that the guest gets a DHCP lease.
//
// Requires build/v86-a64.wasm (make build/v86-a64.wasm).

import url from "node:url";
import assert from "node:assert/strict";

const __dirname = url.fileURLToPath(new URL(".", import.meta.url));

process.on("unhandledRejection", exn => { throw exn; });

const TEST_RELEASE_BUILD = +process.env.TEST_RELEASE_BUILD;
const { V86 } = await import(TEST_RELEASE_BUILD ? "../../build/libv86.mjs" : "../../src/main.js");

const kernel = process.env.A64_KERNEL;
const rootfs = process.env.A64_ROOTFS;
if(!kernel || !rootfs)
{
    console.error("Set A64_KERNEL (arm64 Image) and A64_ROOTFS (ext4 image)");
    process.exit(2);
}

const TIMEOUT_MS = 10 * 60 * 1000;
const start = Date.now();

const emulator = new V86({
    arch: "aarch64",
    wasm_path: __dirname + "/../../build/v86-a64.wasm",
    kernel: { url: kernel },
    hda: { url: rootfs, async: false },
    memory_size: 128 * 1024 * 1024,
    cmdline: "rootwait root=/dev/vda console=ttyAMA0",
    autostart: true,
    disable_keyboard: true,
    disable_mouse: true,
    disable_speaker: true,
    log_level: 0,
    net_device: process.env.A64_NET ? { relay_url: process.env.A64_NET, type: "virtio" } : undefined,
});

const steps = [
    ...(process.env.A64_NET ? [{ wait: "udhcpc: lease of", send: "" }] : []),
    { wait: "login:", send: "root\n" },
    { wait: "# ", send: "uname -m; cat /proc/cpuinfo | head -3; echo marker-$((6*7))\n" },
    { wait: "marker-42", send: "" },
    { wait: "# ", send: "poweroff\n" },
    { wait: "reboot: Power down", send: "" },
];

let output = "";
let step = 0;

emulator.add_listener("serial0-output-byte", byte =>
{
    const c = String.fromCharCode(byte);
    process.stdout.write(c);
    output += c;
    while(step < steps.length && output.includes(steps[step].wait))
    {
        output = output.slice(output.indexOf(steps[step].wait) + steps[step].wait.length);
        if(steps[step].send)
        {
            emulator.serial0_send(steps[step].send);
        }
        step++;
    }
    if(step === steps.length)
    {
        const secs = (Date.now() - start) / 1000;
        console.error(`\n[a64] PASS: booted, logged in, ran commands and powered off in ${secs.toFixed(1)}s`);
        emulator.destroy();
        process.exit(0);
    }
});

setTimeout(() =>
{
    console.error(`\n[a64] FAIL: timed out at step ${step} (waiting for ${JSON.stringify(steps[step].wait)})`);
    assert.fail("timeout");
}, TIMEOUT_MS).unref();
