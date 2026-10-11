// ARM PL011 UART, used as the aarch64 machine's serial console (ttyAMA0).
// Register model follows QEMU's hw/char/pl011.c. Output and input go through
// the same bus events as the x86 UART, so the serial adapters work unchanged.

import { BusConnector } from "./bus.js";

const INT_RX = 1 << 4;
const INT_TX = 1 << 5;
const INT_RT = 1 << 6;

const FR_RXFE = 1 << 4;
const FR_TXFE = 1 << 7;

const PERIPH_ID = [0x11, 0x10, 0x14, 0x00];
const CELL_ID = [0x0D, 0xF0, 0x05, 0xB1];

/**
 * @constructor
 * @param {BusConnector} bus
 * @param {function(boolean)} set_irq
 * @param {number=} port serial port number for bus events (default 0)
 */
export function PL011(bus, set_irq, port)
{
    this.bus = bus;
    this.set_irq = set_irq;
    this.port = port || 0;

    /** @type {!Array<number>} */
    this.rx = [];
    this.int_level = 0;
    this.imsc = 0;
    this.ibrd = 0;
    this.fbrd = 0;
    this.lcr_h = 0;
    this.cr = 0x300;
    this.ifls = 0x12;
    this.ilpr = 0;
    this.dmacr = 0;

    bus.register("serial" + this.port + "-input", function(byte)
    {
        this.rx.push(byte & 0xFF);
        this.update();
    }, this);
}

PL011.prototype.update = function()
{
    if(this.rx.length)
    {
        this.int_level |= INT_RX | INT_RT;
    }
    this.set_irq((this.int_level & this.imsc) !== 0);
};

/**
 * @param {number} offset
 * @return {number}
 */
PL011.prototype.read = function(offset)
{
    switch(offset)
    {
        case 0x000:
        {
            const c = this.rx.length ? this.rx.shift() : 0;
            if(!this.rx.length)
            {
                this.int_level &= ~(INT_RX | INT_RT);
            }
            this.update();
            return c;
        }
        case 0x018: return FR_TXFE | (this.rx.length ? 0 : FR_RXFE);
        case 0x020: return this.ilpr;
        case 0x024: return this.ibrd;
        case 0x028: return this.fbrd;
        case 0x02C: return this.lcr_h;
        case 0x030: return this.cr;
        case 0x034: return this.ifls;
        case 0x038: return this.imsc;
        case 0x03C: return this.int_level;
        case 0x040: return this.int_level & this.imsc;
        case 0x048: return this.dmacr;
    }
    if(offset >= 0xFE0 && offset < 0xFF0) return PERIPH_ID[(offset - 0xFE0) >> 2];
    if(offset >= 0xFF0 && offset < 0x1000) return CELL_ID[(offset - 0xFF0) >> 2];
    return 0;
};

/**
 * @param {number} offset
 * @param {number} value
 */
PL011.prototype.write = function(offset, value)
{
    switch(offset)
    {
        case 0x000:
            this.bus.send("serial" + this.port + "-output-byte", value & 0xFF);
            this.int_level |= INT_TX;
            break;
        case 0x020: this.ilpr = value; break;
        case 0x024: this.ibrd = value; break;
        case 0x028: this.fbrd = value; break;
        case 0x02C: this.lcr_h = value; break;
        case 0x030: this.cr = value; break;
        case 0x034: this.ifls = value; break;
        case 0x038: this.imsc = value; break;
        case 0x044: this.int_level &= ~value; break;
        case 0x048: this.dmacr = value; break;
    }
    this.update();
};
