// AArch64 machine for v86: QEMU-style "virt" board with a cortex-a53.
//
// The CPU, MMU, GIC and generic timer run in v86-a64.wasm (src/a64). The
// devices are JS: PL011 (serial0), PL031 RTC, and virtio-mmio slots for
// virtio-blk (hda), virtio-net, virtio-console and 9p. This object stands in
// for CPU in the v86 class: main_loop, init, reboot_internal, plus the guest
// memory accessors the virtio code uses.

import { dbg_log, dbg_assert } from "./log.js";
import { LOG_CPU } from "./const.js";
import { PL011 } from "./pl011.js";
import { PL031 } from "./pl031.js";
import { VirtioBlk } from "./virtio_blk.js";
import { VirtioNet } from "./virtio_net.js";
import { VirtioConsole } from "./virtio_console.js";
import { Virtio9p } from "../lib/9p.js";
import { FdtNode, fdt_build } from "./fdt.js";
import { A64Display } from "./a64_display.js";

// For Types Only
import { BusConnector } from "./bus.js";

const RAM_BASE = 0x40000000;
const GICD_BASE = 0x08000000;
const GICC_BASE = 0x08010000;
const PL011_BASE = 0x09000000;
const PL011_SPI = 1;
const PL031_BASE = 0x09010000;
const PL031_SPI = 2;
const VIRTIO_BASE = 0x0A000000;
const VIRTIO_SLOT_SIZE = 0x200;
const VIRTIO_FIRST_SPI = 16;
const VIRTIO_MAX_SLOTS = 32;
/** Must match NUM_SPI_LINES in src/a64/src/wasm.rs. */
const NUM_SPI_LINES = 64;

const DEFAULT_CMDLINE = "console=ttyAMA0 root=/dev/vda rw rootwait";

/** Instructions per call into wasm, and how long main_loop runs before yielding. */
const STEPS_PER_RUN = 100000;
const TIME_PER_SLICE = 10;

const now = typeof performance === "object" ? () => performance.now() : () => Date.now();

/**
 * @constructor
 * @param {BusConnector} bus
 * @param {?} wasm {exports, ...} from the instantiated v86-a64.wasm
 * @param {function()} stop_idling called when a device needs the CPU to wake up
 */
export function A64CPU(bus, wasm, stop_idling)
{
    this.bus = bus;
    this.wasm = wasm.exports;
    this.wasm_memory = this.wasm["memory"];
    this.stop_idling = stop_idling;

    /** Devices by role; virtio_net reads devices.net to pick its id. */
    this.devices = {};

    /** @type {!Array<{base: number, size: number, read: function(number, number):number, write: function(number, number, number)}>} */
    this.mmio = [];
    this.virtio_slots = 0;
    /** @type {!Array<!Object>} */
    this.virtio_devices = [];

    this.ram_ptr = 0;
    this.ram_size = 0;
    /** @type {Uint8Array} */
    this.mem8 = null;
    /** @type {Uint8Array} */
    this.irq_lines = null;

    this.powered_off = false;
    this.settings = null;

    // JIT (src/a64/src/jit.rs): compiled blocks go into the core's function table
    this.jit_table = this.wasm["__indirect_function_table"];
    this.jit_imports = { "m": this.wasm_memory, "r": this.wasm["a64_jit_rd"], "w": this.wasm["a64_jit_wr"], "t": this.jit_table };
    /** Bumped when the core is recreated: compilations for the old one are dropped. */
    this.jit_generation = 0;
    this.jit_errors = 0;
    /** @type {Uint8Array} pages with compiled code (see ram_written) */
    this.jit_code_pages = null;
}

/**
 * Called from wasm for device-space reads.
 * @param {number} addr
 * @param {number} size
 * @return {number}
 */
A64CPU.prototype.mmio_read = function(addr, size)
{
    for(const m of this.mmio)
    {
        if(addr >= m.base && addr < m.base + m.size)
        {
            return m.read(addr - m.base, size) >>> 0;
        }
    }
    dbg_log("a64: unmapped mmio read " + addr.toString(16), LOG_CPU);
    return 0;
};

/**
 * Called from wasm for device-space writes.
 * @param {number} addr
 * @param {number} size
 * @param {number} value
 */
A64CPU.prototype.mmio_write = function(addr, size, value)
{
    for(const m of this.mmio)
    {
        if(addr >= m.base && addr < m.base + m.size)
        {
            m.write(addr - m.base, size, value >>> 0);
            return;
        }
    }
    dbg_log("a64: unmapped mmio write " + addr.toString(16), LOG_CPU);
};

/**
 * Drive a GIC SPI line (as numbered in the device tree).
 * @param {number} spi
 * @param {boolean} level
 */
A64CPU.prototype.set_irq = function(spi, level)
{
    dbg_assert(spi < NUM_SPI_LINES);
    const lines = this.get_irq_lines();
    if(lines[spi] !== +level)
    {
        lines[spi] = +level;
        lines[NUM_SPI_LINES] = 1;
        if(level)
        {
            this.stop_idling();
        }
    }
};

/**
 * Called by VirtIO when it is created on this machine: assigns the next
 * virtio-mmio slot and returns its interrupt setter.
 * @param {!Object} virtio
 * @return {function(boolean)}
 */
A64CPU.prototype.virtio_mmio_register = function(virtio)
{
    dbg_assert(this.virtio_slots < VIRTIO_MAX_SLOTS);
    const slot = this.virtio_slots++;
    const spi = VIRTIO_FIRST_SPI + slot;
    this.mmio.push({
        base: VIRTIO_BASE + slot * VIRTIO_SLOT_SIZE,
        size: VIRTIO_SLOT_SIZE,
        read: (offset, size) => virtio.mmio_read(offset, size),
        write: (offset, size, value) => virtio.mmio_write(offset, size, value),
    });
    this.virtio_devices.push(virtio);
    return level => this.set_irq(spi, level);
};

// ---- guest memory, by guest physical address (used by the virtio code) ----

/** @return {!Uint8Array} */
A64CPU.prototype.ram = function()
{
    // Views detach when wasm memory grows, so re-create them as needed.
    if(!this.mem8 || this.mem8.buffer !== this.wasm_memory.buffer)
    {
        this.mem8 = new Uint8Array(this.wasm_memory.buffer, this.ram_ptr, this.ram_size);
    }
    return this.mem8;
};

/** @return {!Uint8Array} */
A64CPU.prototype.get_irq_lines = function()
{
    if(!this.irq_lines || this.irq_lines.buffer !== this.wasm_memory.buffer)
    {
        this.irq_lines = new Uint8Array(this.wasm_memory.buffer, this.wasm["a64_irq_lines_ptr"](), NUM_SPI_LINES + 1);
    }
    return this.irq_lines;
};

A64CPU.prototype.read8 = function(addr)
{
    return this.ram()[addr - RAM_BASE];
};

A64CPU.prototype.read16 = function(addr)
{
    const m = this.ram(), o = addr - RAM_BASE;
    return m[o] | m[o + 1] << 8;
};

A64CPU.prototype.read32s = function(addr)
{
    const m = this.ram(), o = addr - RAM_BASE;
    return m[o] | m[o + 1] << 8 | m[o + 2] << 16 | m[o + 3] << 24;
};

/**
 * Before a write to RAM offsets o..o + len: tell the JIT if a page with
 * compiled code is written (see Bus::code_pages in src/a64/src/bus.rs).
 * @param {number} o
 * @param {number} len
 */
A64CPU.prototype.ram_written = function(o, len)
{
    const pages = this.jit_code_pages;
    if(!pages) return;
    if(pages.buffer !== this.wasm_memory.buffer)
    {
        const code_pages = this.wasm["a64_jit_code_pages"]();
        this.jit_code_pages = new Uint8Array(this.wasm_memory.buffer, code_pages, this.ram_size >> 12);
        return this.ram_written(o, len);
    }
    for(let p = o >> 12; p <= (o + Math.max(len, 1) - 1) >> 12; p++)
    {
        if(pages[p])
        {
            this.wasm["a64_jit_written"](o, len);
            return;
        }
    }
};

A64CPU.prototype.write8 = function(addr, value)
{
    this.ram_written(addr - RAM_BASE, 1);
    this.ram()[addr - RAM_BASE] = value;
};

A64CPU.prototype.write16 = function(addr, value)
{
    const m = this.ram(), o = addr - RAM_BASE;
    this.ram_written(o, 2);
    m[o] = value;
    m[o + 1] = value >> 8;
};

A64CPU.prototype.write32 = function(addr, value)
{
    const m = this.ram(), o = addr - RAM_BASE;
    this.ram_written(o, 4);
    m[o] = value;
    m[o + 1] = value >> 8;
    m[o + 2] = value >> 16;
    m[o + 3] = value >> 24;
};

/**
 * @param {number} addr
 * @param {number} length
 * @return {!Uint8Array}
 */
A64CPU.prototype.read_blob = function(addr, length)
{
    const o = addr - RAM_BASE;
    return this.ram().subarray(o, o + length);
};

/**
 * @param {!Uint8Array} blob
 * @param {number} addr
 */
A64CPU.prototype.write_blob = function(blob, addr)
{
    this.ram_written(addr - RAM_BASE, blob.length);
    this.ram().set(blob, addr - RAM_BASE);
};

// ---- setup ----

/**
 * @param {!Object} settings from V86 (kernel, hda, cmdline, memory_size, ...)
 * @param {BusConnector} device_bus
 */
A64CPU.prototype.init = function(settings, device_bus)
{
    this.settings = settings;
    const ram_mb = Math.max(64, (settings.memory_size || 128 * 1024 * 1024) / (1024 * 1024) | 0);
    this.reset_core(ram_mb);

    const uart = new PL011(device_bus, level => this.set_irq(PL011_SPI, level), 0);
    this.mmio.push({ base: PL011_BASE, size: 0x1000, read: (o, s) => uart.read(o), write: (o, s, v) => uart.write(o, v) });
    this.devices.uart0 = uart;

    const rtc = new PL031(level => this.set_irq(PL031_SPI, level));
    this.mmio.push({ base: PL031_BASE, size: 0x1000, read: (o, s) => rtc.read(o), write: (o, s, v) => rtc.write(o, v) });
    this.devices.rtc = rtc;

    // The virtio devices are typed for the x86 CPU; this object provides the
    // parts they use (guest memory accessors, devices, virtio_mmio_register).
    const cpu = /** @type {?} */ (this);

    // Slot order = probe order, so the disk becomes vda.
    if(settings.hda)
    {
        this.devices.virtio_blk = new VirtioBlk(cpu, device_bus, settings.hda);
    }
    if(settings.network_enabled)
    {
        this.devices.virtio_net = new VirtioNet(cpu, device_bus, settings.preserve_mac_from_state_image);
        this.devices.net = this.devices.virtio_net;
    }
    if(settings.virtio_console)
    {
        this.devices.virtio_console = new VirtioConsole(cpu, device_bus);
    }
    if(settings.fs9p)
    {
        this.devices.virtio_9p = new Virtio9p(settings.fs9p, cpu, device_bus);
    }
    if(settings.a64_display)
    {
        // Framebuffer at the top of RAM; keyboard and tablet after the other
        // virtio devices (so they're /dev/input/event0 and event1)
        const addr = RAM_BASE + this.ram_size - A64Display.size(settings.a64_display);
        this.devices.display = new A64Display(cpu, settings.a64_display, addr);
    }

    this.boot();
};

/** (Re)create the wasm machine with `ram_mb` MB of RAM. */
A64CPU.prototype.reset_core = function(ram_mb)
{
    this.ram_size = ram_mb * 1024 * 1024;
    this.ram_ptr = this.wasm["a64_init"](ram_mb);
    this.jit_generation++;
    this.jit_code_pages = null;
    if(this.jit_table && !(this.settings && this.settings.a64_jit === false))
    {
        this.wasm["a64_jit_enable"](this.jit_table.length, 0);
        // (allocates, so the memory may grow: get the buffer after)
        const code_pages = this.wasm["a64_jit_code_pages"]();
        this.jit_code_pages = new Uint8Array(this.wasm_memory.buffer, code_pages, this.ram_size >> 12);
    }
    this.mem8 = null;
    this.irq_lines = null;
    this.powered_off = false;
};

A64CPU.prototype.boot = function()
{
    const settings = this.settings;
    dbg_assert(settings.kernel, "aarch64: a kernel (arm64 Image) is required");
    const kernel = new Uint8Array(settings.kernel);
    if(settings.a64_firmware)
    {
        this.boot_firmware(kernel);
        return;
    }
    const dtb_addr = this.wasm["a64_dtb_addr"]();

    let initrd = null;
    let initrd_addr = 0;
    if(settings.initrd)
    {
        initrd = new Uint8Array(settings.initrd);
        // Above the DTB (at the 64 MB mark) when RAM is large, else just below it.
        initrd_addr = this.ram_size >= 128 * 1024 * 1024 ?
            dtb_addr + 0x200000 :
            (dtb_addr - initrd.length) & ~0xFFF;
        dbg_assert(initrd_addr + initrd.length <= RAM_BASE + this.ram_size, "initrd doesn't fit in RAM");
        dbg_assert(initrd_addr >= RAM_BASE + 0x200000 + kernel.length, "initrd overlaps the kernel");
    }

    const dtb = settings.dtb ? new Uint8Array(settings.dtb) : this.build_dtb(initrd_addr, initrd ? initrd.length : 0);

    const kptr = this.wasm["a64_alloc"](kernel.length);
    const dptr = this.wasm["a64_alloc"](dtb.length);
    new Uint8Array(this.wasm_memory.buffer, kptr, kernel.length).set(kernel);
    new Uint8Array(this.wasm_memory.buffer, dptr, dtb.length).set(dtb);
    const result = this.wasm["a64_boot"](kptr, kernel.length, dptr, dtb.length);
    this.wasm["a64_free"](kptr, kernel.length);
    this.wasm["a64_free"](dptr, dtb.length);
    dbg_assert(result === 0, "aarch64: kernel is not an arm64 Image");

    if(initrd)
    {
        this.write_blob(initrd, initrd_addr);
    }
};

/**
 * Firmware boot (settings.a64_firmware): see a64_boot_firmware.
 * @param {!Uint8Array} firmware
 */
A64CPU.prototype.boot_firmware = function(firmware)
{
    const settings = this.settings;
    const dtb = settings.dtb ? new Uint8Array(settings.dtb) : this.build_dtb(0, 0);
    const fptr = this.wasm["a64_alloc"](firmware.length);
    const dptr = this.wasm["a64_alloc"](dtb.length);
    new Uint8Array(this.wasm_memory.buffer, fptr, firmware.length).set(firmware);
    new Uint8Array(this.wasm_memory.buffer, dptr, dtb.length).set(dtb);
    const result = this.wasm["a64_boot_firmware"](fptr, firmware.length, dptr, dtb.length);
    this.wasm["a64_free"](fptr, firmware.length);
    this.wasm["a64_free"](dptr, dtb.length);
    dbg_assert(result === 0, "aarch64: firmware doesn't fit in RAM");
};

/**
 * Device tree for the machine as configured (cf. tools/a64-ref/v86-virt.dts).
 * @param {number} initrd_addr
 * @param {number} initrd_size
 * @return {!Uint8Array}
 */
A64CPU.prototype.build_dtb = function(initrd_addr, initrd_size)
{
    const GIC_PHANDLE = 1;
    const CLK_PHANDLE = 2;
    const reg = (base, size) => [0, base, 0, size];

    const root = new FdtNode("");
    root.prop("#address-cells", [2])
        .prop("#size-cells", [2])
        .prop("compatible", "linux,dummy-virt")
        .prop("interrupt-parent", [GIC_PHANDLE]);

    root.child("psci")
        .prop("compatible", ["arm,psci-1.0", "arm,psci-0.2", "arm,psci"])
        .prop("method", "hvc");

    root.child("memory@" + RAM_BASE.toString(16))
        .prop("device_type", "memory")
        .prop("reg", reg(RAM_BASE, this.ram_size));

    const cpus = root.child("cpus").prop("#address-cells", [1]).prop("#size-cells", [0]);
    cpus.child("cpu@0")
        .prop("device_type", "cpu")
        .prop("compatible", "arm,cortex-a53")
        .prop("reg", [0]);

    // PPIs: secure phys, non-secure phys, virtual, hyp (flags: level-high, CPU 0).
    root.child("timer")
        .prop("compatible", "arm,armv8-timer")
        .prop("interrupts", [1, 13, 0x104, 1, 14, 0x104, 1, 11, 0x104, 1, 10, 0x104])
        .prop("always-on", true);

    root.child("intc@" + GICD_BASE.toString(16))
        .prop("compatible", "arm,cortex-a15-gic")
        .prop("reg", reg(GICD_BASE, 0x10000).concat(reg(GICC_BASE, 0x10000)))
        .prop("interrupt-controller", true)
        .prop("#interrupt-cells", [3])
        .prop("phandle", [GIC_PHANDLE]);

    root.child("apb-pclk")
        .prop("compatible", "fixed-clock")
        .prop("#clock-cells", [0])
        .prop("clock-frequency", [24000000])
        .prop("clock-output-names", "clk24mhz")
        .prop("phandle", [CLK_PHANDLE]);

    const uart_path = "/pl011@" + PL011_BASE.toString(16);
    root.child("pl011@" + PL011_BASE.toString(16))
        .prop("compatible", ["arm,pl011", "arm,primecell"])
        .prop("reg", reg(PL011_BASE, 0x1000))
        .prop("interrupts", [0, PL011_SPI, 4])
        .prop("clocks", [CLK_PHANDLE, CLK_PHANDLE])
        .prop("clock-names", ["uartclk", "apb_pclk"]);

    root.child("pl031@" + PL031_BASE.toString(16))
        .prop("compatible", ["arm,pl031", "arm,primecell"])
        .prop("reg", reg(PL031_BASE, 0x1000))
        .prop("interrupts", [0, PL031_SPI, 4])
        .prop("clocks", [CLK_PHANDLE])
        .prop("clock-names", "apb_pclk");

    for(let slot = 0; slot < this.virtio_slots; slot++)
    {
        const base = VIRTIO_BASE + slot * VIRTIO_SLOT_SIZE;
        root.child("virtio_mmio@" + base.toString(16))
            .prop("compatible", "virtio,mmio")
            .prop("reg", reg(base, VIRTIO_SLOT_SIZE))
            .prop("interrupts", [0, VIRTIO_FIRST_SPI + slot, 1])
            .prop("dma-coherent", true);
    }

    if(this.devices.display)
    {
        this.devices.display.add_dtb_nodes(root, reg);
    }

    const chosen = root.child("chosen")
        .prop("bootargs", this.settings.cmdline || DEFAULT_CMDLINE)
        .prop("stdout-path", uart_path);
    if(initrd_size)
    {
        chosen.prop("linux,initrd-start", [0, initrd_addr])
              .prop("linux,initrd-end", [0, initrd_addr + initrd_size]);
    }
    if(typeof crypto !== "undefined" && crypto.getRandomValues)
    {
        const seed = new Uint8Array(32);
        crypto.getRandomValues(seed);
        chosen.prop("rng-seed", seed);
    }

    root.child("aliases").prop("serial0", uart_path);

    return fdt_build(root);
};

// ---- execution (called by the v86 class) ----

/**
 * Run for a time slice. Returns how long to wait (ms) before the next call.
 * @return {number}
 */
A64CPU.prototype.main_loop = function()
{
    if(this.powered_off)
    {
        return 1000;
    }
    const start = now();
    for(;;)
    {
        this.wasm["a64_sync_time"](now());
        const exit = this.wasm["a64_run"](STEPS_PER_RUN);
        if(exit === 0)
        {
            if(now() - start >= TIME_PER_SLICE)
            {
                return 0;
            }
        }
        else if(exit === 1)
        {
            // Idle: sleep until the next timer (devices wake us via stop_idling).
            const ms = this.wasm["a64_ms_until_timer"]();
            return ms < 0 ? 100 : Math.min(ms, 100);
        }
        else if(exit === 2)
        {
            this.powered_off = true;
            this.bus.send("a64-power-off");
            return 1000;
        }
        else
        {
            this.reboot_internal();
            return 0;
        }
    }
};

A64CPU.prototype.reboot_internal = function()
{
    this.reset_core(this.ram_size / (1024 * 1024));
    for(const virtio of this.virtio_devices)
    {
        virtio.reset();
    }
    this.boot();
};

/**
 * Called from wasm: compile a JIT module and put its function "f" into the
 * function table at `index`, then report back through a64_jit_done.
 * @return {number} 0 = pending (see jit::CompileFn)
 */
A64CPU.prototype.jit_compile = function(ptr, len, index, cookie)
{
    const generation = this.jit_generation;
    const code = new Uint8Array(this.wasm_memory.buffer, ptr >>> 0, len >>> 0).slice();
    WebAssembly.instantiate(code, { "e": this.jit_imports }).then(({ instance }) => {
        if(generation !== this.jit_generation) return;
        const table = this.jit_table;
        if(table.length <= index)
        {
            table.grow(Math.max(index + 1 - table.length, 256));
        }
        table.set(index, instance.exports["f"]);
        this.wasm["a64_jit_done"](cookie, 1);
    }, err => {
        if(generation !== this.jit_generation) return;
        if(this.jit_errors++ < 5) console.error("a64 JIT: compilation failed:", err);
        this.wasm["a64_jit_done"](cookie, 0);
    });
    return 0;
};

/** @return {Array<number>} JIT counters: compiled, entered, stale, failed, instructions */
A64CPU.prototype.jit_stats = function()
{
    return [0, 1, 2, 3, 4].map(n => this.wasm["a64_jit_stat"](n));
};

/** @return {number} instructions executed since boot */
A64CPU.prototype.get_instruction_counter = function()
{
    return this.wasm["a64_icount"]();
};
