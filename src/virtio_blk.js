// virtio-blk backed by one of v86's disk buffers (SyncBuffer, AsyncXHRBuffer, ...).
// https://docs.oasis-open.org/virtio/virtio/v1.2/csd01/virtio-v1.2-csd01.html#x1-2990002

import { dbg_log } from "./log.js";
import { LOG_VIRTIO } from "./const.js";
import { VirtIO, VIRTIO_F_VERSION_1, VIRTIO_F_RING_INDIRECT_DESC } from "./virtio.js";

// For Types Only
import { CPU } from "./cpu.js";
import { BusConnector } from "./bus.js";

const VIRTIO_BLK_F_SEG_MAX = 2;
const VIRTIO_BLK_F_BLK_SIZE = 6;
const VIRTIO_BLK_F_FLUSH = 9;

const VIRTIO_BLK_T_IN = 0;
const VIRTIO_BLK_T_OUT = 1;
const VIRTIO_BLK_T_FLUSH = 4;
const VIRTIO_BLK_T_GET_ID = 8;

const VIRTIO_BLK_S_OK = 0;
const VIRTIO_BLK_S_IOERR = 1;
const VIRTIO_BLK_S_UNSUPP = 2;

const SECTOR_SIZE = 512;
const QUEUE_SIZE = 256;

/**
 * @constructor
 * @param {CPU} cpu
 * @param {BusConnector} bus
 * @param {Object} buffer disk image, with get(start, len, fn) and set(start, data, fn)
 */
export function VirtioBlk(cpu, bus, buffer)
{
    this.bus = bus;
    this.buffer = buffer;
    this.sectors = Math.floor(buffer.byteLength / SECTOR_SIZE);

    /** @type {VirtIO} */
    this.virtio = new VirtIO(cpu,
    {
        name: "virtio-blk",
        pci_id: 0x0B << 3,
        device_id: 0x1042,
        subsystem_device_id: 2,
        common:
        {
            initial_port: 0xCA00,
            queues: [{ size_supported: QUEUE_SIZE, notify_offset: 0 }],
            features:
            [
                VIRTIO_BLK_F_SEG_MAX,
                VIRTIO_BLK_F_BLK_SIZE,
                VIRTIO_BLK_F_FLUSH,
                VIRTIO_F_RING_INDIRECT_DESC,
                VIRTIO_F_VERSION_1,
            ],
            on_driver_ok: () => {},
        },
        notification:
        {
            initial_port: 0xCB00,
            single_handler: true,
            handlers: [queue_id => this.handle_queue(queue_id)],
        },
        isr_status:
        {
            initial_port: 0xCC00,
        },
        device_specific:
        {
            initial_port: 0xCD00,
            struct:
            [
                { bytes: 4, name: "capacity_low", read: () => this.sectors >>> 0, write: data => {} },
                { bytes: 4, name: "capacity_high", read: () => Math.floor(this.sectors / 0x100000000), write: data => {} },
                { bytes: 4, name: "size_max", read: () => 0, write: data => {} },
                { bytes: 4, name: "seg_max", read: () => QUEUE_SIZE - 2, write: data => {} },
                { bytes: 2, name: "cylinders", read: () => 0, write: data => {} },
                { bytes: 1, name: "heads", read: () => 0, write: data => {} },
                { bytes: 1, name: "sectors", read: () => 0, write: data => {} },
                { bytes: 4, name: "blk_size", read: () => SECTOR_SIZE, write: data => {} },
            ],
        },
    });
}

VirtioBlk.prototype.handle_queue = function(queue_id)
{
    const queue = this.virtio.queues[queue_id];

    while(queue.has_request())
    {
        const bufchain = queue.pop_request();
        const header = new Uint8Array(16);
        bufchain.get_next_blob(header);
        const view = new DataView(header.buffer);
        const type = view.getUint32(0, true);
        const sector = view.getUint32(8, true) + view.getUint32(12, true) * 0x100000000;
        this.handle_request(queue, bufchain, type, sector);
    }
};

VirtioBlk.prototype.handle_request = function(queue, bufchain, type, sector)
{
    const offset = sector * SECTOR_SIZE;
    // The last device-writable byte is the status.
    const data_writable = bufchain.length_writable - 1;
    const data_readable = bufchain.length_readable - 16;

    const finish = (status, data) =>
    {
        const out = new Uint8Array((data ? data.length : 0) + 1);
        if(data) out.set(data);
        out[out.length - 1] = status;
        bufchain.set_next_blob(out);
        queue.push_reply(bufchain);
        queue.flush_replies();
    };

    switch(type)
    {
        case VIRTIO_BLK_T_IN:
            if(offset + data_writable > this.buffer.byteLength)
            {
                finish(VIRTIO_BLK_S_IOERR, null);
                return;
            }
            this.buffer.get(offset, data_writable, data =>
            {
                finish(VIRTIO_BLK_S_OK, data);
                this.bus.send("virtio-blk-read", [offset, data_writable]);
            });
            break;

        case VIRTIO_BLK_T_OUT:
        {
            if(offset + data_readable > this.buffer.byteLength)
            {
                finish(VIRTIO_BLK_S_IOERR, null);
                return;
            }
            const data = new Uint8Array(data_readable);
            bufchain.get_next_blob(data);
            this.buffer.set(offset, data, () =>
            {
                finish(VIRTIO_BLK_S_OK, null);
                this.bus.send("virtio-blk-write", [offset, data_readable]);
            });
            break;
        }

        case VIRTIO_BLK_T_FLUSH:
            finish(VIRTIO_BLK_S_OK, null);
            break;

        case VIRTIO_BLK_T_GET_ID:
        {
            const id = new Uint8Array(Math.min(20, data_writable));
            id.set(new TextEncoder().encode("v86-virtio-blk").subarray(0, id.length));
            finish(VIRTIO_BLK_S_OK, id);
            break;
        }

        default:
            dbg_log("virtio-blk: unsupported request type " + type, LOG_VIRTIO);
            finish(VIRTIO_BLK_S_UNSUPP, null);
    }
};

VirtioBlk.prototype.get_state = function()
{
    return [this.virtio];
};

VirtioBlk.prototype.set_state = function(state)
{
    this.virtio.set_state(state[0]);
};
