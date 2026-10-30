// A Cortex-M machine for v86: the Rust core in src/cm (v86-cm.wasm), running
// PebbleOS's emulator boards (arch "pebble-flint", "pebble-emery" or
// "pebble-gabbro"): the firmware ELF as `bios`, the external flash's image as
// `spi_flash`; or the card10 badge (arch "card10", two cores): Epicardium's
// ELF as `bios`, Pycardium's as `core1`, the FAT file system's image as
// `spi_flash`. The machine keeps to real time: it runs while it's behind the
// wall clock and sleeps (the page's event loop) while its firmware idles.

import { dbg_assert } from "./log.js";

/** Instructions per wasm call, and the most time one main_loop slice takes */
const STEPS_PER_RUN = 100000;
const TIME_PER_SLICE = 10;

const BOARDS = { "pebble-flint": 0, "pebble-emery": 1, "pebble-gabbro": 2, "card10": 3 };

const now = () => typeof performance !== "undefined" ? performance.now() : Date.now();

/**
 * @constructor
 * @param {!Object} bus
 * @param {!Object} wasm
 * @param {function()} stop_idling
 */
export function CmCPU(bus, wasm, stop_idling)
{
    this.bus = bus;
    this.wasm = wasm.exports;
    this.wasm_memory = this.wasm["memory"];
    this.board = BOARDS[wasm.arch] || 0;
    this.stop_idling = stop_idling;
    this.settings = null;
    /** Machine ms = wall ms - this (set at boot, and when the machine stopped) */
    this.clock_offset = 0;
    this.halted = false;
    /** The buttons held (bit 0 back, 1 up, 2 select, 3 down) */
    this.buttons = 0;
    /** How much slower than its own the CPU's clock runs (cm_set_clock_scale),
        when this host can't keep up with the machine; and when it was last
        looked at */
    this.clock_scale = 1;
    this.scale_checked = 0;
}

CmCPU.prototype.init = function(settings, device_bus)
{
    this.settings = settings;
    this.boot();
};

CmCPU.prototype.boot = function()
{
    const settings = this.settings;
    dbg_assert(settings.bios, "cortex-m: the firmware (bios, an ELF) is required");
    const elf = new Uint8Array(settings.bios);
    const spi = settings.spi_flash ? new Uint8Array(settings.spi_flash) : new Uint8Array(0);
    const core1 = settings.core1 ? new Uint8Array(settings.core1) : new Uint8Array(0);
    const ep = this.wasm["cm_alloc"](elf.length);
    const sp = this.wasm["cm_alloc"](spi.length);
    const cp = this.wasm["cm_alloc"](core1.length);
    new Uint8Array(this.wasm_memory.buffer, ep, elf.length).set(elf);
    new Uint8Array(this.wasm_memory.buffer, sp, spi.length).set(spi);
    new Uint8Array(this.wasm_memory.buffer, cp, core1.length).set(core1);
    const epoch = Date.now() / 1000;
    const result = this.board === BOARDS["card10"] ?
        this.wasm["cm_init_card10"](ep, elf.length, cp, core1.length, sp, spi.length, epoch) :
        this.wasm["cm_init"](this.board, ep, elf.length, sp, spi.length, epoch);
    this.wasm["cm_free"](ep, elf.length);
    this.wasm["cm_free"](sp, spi.length);
    this.wasm["cm_free"](cp, core1.length);
    dbg_assert(result === 0, "cortex-m: the firmware doesn't load");
    this.clock_offset = now();
    this.halted = false;
};

CmCPU.prototype.main_loop = function()
{
    if(this.halted)
    {
        return 1000;
    }
    const start = now();
    for(;;)
    {
        // Where the machine should be, and where it is
        const target = now() - this.clock_offset;
        const machine = this.wasm["cm_now_ms"]();
        this.adjust_clock(target - machine);
        if(machine > target + 1)
        {
            // Ahead of real time (busy-waiting firmware): let time catch up
            return Math.min(machine - target, 100);
        }
        if(machine < target - 1000)
        {
            // Far behind (the tab was in the background): don't race to catch up
            this.clock_offset = now() - machine;
        }
        const exit = this.wasm["cm_run"](STEPS_PER_RUN);
        this.flush_uarts();
        if(exit === 3)
        {
            // reset: a new machine, at its own clock
            this.wasm["cm_set_clock_scale"](this.clock_scale);
        }
        if(exit === 1)
        {
            // Idle: wake at the next event, or now if it's already due
            const next = this.wasm["cm_next_event_ms"]();
            const t = now() - this.clock_offset;
            if(next >= 0 && next <= t && now() - start < TIME_PER_SLICE)
            {
                this.wasm["cm_advance_to_ms"](next);
                continue;
            }
            this.wasm["cm_advance_to_ms"](t);
            return next < 0 ? 100 : Math.max(0, Math.min(next - t, 100));
        }
        if(exit === 2)
        {
            this.halted = true;
            this.bus.send("cortex-m-halted");
            return 1000;
        }
        if(now() - start >= TIME_PER_SLICE)
        {
            return 0;
        }
    }
};

/**
 * Keep the machine to real time on a host too slow for it (the card10's two
 * busy cores): while it falls behind, its CPU's clock runs slower (each
 * instruction takes more of the machine's time), as if the chip were clocked
 * lower; when it keeps up again, back towards its own clock. Its timers,
 * clocks and RTC go on in real time either way.
 * @param {number} lag how far behind real time the machine is (ms)
 */
CmCPU.prototype.adjust_clock = function(lag)
{
    const t = now();
    if(t - this.scale_checked < 500)
    {
        return;
    }
    this.scale_checked = t;
    let scale = this.clock_scale;
    if(lag > 200)
    {
        scale = Math.min(scale * 1.25, 8);
    }
    else if(lag < 20 && scale > 1)
    {
        scale = Math.max(scale / 1.05, 1);
    }
    if(scale !== this.clock_scale)
    {
        this.clock_scale = scale;
        this.wasm["cm_set_clock_scale"](scale);
    }
};

/**
 * Watch the card10's cores' interaction, or stop.
 * @param {boolean} on
 */
CmCPU.prototype.trace = function(on)
{
    this.wasm["cm_trace"](+on);
};

/**
 * What the card10's cores did since the last call: the machine's time (ms),
 * core 0's and core 1's time awake (ms) since then, then (time, kind, id) for
 * each API call (kind 0), return (1) and interrupt (2); null when not watching.
 * @return {?Float64Array}
 */
CmCPU.prototype.trace_take = function()
{
    const n = this.wasm["cm_trace_take"]();
    return n ? new Float64Array(this.wasm_memory.buffer, this.wasm["cm_trace_ptr"](), n).slice() : null;
};

/** The firmware's UART output, as "cortex-m-uart<n>" events (Uint8Array). */
CmCPU.prototype.flush_uarts = function()
{
    for(let n = 0; n < 4; n++)
    {
        const len = this.wasm["cm_uart_take"](n);
        if(len)
        {
            const bytes = new Uint8Array(this.wasm_memory.buffer, this.wasm["cm_out_ptr"](), len).slice();
            this.bus.send("cortex-m-uart" + n, bytes);
        }
    }
};

CmCPU.prototype.reboot_internal = function()
{
    this.wasm["cm_reset"]();
    this.halted = false;
};

CmCPU.prototype.get_instruction_counter = function()
{
    return this.wasm["cm_instructions"]() >>> 0;
};

/**
 * The buttons held: bit 0 back, 1 up, 2 select, 3 down.
 * @param {number} state
 */
CmCPU.prototype.set_buttons = function(state)
{
    this.buttons = state;
    this.wasm["cm_set_buttons"](state);
    this.stop_idling();
};

/**
 * The finger on the touch screen (boards with one): down or up, at (x, y)
 * in display pixels.
 * @param {boolean} down
 * @param {number} x
 * @param {number} y
 */
CmCPU.prototype.touch = function(down, x, y)
{
    this.wasm["cm_touch"](+down, x, y);
    this.stop_idling();
};

/** @return {boolean} */
CmCPU.prototype.has_touch = function()
{
    return !!this.wasm["cm_has_touch"]();
};

/**
 * Bytes into UART n.
 * @param {number} n
 * @param {!Uint8Array} bytes
 */
CmCPU.prototype.uart_input = function(n, bytes)
{
    for(const b of bytes)
    {
        this.wasm["cm_uart_input"](n, b);
    }
    this.stop_idling();
};

/**
 * The display: its frame counter (changes when what it shows may have; 0 while
 * it's off) and, if it's changed since last_frame, its pixels (RGBA, a view
 * into wasm memory valid until the next call).
 * @param {number} last_frame
 * @return {{frame: number, width: number, height: number, pixels: ?Uint8ClampedArray}}
 */
CmCPU.prototype.display = function(last_frame)
{
    const frame = this.wasm["cm_display_frame"]();
    const width = this.wasm["cm_display_width"]();
    const height = this.wasm["cm_display_height"]();
    let pixels = null;
    if(frame !== last_frame)
    {
        // (rendering first: memory may grow meanwhile, replacing its buffer)
        const ptr = this.wasm["cm_display_render"]() >>> 0;
        pixels = new Uint8ClampedArray(this.wasm_memory.buffer, ptr, width * height * 4);
    }
    return { frame, width, height, pixels };
};
