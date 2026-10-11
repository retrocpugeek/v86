// ARM PL031 real-time clock for the aarch64 machine (host wall-clock time).

const PERIPH_ID = [0x31, 0x10, 0x14, 0x00];
const CELL_ID = [0x0D, 0xF0, 0x05, 0xB1];

/**
 * @constructor
 * @param {function(boolean)} set_irq
 */
export function PL031(set_irq)
{
    this.set_irq = set_irq;
    /** Seconds added to host time by guest writes to the load register. */
    this.offset = 0;
    this.mr = 0;
    this.imsc = 0;
    this.ris = 0;
}

PL031.prototype.now = function()
{
    return (Math.floor(Date.now() / 1000) + this.offset) >>> 0;
};

/**
 * @param {number} offset
 * @return {number}
 */
PL031.prototype.read = function(offset)
{
    switch(offset)
    {
        case 0x00: return this.now();
        case 0x04: return this.mr;
        case 0x08: return this.now();
        case 0x0C: return 1;
        case 0x10: return this.imsc;
        case 0x14: return this.ris;
        case 0x18: return this.ris & this.imsc;
    }
    if(offset >= 0xFE0 && offset < 0xFF0) return PERIPH_ID[(offset - 0xFE0) >> 2];
    if(offset >= 0xFF0 && offset < 0x1000) return CELL_ID[(offset - 0xFF0) >> 2];
    return 0;
};

/**
 * @param {number} offset
 * @param {number} value
 */
PL031.prototype.write = function(offset, value)
{
    switch(offset)
    {
        case 0x04: this.mr = value; break;
        case 0x08: this.offset += (value >>> 0) - this.now(); break;
        case 0x10: this.imsc = value & 1; break;
        case 0x1C: this.ris &= ~value; break;
    }
    this.set_irq((this.ris & this.imsc) !== 0);
};
