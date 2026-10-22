#!/usr/bin/env python3
"""Puts each image of a site (build/a64-site, build/rv64-site) in a directory
named after its contents, and records the name in the manifest ("dir").

The files of an image keep their names from one version to the next (Image,
rootfs/0-1048576.squashfs, ...) and are cached for a day, so a browser that
had an earlier version could mix its cached parts with the new ones: a disk
assembled from two images, which the kernel finds corrupt. In a directory
named after the contents, a changed image is at new addresses entirely, and
an unchanged one keeps its cached parts.

Idempotent. An image whose manifest entry names an existing directory is left
alone; one in the flat layout add-image.sh writes is moved into its directory,
and any directory of an earlier version is removed.

    tools/version-images.py build/a64-site
"""
import hashlib
import json
import os
import shutil
import sys

PREFIX = "v-"


def content_hash(root, entries):
    h = hashlib.sha256()
    for entry in sorted(entries):
        path = os.path.join(root, entry)
        files = [path] if os.path.isfile(path) else sorted(
            os.path.join(d, f) for d, _, fs in os.walk(path) for f in fs)
        for f in files:
            h.update(os.path.relpath(f, root).encode() + b"\0")
            with open(f, "rb") as fh:
                for chunk in iter(lambda: fh.read(1 << 20), b""):
                    h.update(chunk)
    return h.hexdigest()[:12]


def main(site):
    images = os.path.join(site, "images")
    manifest_path = os.path.join(images, "manifest.json")
    with open(manifest_path) as f:
        manifest = json.load(f)
    changed = False
    for build in manifest["builds"]:
        root = os.path.join(images, build["id"])
        if not os.path.isdir(root):
            continue
        current = build.get("dir")
        flat = [e for e in os.listdir(root) if not e.startswith(PREFIX)]
        if current and os.path.isdir(os.path.join(root, current)) and not flat:
            continue
        if not flat:
            print(f"version-images: {build['id']}: no files", file=sys.stderr)
            continue
        version = PREFIX + content_hash(root, flat)
        target = os.path.join(root, version)
        for e in os.listdir(root):
            if e.startswith(PREFIX) and e != version:
                shutil.rmtree(os.path.join(root, e))
        os.makedirs(target, exist_ok=True)
        for e in flat:
            dest = os.path.join(target, e)
            if os.path.isdir(dest):
                shutil.rmtree(dest)
            os.replace(os.path.join(root, e), dest)
        build["dir"] = version
        changed = True
        print(f"version-images: {build['id']} -> {version}")
    if changed:
        with open(manifest_path, "w") as f:
            json.dump(manifest, f, indent=2)
            f.write("\n")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
