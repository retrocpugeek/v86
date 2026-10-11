// Minimal flattened device tree (DTB) writer.
// https://devicetree-specification.readthedocs.io/en/stable/flattened-format.html

const FDT_MAGIC = 0xD00DFEED;
const FDT_BEGIN_NODE = 1;
const FDT_END_NODE = 2;
const FDT_PROP = 3;
const FDT_END = 9;

/**
 * A device tree node. Property values are:
 *   - a string, or an array of strings (string list)
 *   - an array of numbers (u32 cells)
 *   - a Uint8Array (raw bytes)
 *   - true (empty property)
 * @constructor
 * @param {string} name
 */
export function FdtNode(name)
{
    this.name = name;
    /** @type {!Array<!Array<*>>} */
    this.props = [];
    /** @type {!Array<!FdtNode>} */
    this.children = [];
}

/**
 * @param {string} name
 * @param {*} value
 * @return {!FdtNode}
 */
FdtNode.prototype.prop = function(name, value)
{
    this.props.push([name, value]);
    return this;
};

/**
 * @param {string} name
 * @return {!FdtNode}
 */
FdtNode.prototype.child = function(name)
{
    const node = new FdtNode(name);
    this.children.push(node);
    return node;
};

/** @param {*} value @return {!Uint8Array} */
function encode_value(value)
{
    if(value === true)
    {
        return new Uint8Array(0);
    }
    if(value instanceof Uint8Array)
    {
        return value;
    }
    if(typeof value === "string")
    {
        value = [value];
    }
    if(Array.isArray(value) && typeof value[0] === "string")
    {
        const bytes = [];
        for(const s of value)
        {
            for(const b of new TextEncoder().encode(s)) bytes.push(b);
            bytes.push(0);
        }
        return new Uint8Array(bytes);
    }
    const cells = /** @type {!Array<number>} */ (value);
    const out = new Uint8Array(cells.length * 4);
    const view = new DataView(out.buffer);
    cells.forEach((c, i) => view.setUint32(i * 4, c >>> 0));
    return out;
}

/**
 * Serialise a tree rooted at `root` (whose name should be "").
 * @param {!FdtNode} root
 * @return {!Uint8Array}
 */
export function fdt_build(root)
{
    const strings = [];
    const string_offsets = new Map();
    let strings_len = 0;
    const structure = [];

    const u32 = v => structure.push((v >>> 24) & 0xFF, (v >>> 16) & 0xFF, (v >>> 8) & 0xFF, v & 0xFF);
    const pad = () => { while(structure.length & 3) structure.push(0); };

    function string_offset(name)
    {
        let off = string_offsets.get(name);
        if(off === undefined)
        {
            off = strings_len;
            string_offsets.set(name, off);
            const bytes = new TextEncoder().encode(name);
            strings.push(bytes);
            strings_len += bytes.length + 1;
        }
        return off;
    }

    function emit(node)
    {
        u32(FDT_BEGIN_NODE);
        for(const b of new TextEncoder().encode(node.name)) structure.push(b);
        structure.push(0);
        pad();
        for(const [name, value] of node.props)
        {
            const data = encode_value(value);
            u32(FDT_PROP);
            u32(data.length);
            u32(string_offset(name));
            for(const b of data) structure.push(b);
            pad();
        }
        node.children.forEach(emit);
        u32(FDT_END_NODE);
    }

    emit(root);
    u32(FDT_END);

    const header_size = 40;
    const rsvmap_off = header_size;
    const rsvmap_size = 16; // one terminating empty entry
    const struct_off = rsvmap_off + rsvmap_size;
    const strings_off = struct_off + structure.length;
    const total = strings_off + strings_len;

    const out = new Uint8Array(total);
    const view = new DataView(out.buffer);
    view.setUint32(0, FDT_MAGIC);
    view.setUint32(4, total);
    view.setUint32(8, struct_off);
    view.setUint32(12, strings_off);
    view.setUint32(16, rsvmap_off);
    view.setUint32(20, 17); // version
    view.setUint32(24, 16); // last compatible version
    view.setUint32(28, 0); // boot cpu
    view.setUint32(32, strings_len);
    view.setUint32(36, structure.length);
    out.set(structure, struct_off);
    let off = strings_off;
    for(const s of strings)
    {
        out.set(s, off);
        off += s.length + 1;
    }
    return out;
}
