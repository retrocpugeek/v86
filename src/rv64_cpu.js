// RISC-V machine for v86: QEMU-style riscv64 (or, for arch "riscv32",
// riscv32) "virt" board with one hart. For arch "ch32v003" or "ch32v307",
// a WCH CH32V microcontroller instead: `bios` is its flash image, USART1 the
// serial console (src/rv64/src/dev/ch32.rs).
//
// The hart, MMU, CLINT, PLIC, 16550 UART and goldfish RTC run in
// v86-rv64.wasm (src/rv64). The virtio-mmio slots are JS: virtio-blk (hda),
// virtio-net, virtio-console and 9p. It boots OpenSBI (`bios`, fw_dynamic)
// and a Linux `kernel` like QEMU's -bios/-kernel. This object stands in for
// CPU in the v86 class: main_loop, init, reboot_internal, plus the guest
// memory accessors the virtio code uses.

import { dbg_log, dbg_assert } from "./log.js";
import { LOG_CPU } from "./const.js";
import { VirtioBlk } from "./virtio_blk.js";
import { VirtioNet } from "./virtio_net.js";
import { VirtioConsole } from "./virtio_console.js";
import { Virtio9p } from "../lib/9p.js";
import { FdtNode, fdt_build } from "./fdt.js";

// For Types Only
import { BusConnector } from "./bus.js";

const RAM_BASE = 0x80000000;
const CH32_SRAM_BASE = 0x20000000;
const UART_BASE = 0x10000000;
const UART_IRQ = 10;
const RTC_BASE = 0x101000;
const RTC_IRQ = 11;
const TEST_BASE = 0x100000;
const CLINT_BASE = 0x2000000;
const PLIC_BASE = 0xc000000;
/** Must match src/rv64/src/wasm.rs. */
const VIRTIO_BASE = 0x10001000;
const VIRTIO_SLOT_SIZE = 0x1000;
const VIRTIO_MAX_SLOTS = 8;
const NUM_IRQ_LINES = 64;

const DEFAULT_CMDLINE = "console=ttyS0 root=/dev/vda rw rootwait";

/** Instructions per call into wasm, and how long main_loop runs before yielding. */
const STEPS_PER_RUN = 100000;
const TIME_PER_SLICE = 10;

const now = typeof performance === "object" ? () => performance.now() : () => Date.now();

/**
 * @constructor
 * @param {BusConnector} bus
 * @param {?} wasm {exports, arch, ...} from the instantiated v86-rv64.wasm
 * @param {function()} stop_idling called when a device needs the CPU to wake up
 */
export function RV64CPU(bus, wasm, stop_idling)
{
    this.bus = bus;
    this.wasm = wasm.exports;
    this.wasm_memory = this.wasm["memory"];
    /** RV32 (arch "riscv32"): see Cpu::xlen32 in src/rv64/src/cpu.rs */
    this.xlen32 = wasm.arch === "riscv32";
    /** A CH32V microcontroller (arch "ch32v003" or "ch32v307") rather than virt */
    this.ch32 = wasm.arch === "ch32v003" || wasm.arch === "ch32v307";
    this.ch32_chip = wasm.arch === "ch32v307" ? 1 : 0;
    /** Guest-physical address of RAM (the CH32V003's SRAM: 0x20000000) */
    this.ram_base = this.ch32 ? CH32_SRAM_BASE : RAM_BASE;
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
    /** @type {?function(!Uint8Array)} where the GDB stub's replies go while a debugger is attached */
    this.gdb_output = null;

    // JIT (src/rv64/src/jit.rs): compiled regions go into the core's function table
    this.jit_table = this.wasm["__indirect_function_table"];
    this.jit_imports = { "m": this.wasm_memory, "r": this.wasm["rv64_jit_rd"], "w": this.wasm["rv64_jit_wr"], "t": this.jit_table };
    /** Bumped when the core is recreated: compilations for the old one are dropped. */
    this.jit_generation = 0;
    this.jit_errors = 0;
    /** @type {Uint8Array} pages with compiled code (see ram_written) */
    this.jit_code_pages = null;

    bus.register("serial0-input", byte => {
        this.wasm["rv64_serial_in"](byte);
        this.stop_idling();
    }, this);
}

/**
 * Called from wasm for a byte from the UART.
 * @param {number} byte
 */
RV64CPU.prototype.serial_out = function(byte)
{
    this.bus.send("serial0-output-byte", byte & 0xFF);
};

/**
 * Called from wasm for virtio-mmio reads.
 * @param {number} addr
 * @param {number} size
 * @return {number}
 */
RV64CPU.prototype.mmio_read = function(addr, size)
{
    for(const m of this.mmio)
    {
        if(addr >= m.base && addr < m.base + m.size)
        {
            return m.read(addr - m.base, size) >>> 0;
        }
    }
    dbg_log("rv64: unmapped mmio read " + addr.toString(16), LOG_CPU);
    return 0;
};

/**
 * Called from wasm for virtio-mmio writes.
 * @param {number} addr
 * @param {number} size
 * @param {number} value
 */
RV64CPU.prototype.mmio_write = function(addr, size, value)
{
    for(const m of this.mmio)
    {
        if(addr >= m.base && addr < m.base + m.size)
        {
            m.write(addr - m.base, size, value >>> 0);
            return;
        }
    }
    dbg_log("rv64: unmapped mmio write " + addr.toString(16), LOG_CPU);
};

/**
 * Drive a PLIC source's line.
 * @param {number} irq
 * @param {boolean} level
 */
RV64CPU.prototype.set_irq = function(irq, level)
{
    dbg_assert(irq > 0 && irq < NUM_IRQ_LINES);
    const lines = this.get_irq_lines();
    if(lines[irq] !== +level)
    {
        lines[irq] = +level;
        lines[NUM_IRQ_LINES] = 1;
        if(level)
        {
            this.stop_idling();
        }
    }
};

/**
 * Called by VirtIO when it's created on this machine: assigns the next
 * virtio-mmio slot (PLIC source 1, 2, ...) and returns its interrupt setter.
 * @param {!Object} virtio
 * @return {function(boolean)}
 */
RV64CPU.prototype.virtio_mmio_register = function(virtio)
{
    dbg_assert(this.virtio_slots < VIRTIO_MAX_SLOTS);
    const slot = this.virtio_slots++;
    this.mmio.push({
        base: VIRTIO_BASE + slot * VIRTIO_SLOT_SIZE,
        size: VIRTIO_SLOT_SIZE,
        read: (offset, size) => virtio.mmio_read(offset, size),
        write: (offset, size, value) => virtio.mmio_write(offset, size, value),
    });
    this.virtio_devices.push(virtio);
    return level => this.set_irq(1 + slot, level);
};

// ---- guest memory, by guest physical address (used by the virtio code) ----

/** @return {!Uint8Array} */
RV64CPU.prototype.ram = function()
{
    // Views detach when wasm memory grows, so re-create them as needed.
    if(!this.mem8 || this.mem8.buffer !== this.wasm_memory.buffer)
    {
        this.mem8 = new Uint8Array(this.wasm_memory.buffer, this.ram_ptr, this.ram_size);
    }
    return this.mem8;
};

/** @return {!Uint8Array} */
RV64CPU.prototype.get_irq_lines = function()
{
    if(!this.irq_lines || this.irq_lines.buffer !== this.wasm_memory.buffer)
    {
        this.irq_lines = new Uint8Array(this.wasm_memory.buffer, this.wasm["rv64_irq_lines_ptr"](), NUM_IRQ_LINES + 1);
    }
    return this.irq_lines;
};

// Guest-physical accessors for v86's devices. RAM starts at 0x8000_0000, so
// addresses that went through read32s/| 0 arrive negative: >>> 0 them.
RV64CPU.prototype.read8 = function(addr)
{
    return this.ram()[(addr >>> 0) - this.ram_base];
};

RV64CPU.prototype.read16 = function(addr)
{
    const m = this.ram(), o = (addr >>> 0) - this.ram_base;
    return m[o] | m[o + 1] << 8;
};

RV64CPU.prototype.read32s = function(addr)
{
    const m = this.ram(), o = (addr >>> 0) - this.ram_base;
    return m[o] | m[o + 1] << 8 | m[o + 2] << 16 | m[o + 3] << 24;
};

/**
 * Before a write to RAM offsets o..o + len: tell the JIT if a page with
 * compiled code is written (see Bus::code_pages in src/rv64/src/bus.rs).
 * @param {number} o
 * @param {number} len
 */
RV64CPU.prototype.ram_written = function(o, len)
{
    const pages = this.jit_code_pages;
    if(!pages) return;
    if(pages.buffer !== this.wasm_memory.buffer)
    {
        const code_pages = this.wasm["rv64_jit_code_pages"]();
        this.jit_code_pages = new Uint8Array(this.wasm_memory.buffer, code_pages, this.ram_size >> 12);
        return this.ram_written(o, len);
    }
    for(let p = o >> 12; p <= (o + Math.max(len, 1) - 1) >> 12; p++)
    {
        if(pages[p])
        {
            this.wasm["rv64_jit_written"](o, len);
            return;
        }
    }
};

RV64CPU.prototype.write8 = function(addr, value)
{
    const o = (addr >>> 0) - this.ram_base;
    this.ram_written(o, 1);
    this.ram()[o] = value;
};

RV64CPU.prototype.write16 = function(addr, value)
{
    const m = this.ram(), o = (addr >>> 0) - this.ram_base;
    this.ram_written(o, 2);
    m[o] = value;
    m[o + 1] = value >> 8;
};

RV64CPU.prototype.write32 = function(addr, value)
{
    const m = this.ram(), o = (addr >>> 0) - this.ram_base;
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
RV64CPU.prototype.read_blob = function(addr, length)
{
    const o = (addr >>> 0) - this.ram_base;
    return this.ram().subarray(o, o + length);
};

/**
 * @param {!Uint8Array} blob
 * @param {number} addr
 */
RV64CPU.prototype.write_blob = function(blob, addr)
{
    const o = (addr >>> 0) - this.ram_base;
    this.ram_written(o, blob.length);
    this.ram().set(blob, o);
};

// ---- setup ----

/**
 * @param {!Object} settings from V86 (bios, kernel, hda, cmdline, memory_size, ...)
 * @param {BusConnector} device_bus
 */
RV64CPU.prototype.init = function(settings, device_bus)
{
    this.settings = settings;
    const ram_mb = Math.max(64, (settings.memory_size || 128 * 1024 * 1024) / (1024 * 1024) | 0);
    this.reset_core(ram_mb);
    if(this.ch32)
    {
        this.boot();
        return;
    }

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

    this.boot();
};

/** (Re)create the wasm machine with `ram_mb` MB of RAM. */
RV64CPU.prototype.reset_core = function(ram_mb)
{
    if(this.ch32)
    {
        // boot() creates the machine with its firmware
        this.ram_size = this.ch32_chip === 1 ? 128 * 1024 : 2048;
        this.powered_off = false;
        return;
    }
    this.ram_size = ram_mb * 1024 * 1024;
    this.ram_ptr = this.wasm["rv64_init"](ram_mb, Date.now() / 1000);
    this.wasm["rv64_set_xlen32"](+this.xlen32);
    this.mem8 = null;
    this.irq_lines = null;
    this.powered_off = false;
    this.jit_generation++;
    this.jit_code_pages = null;
    if(this.jit_table && !(this.settings && this.settings.rv64_jit === false))
    {
        this.wasm["rv64_jit_enable"](this.jit_table.length, 0);
        const code_pages = this.wasm["rv64_jit_code_pages"]();
        this.jit_code_pages = new Uint8Array(this.wasm_memory.buffer, code_pages, this.ram_size >> 12);
    }
};

RV64CPU.prototype.boot = function()
{
    const settings = this.settings;
    if(this.ch32)
    {
        dbg_assert(settings.bios, "ch32v: a flash image (bios) is required");
        const flash = new Uint8Array(settings.bios);
        const ptr = this.wasm["rv64_alloc"](flash.length);
        new Uint8Array(this.wasm_memory.buffer, ptr, flash.length).set(flash);
        const result = this.wasm["rv64_init_ch32"](this.ch32_chip, ptr, flash.length, Date.now() / 1000);
        this.wasm["rv64_free"](ptr, flash.length);
        dbg_assert(result === 0, "ch32v: the flash image doesn't fit in the flash");
        this.ram_ptr = this.wasm["rv64_ram_ptr"]();
        this.mem8 = null;
        return;
    }
    dbg_assert(settings.bios, "riscv64: firmware (bios, OpenSBI fw_dynamic) is required");
    dbg_assert(settings.kernel, "riscv64: a kernel (Image) is required");
    const firmware = new Uint8Array(settings.bios);
    const kernel = new Uint8Array(settings.kernel);
    const dtb = settings.dtb ? new Uint8Array(settings.dtb) : this.build_dtb();

    const fptr = this.wasm["rv64_alloc"](firmware.length);
    const kptr = this.wasm["rv64_alloc"](kernel.length);
    const dptr = this.wasm["rv64_alloc"](dtb.length);
    new Uint8Array(this.wasm_memory.buffer, fptr, firmware.length).set(firmware);
    new Uint8Array(this.wasm_memory.buffer, kptr, kernel.length).set(kernel);
    new Uint8Array(this.wasm_memory.buffer, dptr, dtb.length).set(dtb);
    const result = this.wasm["rv64_boot"](fptr, firmware.length, kptr, kernel.length, dptr, dtb.length);
    this.wasm["rv64_free"](fptr, firmware.length);
    this.wasm["rv64_free"](kptr, kernel.length);
    this.wasm["rv64_free"](dptr, dtb.length);
    dbg_assert(result === 0, "riscv64: firmware, kernel and DTB don't fit in RAM");
};

/**
 * Device tree for the machine as configured (cf. tools/rv64-ref/virt.dts).
 * @return {!Uint8Array}
 */
RV64CPU.prototype.build_dtb = function()
{
    const CPU_INTC = 1;
    const PLIC = 2;
    const TEST = 3;
    const CPU0 = 4;
    const reg = (base, size) => [0, base, 0, size];

    const root = new FdtNode("");
    root.prop("#address-cells", [2])
        .prop("#size-cells", [2])
        .prop("compatible", "riscv-virtio")
        .prop("model", "riscv-virtio,qemu");

    const uart_path = "/soc/serial@" + UART_BASE.toString(16);
    const chosen = root.child("chosen")
        .prop("bootargs", this.settings.cmdline || DEFAULT_CMDLINE)
        .prop("stdout-path", uart_path);
    // Seeds the kernel's random pool: without it (no RNG device here, and a
    // read-only root can't keep a seed), getrandom() can block for minutes
    if(typeof crypto !== "undefined" && crypto.getRandomValues)
    {
        const seed = new Uint8Array(32);
        crypto.getRandomValues(seed);
        chosen.prop("rng-seed", seed);
    }

    root.child("memory@" + RAM_BASE.toString(16))
        .prop("device_type", "memory")
        .prop("reg", reg(RAM_BASE, this.ram_size));

    const cpus = root.child("cpus")
        .prop("#address-cells", [1])
        .prop("#size-cells", [0])
        .prop("timebase-frequency", [10000000]);
    const cpu0 = cpus.child("cpu@0")
        .prop("device_type", "cpu")
        .prop("reg", [0])
        .prop("status", "okay")
        .prop("compatible", "riscv")
        .prop("riscv,isa", (this.xlen32 ? "rv32" : "rv64") + "imafdc_zicsr_zifencei")
        .prop("riscv,isa-base", this.xlen32 ? "rv32i" : "rv64i")
        .prop("riscv,isa-extensions", ["i", "m", "a", "f", "d", "c", "zicsr", "zifencei"])
        .prop("mmu-type", this.xlen32 ? "riscv,sv32" : "riscv,sv39")
        .prop("phandle", [CPU0]);
    cpu0.child("interrupt-controller")
        .prop("#interrupt-cells", [1])
        .prop("interrupt-controller", true)
        .prop("compatible", "riscv,cpu-intc")
        .prop("phandle", [CPU_INTC]);
    cpus.child("cpu-map").child("cluster0").child("core0").prop("cpu", [CPU0]);

    root.child("poweroff")
        .prop("compatible", "syscon-poweroff")
        .prop("regmap", [TEST])
        .prop("offset", [0])
        .prop("value", [0x5555]);
    root.child("reboot")
        .prop("compatible", "syscon-reboot")
        .prop("regmap", [TEST])
        .prop("offset", [0])
        .prop("value", [0x7777]);

    const soc = root.child("soc")
        .prop("#address-cells", [2])
        .prop("#size-cells", [2])
        .prop("compatible", "simple-bus")
        .prop("ranges", true);
    soc.child("test@" + TEST_BASE.toString(16))
        .prop("compatible", ["sifive,test1", "sifive,test0", "syscon"])
        .prop("reg", reg(TEST_BASE, 0x1000))
        .prop("phandle", [TEST]);
    soc.child("rtc@" + RTC_BASE.toString(16))
        .prop("compatible", "google,goldfish-rtc")
        .prop("reg", reg(RTC_BASE, 0x1000))
        .prop("interrupts", [RTC_IRQ])
        .prop("interrupt-parent", [PLIC]);
    soc.child("clint@" + CLINT_BASE.toString(16))
        .prop("compatible", ["sifive,clint0", "riscv,clint0"])
        .prop("reg", reg(CLINT_BASE, 0x10000))
        .prop("interrupts-extended", [CPU_INTC, 3, CPU_INTC, 7]);
    soc.child("plic@" + PLIC_BASE.toString(16))
        .prop("compatible", ["sifive,plic-1.0.0", "riscv,plic0"])
        .prop("reg", reg(PLIC_BASE, 0x600000))
        .prop("#address-cells", [0])
        .prop("#interrupt-cells", [1])
        .prop("interrupt-controller", true)
        .prop("riscv,ndev", [0x5f])
        .prop("interrupts-extended", [CPU_INTC, 11, CPU_INTC, 9])
        .prop("phandle", [PLIC]);
    soc.child("serial@" + UART_BASE.toString(16))
        .prop("compatible", "ns16550a")
        .prop("reg", reg(UART_BASE, 0x100))
        .prop("interrupts", [UART_IRQ])
        .prop("interrupt-parent", [PLIC])
        .prop("clock-frequency", [0x384000]);
    for(let slot = 0; slot < this.virtio_slots; slot++)
    {
        const base = VIRTIO_BASE + slot * VIRTIO_SLOT_SIZE;
        soc.child("virtio_mmio@" + base.toString(16))
            .prop("compatible", "virtio,mmio")
            .prop("reg", reg(base, VIRTIO_SLOT_SIZE))
            .prop("interrupts", [1 + slot])
            .prop("interrupt-parent", [PLIC]);
    }

    return fdt_build(root);
};

// ---- execution (called by the v86 class) ----

/**
 * Run for a time slice. Returns how long to wait (ms) before the next call.
 * @return {number}
 */
RV64CPU.prototype.main_loop = function()
{
    if(this.powered_off)
    {
        return 1000;
    }
    if(this.gdb_output && this.wasm["rv64_gdb_halted"]())
    {
        // Stopped by the debugger, whose next packet wakes us
        return 1000;
    }
    const start = now();
    for(;;)
    {
        this.wasm["rv64_sync_time"](now());
        const exit = this.wasm["rv64_run"](STEPS_PER_RUN);
        if(exit === 0)
        {
            if(this.ch32)
            {
                // A microcontroller runs at its own clock's speed: a firmware
                // that busy-waits mustn't get ahead of real time
                const ahead = this.wasm["rv64_ms_ahead"](now());
                if(ahead > 1)
                {
                    return Math.min(ahead, 100);
                }
            }
            if(now() - start >= TIME_PER_SLICE)
            {
                return 0;
            }
        }
        else if(exit === 1)
        {
            // Idle: sleep until the next timer (devices wake us via stop_idling).
            const ms = this.wasm["rv64_ms_until_timer"]();
            return ms < 0 ? 100 : Math.min(ms, 100);
        }
        else if(exit === 4)
        {
            // Stopped for the debugger: tell it
            this.gdb_flush();
            return 1000;
        }
        else if(exit === 2)
        {
            this.powered_off = true;
            this.bus.send("rv64-power-off");
            return 1000;
        }
        else
        {
            this.reboot_internal();
            return 0;
        }
    }
};

RV64CPU.prototype.reboot_internal = function()
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
 * function table at `index`, then report back through rv64_jit_done.
 * @return {number} 0 = pending (see jit::CompileFn)
 */
RV64CPU.prototype.jit_compile = function(ptr, len, index, cookie)
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
        this.wasm["rv64_jit_done"](cookie, 1);
    }, err => {
        if(generation !== this.jit_generation) return;
        if(this.jit_errors++ < 5) console.error("rv64 JIT: compilation failed:", err);
        this.wasm["rv64_jit_done"](cookie, 0);
    });
    return 0;
};

// ---- the GDB stub (src/rv64/src/gdb.rs), for a microcontroller ----

/**
 * A debugger connects: the hart stops, and the stub's replies go to
 * `output`. Returns false if this isn't a microcontroller.
 * @param {function(!Uint8Array)} output
 * @return {boolean}
 */
RV64CPU.prototype.gdb_attach = function(output)
{
    if(!this.ch32 || this.wasm["rv64_gdb_attach"]() !== 0) return false;
    this.gdb_output = output;
    return true;
};

/** The debugger went away: the hart runs on. */
RV64CPU.prototype.gdb_detach = function()
{
    if(!this.gdb_output) return;
    this.wasm["rv64_gdb_detach"]();
    this.gdb_output = null;
    this.stop_idling();
};

/**
 * Bytes from the debugger.
 * @param {!Uint8Array} data
 */
RV64CPU.prototype.gdb_input = function(data)
{
    if(!this.gdb_output) return;
    const ptr = this.wasm["rv64_gdb_in_ptr"]();
    for(let i = 0; i < data.length; i += 4096)
    {
        const chunk = data.subarray(i, i + 4096);
        new Uint8Array(this.wasm_memory.buffer, ptr, chunk.length).set(chunk);
        this.wasm["rv64_gdb_input"](chunk.length);
    }
    this.gdb_flush();
    // It may have resumed the hart
    this.stop_idling();
};

/** Pass the stub's replies on. */
RV64CPU.prototype.gdb_flush = function()
{
    const len = this.wasm["rv64_gdb_out_len"]();
    if(!len || !this.gdb_output) return;
    const bytes = new Uint8Array(this.wasm_memory.buffer, this.wasm["rv64_gdb_out_ptr"](), len).slice();
    this.wasm["rv64_gdb_out_clear"]();
    this.gdb_output(bytes);
};

/** @return {Array<number>} JIT counters: compiled, entered, stale, failed, instructions, slow loads/stores */
RV64CPU.prototype.jit_stats = function()
{
    return [0, 1, 2, 3, 4, 5].map(n => this.wasm["rv64_jit_stat"](n));
};

/**
 * CH32V: a GPIO port's output data register (port 0: A, 1: B, ...).
 * @param {number} port
 * @return {number}
 */
RV64CPU.prototype.ch32_gpio_out = function(port)
{
    return this.ch32 ? this.wasm["rv64_ch32_gpio_out"](port) : 0;
};

/**
 * CH32V: drive input pins of a GPIO port (port 0: A, 1: B, ...).
 * @param {number} port
 * @param {number} mask the pins driven (others read what the chip drives or pulls)
 * @param {number} levels their levels
 */
RV64CPU.prototype.ch32_set_gpio_in = function(port, mask, levels)
{
    if(this.ch32) this.wasm["rv64_ch32_set_gpio_in"](port, mask, levels);
};

/**
 * CH32V: set an ADC channel's input.
 * @param {number} channel
 * @param {number} value 0 to 1023
 */
RV64CPU.prototype.ch32_set_adc = function(channel, value)
{
    if(this.ch32) this.wasm["rv64_ch32_set_adc"](channel, value);
};

/**
 * CH32V: the SSD1306 OLED display on I2C1 (dev/ssd1306.rs): its frame
 * counter, which changes when what it shows may have, its 128 x 64 pixels
 * (a byte each, 0 or 1, row by row; a view into wasm memory, valid until the
 * next call) and its contrast (0 to 255).
 * @param {number} last_frame the frame counter of the pixels the caller has
 * @return {?{frame: number, pixels: ?Uint8Array, contrast: number}} pixels is
 * null if the frame counter is still last_frame
 */
RV64CPU.prototype.ch32_display = function(last_frame)
{
    if(!this.ch32) return null;
    const frame = this.wasm["rv64_ch32_oled_frame"]();
    if(!frame) return null;
    const pixels = frame === last_frame ? null :
        new Uint8Array(this.wasm_memory.buffer, this.wasm["rv64_ch32_oled_render"]() >>> 0, 128 * 64);
    return { frame, pixels, contrast: this.wasm["rv64_ch32_oled_contrast"]() };
};

/**
 * CH32V: record changes of output pins (e.g. a buzzer's), taken with
 * ch32_take_gpio_edges.
 * @param {number} port
 * @param {number} mask
 */
RV64CPU.prototype.ch32_watch_gpio = function(port, mask)
{
    if(this.ch32) this.wasm["rv64_ch32_watch_gpio"](port, mask);
};

/**
 * CH32V: the watched pins' changes since the last call, and the virtual
 * time now (ms).
 * @return {{now: number, edges: !Array<{time: number, port: number, value: number}>}}
 */
RV64CPU.prototype.ch32_take_gpio_edges = function()
{
    const edges = [];
    if(this.ch32)
    {
        const n = this.wasm["rv64_ch32_take_edges"]();
        const a = new Float64Array(this.wasm_memory.buffer, this.wasm["rv64_ch32_edges_ptr"]() >>> 0, n * 3);
        for(let i = 0; i < n; i++)
        {
            edges.push({ time: a[3 * i], port: a[3 * i + 1], value: a[3 * i + 2] });
        }
    }
    return { now: this.ch32 ? this.wasm["rv64_now_ms"]() : 0, edges };
};

/** @return {number} instructions executed since boot */
RV64CPU.prototype.get_instruction_counter = function()
{
    return this.wasm["rv64_icount"]();
};
