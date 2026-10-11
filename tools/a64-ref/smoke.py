#!/usr/bin/env python3
"""Boot a guest on the serial console, log in, run commands, power off.

    smoke.py [--qemu] Image dtb rootfs.ext4 [command ...]

Without --qemu it drives src/a64's a64run; with --qemu, the reference
qemu-system-aarch64 (same DTB), so the two transcripts can be compared.
"""
import os
import select
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def main():
    args = sys.argv[1:]
    use_qemu = args and args[0] == "--qemu"
    if use_qemu:
        args = args[1:]
    if len(args) < 3:
        sys.exit(__doc__)
    image, dtb, disk = args[:3]
    cmds = args[3:] or ["uname -a", "cat /proc/cpuinfo", "ls /", "echo smoke-ok"]
    if use_qemu:
        cmd = ["qemu-system-aarch64", "-M", "virt", "-cpu", "cortex-a53", "-smp", "1", "-m", "128",
               "-nographic", "-monitor", "none", "-kernel", image, "-dtb", dtb,
               "-global", "virtio-mmio.force-legacy=false",
               "-drive", f"file={disk},if=none,format=raw,id=hd0,snapshot=on",
               "-device", "virtio-blk-device,drive=hd0"]
    else:
        cmd = [os.path.join(ROOT, "src/a64/target/release/a64run"), "--kernel", image, "--dtb", dtb,
               "--disk", disk]
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    out = b""
    t0 = time.time()

    def wait(pat, timeout):
        nonlocal out
        end = time.time() + timeout
        while time.time() < end:
            r, _, _ = select.select([p.stdout], [], [], 0.5)
            if r:
                d = os.read(p.stdout.fileno(), 65536)
                if not d:
                    return False
                sys.stdout.buffer.write(d)
                sys.stdout.flush()
                out += d
                if pat in out:
                    return True
        return False

    def send(s):
        p.stdin.write(s.encode() + b"\n")
        p.stdin.flush()

    ok = wait(b"login:", 600)
    print(f"\n[smoke] login prompt: {ok} after {time.time() - t0:.1f}s", file=sys.stderr)
    if ok:
        send("root")
        ok = wait(b"# ", 120)
        for c in cmds:
            out = b""
            send(c)
            ok = wait(b"# ", 300) and ok
        send("poweroff")
        wait(b"reboot: Power down", 120)
    p.kill()
    p.wait()
    print(f"[smoke] {'PASS' if ok else 'FAIL'} in {time.time() - t0:.1f}s", file=sys.stderr)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
