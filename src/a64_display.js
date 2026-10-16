// Display for the aarch64 machine (option a64_display): a simple-framebuffer
// in guest RAM (Linux's simplefb driver), drawn to a canvas, and virtio-input
// keyboard and tablet devices fed from the canvas's keyboard and pointer
// events (mouse and touch).

import { VirtioInput, EV_KEY, EV_ABS, EV_REL, ABS_X, ABS_Y, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, REL_WHEEL, TABLET_MAX }
    from "./virtio_input.js";

/** KeyboardEvent.code -> Linux key code */
const KEY_CODES = {
    "Escape": 1, "Digit1": 2, "Digit2": 3, "Digit3": 4, "Digit4": 5, "Digit5": 6, "Digit6": 7, "Digit7": 8,
    "Digit8": 9, "Digit9": 10, "Digit0": 11, "Minus": 12, "Equal": 13, "Backspace": 14, "Tab": 15,
    "KeyQ": 16, "KeyW": 17, "KeyE": 18, "KeyR": 19, "KeyT": 20, "KeyY": 21, "KeyU": 22, "KeyI": 23, "KeyO": 24,
    "KeyP": 25, "BracketLeft": 26, "BracketRight": 27, "Enter": 28, "ControlLeft": 29, "KeyA": 30, "KeyS": 31,
    "KeyD": 32, "KeyF": 33, "KeyG": 34, "KeyH": 35, "KeyJ": 36, "KeyK": 37, "KeyL": 38, "Semicolon": 39,
    "Quote": 40, "Backquote": 41, "ShiftLeft": 42, "Backslash": 43, "KeyZ": 44, "KeyX": 45, "KeyC": 46,
    "KeyV": 47, "KeyB": 48, "KeyN": 49, "KeyM": 50, "Comma": 51, "Period": 52, "Slash": 53, "ShiftRight": 54,
    "NumpadMultiply": 55, "AltLeft": 56, "Space": 57, "CapsLock": 58, "F1": 59, "F2": 60, "F3": 61, "F4": 62,
    "F5": 63, "F6": 64, "F7": 65, "F8": 66, "F9": 67, "F10": 68, "NumLock": 69, "ScrollLock": 70,
    "Numpad7": 71, "Numpad8": 72, "Numpad9": 73, "NumpadSubtract": 74, "Numpad4": 75, "Numpad5": 76,
    "Numpad6": 77, "NumpadAdd": 78, "Numpad1": 79, "Numpad2": 80, "Numpad3": 81, "Numpad0": 82,
    "NumpadDecimal": 83, "IntlBackslash": 86, "F11": 87, "F12": 88, "NumpadEnter": 96, "ControlRight": 97,
    "NumpadDivide": 98, "PrintScreen": 99, "AltRight": 100, "Home": 102, "ArrowUp": 103, "PageUp": 104,
    "ArrowLeft": 105, "ArrowRight": 106, "End": 107, "ArrowDown": 108, "PageDown": 109, "Insert": 110,
    "Delete": 111, "Pause": 119, "MetaLeft": 125, "MetaRight": 126, "ContextMenu": 127,
};

/** Frames drawn per second at most */
const MAX_FPS = 30;

/**
 * @constructor
 * @param {?} cpu A64CPU (as the virtio devices' CPU)
 * @param {{ width: (number|undefined), height: (number|undefined), canvas: (HTMLCanvasElement|undefined) }} options
 * @param {number} addr guest physical address of the framebuffer
 */
export function A64Display(cpu, options, addr)
{
    this.cpu = cpu;
    this.width = options.width || 800;
    this.height = options.height || 600;
    this.stride = this.width * 4;
    this.addr = addr;
    this.keyboard = new VirtioInput(cpu, "keyboard");
    this.tablet = new VirtioInput(cpu, "tablet");
    this.canvas = options.canvas || null;
    this.buttons = 0;
    if(this.canvas)
    {
        this.init_canvas(this.canvas);
    }
}

/** Bytes of RAM the framebuffer needs (whole MBs). */
A64Display.size = function(options)
{
    const bytes = (options.width || 800) * (options.height || 600) * 4;
    return Math.ceil(bytes / 0x100000) * 0x100000;
};

/**
 * Device tree nodes: the framebuffer, and its memory reserved from the kernel.
 * @param {Object} root FdtNode
 * @param {function(number, number): !Array<number>} reg
 */
A64Display.prototype.add_dtb_nodes = function(root, reg)
{
    const size = A64Display.size(this);
    const name = "framebuffer@" + this.addr.toString(16);
    root.child("reserved-memory")
        .prop("#address-cells", [2])
        .prop("#size-cells", [2])
        .prop("ranges", true)
        .child(name)
        .prop("reg", reg(this.addr, size))
        .prop("no-map", true);
    root.child(name)
        .prop("compatible", "simple-framebuffer")
        .prop("reg", reg(this.addr, size))
        .prop("width", [this.width])
        .prop("height", [this.height])
        .prop("stride", [this.stride])
        .prop("format", "x8r8g8b8");
};

/** @param {!HTMLCanvasElement} canvas */
A64Display.prototype.init_canvas = function(canvas)
{
    canvas.width = this.width;
    canvas.height = this.height;
    canvas.tabIndex = 0;
    this.context = canvas.getContext("2d");
    this.image = this.context.createImageData(this.width, this.height);
    this.pixels = new Uint32Array(this.image.data.buffer);
    this.last_draw = 0;

    const draw = time =>
    {
        if(time - this.last_draw >= 1000 / MAX_FPS - 2 && document.visibilityState === "visible")
        {
            this.last_draw = time;
            this.draw();
        }
        this.frame = requestAnimationFrame(draw);
    };
    this.frame = requestAnimationFrame(draw);

    canvas.addEventListener("keydown", e => this.on_key(/** @type {!KeyboardEvent} */ (e), 1));
    canvas.addEventListener("keyup", e => this.on_key(/** @type {!KeyboardEvent} */ (e), 0));
    canvas.addEventListener("pointermove", e => this.on_pointer(/** @type {!PointerEvent} */ (e)));
    canvas.addEventListener("pointerdown", e =>
    {
        canvas.focus();
        const pe = /** @type {!PointerEvent} */ (e);
        canvas.setPointerCapture(pe.pointerId);
        this.on_pointer(pe);
    });
    canvas.addEventListener("pointerup", e => this.on_pointer(/** @type {!PointerEvent} */ (e)));
    canvas.addEventListener("wheel", e =>
    {
        e.preventDefault();
        this.tablet.send([[EV_REL, REL_WHEEL, e.deltaY < 0 ? 1 : -1]]);
    }, { passive: false });
    canvas.addEventListener("contextmenu", e => e.preventDefault());
    // Taps shouldn't scroll or zoom the page
    canvas.style.touchAction = "none";
};

/** Copy the framebuffer (x8r8g8b8) to the canvas. */
A64Display.prototype.draw = function()
{
    const ram = this.cpu.ram();
    const offset = this.addr - 0x40000000 + ram.byteOffset;
    const out = this.pixels;
    if(offset % 4 === 0)
    {
        const fb = new Uint32Array(ram.buffer, offset, this.width * this.height);
        for(let i = 0; i < out.length; i++)
        {
            // 0x00RRGGBB -> bytes R, G, B, 255
            const v = fb[i];
            out[i] = 0xFF000000 | (v & 0xFF00) | (v >>> 16 & 0xFF) | (v & 0xFF) << 16;
        }
    }
    else
    {
        // Guest RAM isn't 4-byte aligned in wasm memory: bytes B, G, R, X
        const fb = new Uint8Array(ram.buffer, offset, this.width * this.height * 4);
        for(let i = 0; i < out.length; i++)
        {
            out[i] = 0xFF000000 | fb[4 * i] << 16 | fb[4 * i + 1] << 8 | fb[4 * i + 2];
        }
    }
    this.context.putImageData(this.image, 0, 0);
};

/**
 * @param {KeyboardEvent} e
 * @param {number} down
 */
A64Display.prototype.on_key = function(e, down)
{
    const code = KEY_CODES[e.code];
    if(code === undefined)
    {
        return;
    }
    e.preventDefault();
    this.keyboard.send([[EV_KEY, code, down && e.repeat ? 2 : down]]);
};

/** @param {PointerEvent} e */
A64Display.prototype.on_pointer = function(e)
{
    e.preventDefault();
    const rect = this.canvas.getBoundingClientRect();
    const fx = Math.min(Math.max((e.clientX - rect.left) / rect.width, 0), 1);
    const fy = Math.min(Math.max((e.clientY - rect.top) / rect.height, 0), 1);
    const events = [[EV_ABS, ABS_X, Math.round(fx * TABLET_MAX)], [EV_ABS, ABS_Y, Math.round(fy * TABLET_MAX)]];
    // e.buttons: 1 left, 2 right, 4 middle
    const buttons = e.buttons;
    for(const [bit, code] of [[1, BTN_LEFT], [2, BTN_RIGHT], [4, BTN_MIDDLE]])
    {
        if((buttons ^ this.buttons) & bit)
        {
            events.push([EV_KEY, code, buttons & bit ? 1 : 0]);
        }
    }
    this.buttons = buttons;
    this.tablet.send(events);
};

A64Display.prototype.destroy = function()
{
    if(this.frame !== undefined)
    {
        cancelAnimationFrame(this.frame);
    }
};
