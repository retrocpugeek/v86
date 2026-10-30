// A Cortex-M machine for v86: the Rust core in src/cm (v86-cm.wasm), running
// PebbleOS's emulator boards (arch "pebble-flint", "pebble-emery" or
// "pebble-gabbro"): the firmware ELF as `bios`, the external flash's image as
// `spi_flash`. The machine keeps to real time: it runs while it's behind the
// wall clock and sleeps (the page's event loop) while PebbleOS idles.

import { dbg_assert } from "./log.js";

/** Instructions per wasm call, and the most time one main_loop slice takes */
const STEPS_PER_RUN = 100000;
const TIME_PER_SLICE = 10;

const BOARDS = { "pebble-flint": 0, "pebble-emery": 1, "pebble-gabbro": 2 };

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
    const ep = this.wasm["cm_alloc"](elf.length);
    const sp = this.wasm["cm_alloc"](spi.length);
    new Uint8Array(this.wasm_memory.buffer, ep, elf.length).set(elf);
    new Uint8Array(this.wasm_memory.buffer, sp, spi.length).set(spi);
    const result = this.wasm["cm_init"](this.board, ep, elf.length, sp, spi.length, Date.now() / 1000);
    this.wasm["cm_free"](ep, elf.length);
    this.wasm["cm_free"](sp, spi.length);
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
