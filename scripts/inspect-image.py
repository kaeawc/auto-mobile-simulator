#!/usr/bin/env python3
"""Unpack what an Android emulator system image expects of the board, without booting it.

    scripts/inspect-image.py <sysdir> <outdir>

Writes into <outdir>: the image's own descriptors (advancedFeatures.ini, build.prop,
VerifiedBootParams.textproto), every cpio archive of the ramdisk (first-stage fstab and kernel
modules), and from the vendor partition the init scripts, VINTF fragments, input device
configuration (idc, keylayout) and kernel module list. Prints a summary to compare with
`emu -avd NAME -dry-run`: the enabled features, the fstab's devices, the modules, the input
device names and the vendor HALs.

Needs python3, lz4 (legacy-frame ramdisks), cpio, and debugfs (ext4) or fsck.erofs (erofs).
Never writes into <sysdir>.
"""
import os, re, shutil, struct, subprocess, sys

def gpt_partitions(path):
    with open(path, 'rb') as d:
        d.seek(512)
        h = d.read(92)
        if h[:8] != b'EFI PART':
            return []
        lba, n, size = struct.unpack_from('<QII', h, 72)
        d.seek(lba * 512)
        table = d.read(n * size)
    out = []
    for i in range(n):
        e = table[i * size:(i + 1) * size]
        if e[:16] == bytes(16):
            continue
        first, last = struct.unpack_from('<QQ', e, 32)
        out.append((e[56:128].decode('utf-16le').rstrip('\0'), first * 512, (last - first + 1) * 512))
    return out

def copy_range(src, off, size, dst):
    with open(src, 'rb') as s, open(dst, 'wb') as o:
        s.seek(off)
        while size:
            b = s.read(min(size, 8 << 20))
            if not b:
                break
            o.write(b)
            size -= len(b)

def fs_type(path):
    with open(path, 'rb') as f:
        b = f.read(4096)
    if b[1080:1082] == b'\x53\xef':
        return 'ext4'
    if b[1024:1028] == bytes.fromhex('e2e1f5e0'):
        return 'erofs'
    return None

def ramdisk_archives(ramdisk, out):
    raw = subprocess.run(['lz4', '-dc', ramdisk], capture_output=True, check=True).stdout if open(ramdisk, 'rb').read(4) != b'0707' else open(ramdisk, 'rb').read()
    parts, pos = [], 0
    while pos < len(raw):
        while pos < len(raw) and raw[pos] == 0:
            pos += 1
        t = raw.find(b'TRAILER!!!', pos)
        if pos >= len(raw) or t < 0:
            break
        end = (t + len('TRAILER!!!') + 1 + 3) & ~3
        parts.append(raw[pos:end])
        pos = end
    dirs = []
    for i, p in enumerate(parts):
        d = os.path.join(out, f'ramdisk.{i}')
        os.makedirs(d, exist_ok=True)
        subprocess.run(['cpio', '-idm', '--quiet', '--no-absolute-filenames'], input=p, cwd=d, check=False, stderr=subprocess.DEVNULL)
        dirs.append(d)
    return dirs

class Fs:
    """Reads files out of an ext4 (debugfs) or erofs (fsck.erofs --extract) partition image."""

    def __init__(self, image, scratch):
        self.image, self.kind = image, fs_type(image)
        self.root = None
        if self.kind == 'erofs':
            self.root = os.path.join(scratch, 'vendor.fs')
            shutil.rmtree(self.root, ignore_errors=True)
            subprocess.run(['fsck.erofs', f'--extract={self.root}', image], check=True, capture_output=True)

    def ls(self, path):
        if self.root:
            p = os.path.join(self.root, path.lstrip('/'))
            return sorted(os.listdir(p)) if os.path.isdir(p) else []
        r = subprocess.run(['debugfs', '-R', f'ls -p {path}', self.image], capture_output=True, text=True)
        names = [l.split('/')[5] for l in r.stdout.splitlines() if l.count('/') >= 6]
        return sorted(n for n in names if n not in ('.', '..'))

    def read(self, path):
        if self.root:
            p = os.path.join(self.root, path.lstrip('/'))
            return open(p, 'rb').read() if os.path.isfile(p) else b''
        return subprocess.run(['debugfs', '-R', f'cat {path}', self.image], capture_output=True).stdout

def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    sysdir, out = sys.argv[1], sys.argv[2]
    os.makedirs(out, exist_ok=True)
    for name in ('advancedFeatures.ini', 'build.prop', 'VerifiedBootParams.textproto', 'source.properties'):
        if os.path.isfile(os.path.join(sysdir, name)):
            shutil.copy(os.path.join(sysdir, name), out)

    print(f'== {sysdir}')
    features = open(os.path.join(sysdir, 'advancedFeatures.ini')).read() if os.path.isfile(os.path.join(sysdir, 'advancedFeatures.ini')) else ''
    on = [m.group(1) for m in re.finditer(r'^\s*(\w+)\s*=\s*on\b', features, re.M)]
    print('features  ' + ' '.join(on))

    for d in ramdisk_archives(os.path.join(sysdir, 'ramdisk.img'), out):
        for root, _, files in os.walk(d):
            for f in files:
                if f.startswith('fstab.'):
                    print(f'fstab     {os.path.relpath(os.path.join(root, f), out)}')
                    for line in open(os.path.join(root, f)):
                        if line.strip() and not line.startswith('#'):
                            print('  ' + ' '.join(line.split()[:3]))
                if f == 'modules.load':
                    print('modules   (first stage) ' + ' '.join(open(os.path.join(root, f)).read().split()))

    vendor_img = os.path.join(sysdir, 'vendor.img')
    parts = gpt_partitions(vendor_img)
    part = os.path.join(out, 'vendor.part')
    match = [p for p in parts if p[0] == 'vendor']
    if match:
        copy_range(vendor_img, match[0][1], match[0][2], part)
    else:
        shutil.copy(vendor_img, part)
    fs = Fs(part, out)
    print(f'vendor    {fs.kind}')
    vdir = os.path.join(out, 'vendor')
    for sub in ('etc/init', 'etc/init/hw', 'etc/vintf/manifest', 'usr/idc', 'usr/keylayout', 'lib/modules'):
        names = fs.ls('/' + sub)
        os.makedirs(os.path.join(vdir, sub), exist_ok=True)
        for n in names:
            if n.endswith(('.rc', '.xml', '.idc', '.kl', '.kcm')) or n in ('modules.load',):
                open(os.path.join(vdir, sub, n), 'wb').write(fs.read(f'/{sub}/{n}'))
    load = open(os.path.join(vdir, 'lib/modules/modules.load')).read().split() if os.path.isfile(os.path.join(vdir, 'lib/modules/modules.load')) else []
    print('modules   (vendor) ' + ' '.join(load))
    print('idc       ' + ' '.join(sorted(os.listdir(os.path.join(vdir, 'usr/idc')))))
    print('keylayout ' + ' '.join(sorted(os.listdir(os.path.join(vdir, 'usr/keylayout')))))
    print('hals      ' + ' '.join(sorted(n[:-4] for n in os.listdir(os.path.join(vdir, 'etc/vintf/manifest')))))

if __name__ == '__main__':
    main()
