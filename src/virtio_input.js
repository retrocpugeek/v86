// virtio-input: a keyboard or a tablet (absolute pointer), fed with Linux
// input events (type, code, value).
// https://docs.oasis-open.org/virtio/virtio/v1.2/csd01/virtio-v1.2-csd01.html#x1-3390008

import { VirtIO, VIRTIO_F_VERSION_1 } from "./virtio.js";

// For Types Only
import { CPU } from "./cpu.js";

// Config space selectors
const CFG_ID_NAME = 0x01;
const CFG_ID_SERIAL = 0x02;
const CFG_ID_DEVIDS = 0x03;
const CFG_PROP_BITS = 0x10;
const CFG_EV_BITS = 0x11;
const CFG_ABS_INFO = 0x12;

// Linux input event types and codes
export const EV_SYN = 0;
export const EV_KEY = 1;
export const EV_REL = 2;
export const EV_ABS = 3;
export const SYN_REPORT = 0;
export const BTN_LEFT = 0x110;
export const BTN_RIGHT = 0x111;
export const BTN_MIDDLE = 0x112;
export const REL_WHEEL = 8;
export const ABS_X = 0;
export const ABS_Y = 1;
/** Range of the tablet's ABS_X and ABS_Y */
export const TABLET_MAX = 0x7FFF;

const QUEUE_SIZE = 64;
/** Events kept while the driver has no buffers posted */
const MAX_PENDING = 256;

/**
 * @constructor
 * @param {CPU} cpu
 * @param {string} kind "keyboard" or "tablet"
 */
export function VirtioInput(cpu, kind)
{
    this.kind = kind;
    this.name = kind === "keyboard" ? "v86 keyboard" : "v86 tablet";
    /** @type {!Array<!Array<number>>} events not yet delivered */
    this.pending = [];
    this.select = 0;
    this.subsel = 0;
    /** @type {!Uint8Array} the selected config data (the union "u") */
    this.data = new Uint8Array(128);
    this.size = 0;

    const u = [];
    for(let i = 0; i < 32; i++)
    {
        u.push({ bytes: 4, name: "u" + i, read: () => this.data[4 * i] | this.data[4 * i + 1] << 8 |
            this.data[4 * i + 2] << 16 | this.data[4 * i + 3] << 24, write: data => {} });
    }

    /** @type {VirtIO} */
    this.virtio = new VirtIO(cpu,
    {
        name: "virtio-input-" + kind,
        pci_id: (kind === "keyboard" ? 0x0E : 0x0F) << 3,
        device_id: 0x1052,
        subsystem_device_id: 18,
        common:
        {
            initial_port: kind === "keyboard" ? 0xD000 : 0xD400,
            queues: [{ size_supported: QUEUE_SIZE, notify_offset: 0 }, { size_supported: QUEUE_SIZE, notify_offset: 1 }],
            features: [VIRTIO_F_VERSION_1],
            on_driver_ok: () => {},
        },
        notification:
        {
            initial_port: kind === "keyboard" ? 0xD100 : 0xD500,
            single_handler: false,
            handlers: [
                () => this.deliver(),
                () => this.drain_status(),
            ],
        },
        isr_status:
        {
            initial_port: kind === "keyboard" ? 0xD200 : 0xD600,
        },
        device_specific:
        {
            initial_port: kind === "keyboard" ? 0xD300 : 0xD700,
            struct: [
                { bytes: 1, name: "select", read: () => this.select, write: data => { this.select = data & 0xFF; this.update_config(); } },
                { bytes: 1, name: "subsel", read: () => this.subsel, write: data => { this.subsel = data & 0xFF; this.update_config(); } },
                { bytes: 1, name: "size", read: () => this.size, write: data => {} },
                { bytes: 4, name: "reserved0", read: () => 0, write: data => {} },
                { bytes: 1, name: "reserved1", read: () => 0, write: data => {} },
            ].concat(u),
        },
    });
}

/** Fill `data` and `size` for the current select/subsel. */
VirtioInput.prototype.update_config = function()
{
    const data = this.data;
    data.fill(0);
    let size = 0;
    const bits = codes =>
    {
        for(const c of codes)
        {
            data[c >> 3] |= 1 << (c & 7);
            size = Math.max(size, (c >> 3) + 1);
        }
    };
    const keyboard = this.kind === "keyboard";
    switch(this.select)
    {
        case CFG_ID_NAME:
        {
            const name = new TextEncoder().encode(this.name);
            data.set(name);
            size = name.length;
            break;
        }
        case CFG_ID_SERIAL:
            break;
        case CFG_ID_DEVIDS:
            // bustype BUS_VIRTUAL, vendor, product, version
            data.set([0x06, 0, 0xF4, 0x1A, keyboard ? 1 : 2, 0, 1, 0]);
            size = 8;
            break;
        case CFG_PROP_BITS:
            break;
        case CFG_EV_BITS:
            if(keyboard && this.subsel === EV_KEY)
            {
                // KEY_ESC .. KEY_MICMUTE: everything a keyboard can send
                const codes = [];
                for(let c = 1; c < 0xF8; c++) codes.push(c);
                bits(codes);
            }
            else if(!keyboard && this.subsel === EV_KEY)
            {
                bits([BTN_LEFT, BTN_RIGHT, BTN_MIDDLE]);
            }
            else if(!keyboard && this.subsel === EV_ABS)
            {
                bits([ABS_X, ABS_Y]);
            }
            else if(!keyboard && this.subsel === EV_REL)
            {
                bits([REL_WHEEL]);
            }
            break;
        case CFG_ABS_INFO:
            if(!keyboard && (this.subsel === ABS_X || this.subsel === ABS_Y))
            {
                // min, max, fuzz, flat, res
                new DataView(data.buffer).setUint32(4, TABLET_MAX, true);
                size = 20;
            }
            break;
    }
    this.size = size;
};

/**
 * Send events (each [type, code, value]), followed by a SYN_REPORT.
 * @param {!Array<!Array<number>>} events
 */
VirtioInput.prototype.send = function(events)
{
    if(!this.virtio.queues[0].is_configured())
    {
        return;
    }
    for(const e of events)
    {
        this.pending.push(e);
    }
    this.pending.push([EV_SYN, SYN_REPORT, 0]);
    if(this.pending.length > MAX_PENDING)
    {
        this.pending.splice(0, this.pending.length - MAX_PENDING);
    }
    this.deliver();
};

/** Move pending events into the buffers the driver has posted. */
VirtioInput.prototype.deliver = function()
{
    const queue = this.virtio.queues[0];
    let sent = false;
    while(this.pending.length && queue.has_request())
    {
        const [type, code, value] = this.pending.shift();
        const event = new Uint8Array(8);
        const view = new DataView(event.buffer);
        view.setUint16(0, type, true);
        view.setUint16(2, code, true);
        view.setInt32(4, value, true);
        const bufchain = queue.pop_request();
        bufchain.set_next_blob(event);
        queue.push_reply(bufchain);
        sent = true;
    }
    if(sent)
    {
        queue.flush_replies();
    }
};

/** The status queue carries LED updates: just return the buffers. */
VirtioInput.prototype.drain_status = function()
{
    const queue = this.virtio.queues[1];
    while(queue.has_request())
    {
        queue.push_reply(queue.pop_request());
    }
    queue.flush_replies();
};
