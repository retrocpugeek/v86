#!/usr/bin/env node
// Boot a riscv64 buildroot image (qemu_riscv64_virt_defconfig) on the
// riscv64 machine through the V86 API: OpenSBI, then Linux; log in on the
// serial console, run commands, and power off.
//
//   RV64_BIOS=.../images/fw_dynamic.bin RV64_KERNEL=.../images/Image \
//   RV64_ROOTFS=.../images/rootfs.squashfs tests/rv64/boot.mjs
//
// RV64_CMDLINE overrides the kernel command line. With RV64_NET=fetch, a
// virtio-net device backed by v86's fetch network adapter is added and the
// test checks that the guest gets a DHCP lease.
//
// Requires build/v86-rv64.wasm (make build/v86-rv64.wasm).

import url from "node:url";
import assert from "node:assert/strict";

const __dirname = url.fileURLToPath(new URL(".", import.meta.url));

process.on("unhandledRejection", exn => { throw exn; });

const TEST_RELEASE_BUILD = +process.env.TEST_RELEASE_BUILD;
const { V86 } = await import(TEST_RELEASE_BUILD ? "../../build/libv86.mjs" : "../../src/main.js");

const { RV64_BIOS: bios, RV64_KERNEL: kernel, RV64_ROOTFS: rootfs } = process.env;
if(!bios || !kernel || !rootfs)
{
    console.error("Set RV64_BIOS (OpenSBI fw_dynamic.bin), RV64_KERNEL (Image) and RV64_ROOTFS");
    process.exit(2);
}

const TIMEOUT_MS = 10 * 60 * 1000;
const start = Date.now();

const emulator = new V86({
    arch: "riscv64",
    wasm_path: __dirname + "/../../build/v86-rv64.wasm",
    bios: { url: bios },
    kernel: { url: kernel },
    hda: { url: rootfs, async: false },
    memory_size: 256 * 1024 * 1024,
    cmdline: process.env.RV64_CMDLINE || "rootwait root=/dev/vda ro rootfstype=squashfs console=ttyS0",
    autostart: true,
    disable_keyboard: true,
    disable_mouse: true,
    disable_speaker: true,
    log_level: 0,
    net_device: process.env.RV64_NET ? { relay_url: process.env.RV64_NET, type: "virtio" } : undefined,
});

const steps = [
    ...(process.env.RV64_NET ? [{ wait: "udhcpc: lease of", send: "" }] : []),
    { wait: "login:", send: "root\n" },
    { wait: "# ", send: "uname -m; md5sum /bin/busybox; echo marker-$((6*7))\n" },
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
        console.error(`\n[rv64] PASS: booted, logged in, ran commands and powered off in ${secs.toFixed(1)}s`);
        emulator.destroy();
        process.exit(0);
    }
});

setTimeout(() =>
{
    console.error(`\n[rv64] FAIL: timed out at step ${step} (waiting for ${JSON.stringify(steps[step].wait)})`);
    assert.fail("timeout");
}, TIMEOUT_MS).unref();
