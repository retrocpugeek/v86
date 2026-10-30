# QEMU -d exec,cpu,nochain log (one instruction per block) -> per executed
# instruction: "pc r0..r14 nzcv" (the state before it), dropping entries QEMU
# stopped before executing.
import re, sys
limit = int(sys.argv[2])
out = sys.stdout
n = 0
cur = None      # (pc, [regs], xpsr) being collected
pend = None
regs = {}
def flush():
    global n, pend
    if pend:
        out.write(pend); n += 1
        if n >= limit: sys.exit(0)
    pend = None
for line in open(sys.argv[1], errors="replace"):
    if line.startswith("Trace "):
        flush()
        m = re.search(r"\[[0-9a-f]+/([0-9a-f]+)/", line)
        cur = int(m.group(1), 16); regs = {}
    elif line.startswith("R") and "=" in line and cur is not None:
        for k, v in re.findall(r"R(\d\d)=([0-9a-f]{8})", line):
            regs[int(k)] = v
    elif line.startswith("XPSR=") and cur is not None:
        x = int(line[5:13], 16)
        pend = "%08x %s %x\n" % (cur, " ".join(regs[i] for i in range(15)), x >> 28)
        cur = None
    elif line.startswith("Stopped execution of TB chain before"):
        m = re.search(r"\[([0-9a-f]+)\]", line)
        if pend and m and int(m.group(1), 16) == int(pend[:8], 16):
            pend = None
flush()
