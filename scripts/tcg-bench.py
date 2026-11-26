#!/usr/bin/env python3
"""Boot a system image under plain QEMU TCG (no KVM, no GPU) and measure the CPU translator.

    scripts/tcg-bench.py <sysdir> <workdir> [--boots N] [--cpu MODEL] [--qemu BIN] [--tb-size MB]
                         [--threads multi|single] [--append ARGS]

This is the software-mode CPU spike of issue #55: it measures what the translator alone costs,
before any of emu's devices are involved. QEMU gets stock's disk layout for the image's
architecture (x86_64: virtio-blk-pci at 00:03.0-00:07.0, `boot_devices=pci0000:00/0000:00:03.0`;
arm64: QEMU `virt`, whose virtio-mmio slots fill from 31 down as stock's do), a bootconfig like
emu's, and a root shell on a console (x86_64: ttyS0; arm64: hvc0, with logcat on hvc1 as stock).
There is no GPU, so SurfaceFlinger can't start and `sys.boot_completed` never comes; the
milestones are the shell and zygote's first start (`boot_progress_start`, in guest uptime).

Each boot then stops the framework and runs android-bench.sh's guest CPU microbenchmarks three
times: a 100k-iteration shell loop and four parallel md5sums of 64 MiB from /dev/zero. One line
per boot, then the medians. The first boot of a fresh <workdir> formats userdata and is not
counted (`--boots` counts the rest). Needs QEMU 11 or newer: older MTTCG (6.2) panics the guest
kernel in its live-patching (`int3` in `text_poke_bp`'s window).

With `--emu-devices <soft_devices binary> --lib64 <renderer lib64>` the guest gets emu's devices
instead (crates/emu-vmm/examples/soft_devices.rs: vsock, console, net and the GPU over vhost-user,
the goldfish address space and pipe over vfio-user) and the guest's graphics bootconfig, so it
boots to `sys.boot_completed`. Each boot then reports the time to it, the host CPU of QEMU and emu
over 60 s of idle after a 60 s settle (in cores), and both processes' peak RSS. `--icd` picks the
renderer's software Vulkan (swiftshader or lavapipe). x86_64 images only.

With `--stock <SDK emulator> --avd <name>` it boots that AVD under the SDK's own emulator in its
software mode instead (`-accel off`, which it refuses unless `ANDROID_I_WANT_MY_TCG=yes`; SwiftShader
for graphics, no snapshots), polls `sys.boot_completed` over adb, and reports the same idle windows
and the peak RSS of its process tree. The stock emulator writes into its AVD as it always does.

Writes only into <workdir>; never into <sysdir>.
"""
import argparse, os, re, shutil, socket, statistics, struct, subprocess, sys, time

# Every interval is timed on the monotonic clock: hosts that step their wall clock (an HTTP date
# sync on some cloud workers) would otherwise lengthen or shorten a boot mid-measurement.

# The best guest CPU per architecture on a 4-vCPU Xeon (2026-11-26, docs/benchmarks.md): `max`
# without the fast-string features, which steer the guest kernel's memcpy to `rep movsb` (a
# byte-loop helper under TCG); on arm64, pointer authentication and SVE off (emulated PAC costs
# 10-25% at boot).
DEFAULT_CPU = {
    'x86_64': 'max,-erms,-fsrm,-fzrm,-fsrs,-fsrc',
    'arm64': 'max,pauth=off,sve=off',
}

LOOP = 'i=0; while [ \\\\\\$i -lt 100000 ]; do i=\\\\\\$((i+1)); done'
MD5 = 'for j in 1 2 3 4; do (dd if=/dev/zero bs=1048576 count=64 2>/dev/null | md5sum >/dev/null) & done; wait'
# All three rounds in one command line: a second long line typed into arm64's virtio console
# after the first round's output was sometimes never read.
BENCH = f'su 0 sh -c "for r in 1 2 3; do time sh -c \\"{LOOP}\\"; time sh -c \\"{MD5}\\"; done" 2>&1; echo BDONE'
REAL = rb'(\d+)m(\d+\.\d+)s real'


def bootconfig(pairs):
    """Linux bootconfig (Documentation/admin-guide/bootconfig.rst), as emu-avd encodes it."""
    body = b''.join(k.encode() + b'="' + v.replace('"', "'").encode() + b'"\n' for k, v in pairs) + b'\0'
    body += b'\0' * (-len(body) % 4)
    return body + struct.pack('<II', len(body), sum(body) & 0xffffffff) + b'#BOOTCONFIG\n'


def vbmeta(sysdir):
    path = os.path.join(sysdir, 'VerifiedBootParams.textproto')
    if not os.path.exists(path):
        return []
    return [tuple(m.split('=', 1)) for m in re.findall(r'param: "([^"]+)"', open(path).read())]


# What emu gives an API 36 guest's graphics on ANGLE (apps/emu/src/instance.rs, `bootconfig` and
# `guest_graphics` for a `VulkanVirtualQueue` image).
GRAPHICS = [('androidboot.hardware.gltransport', 'virtio-gpu-asg'), ('androidboot.hardware.vulkan', 'ranchu'),
            ('androidboot.hardwareegl', 'angle'), ('androidboot.debug.hwui.renderer', 'skiavk'),
            ('androidboot.debug.renderengine.backend', 'skiavkthreaded'),
            ('androidboot.hardware.angle_feature_overrides_disabled', 'exposeN*'),
            ('androidboot.hardware.angle_feature_overrides_enabled', '')]


def prepare(sysdir, work, arch, fresh, graphics=False):
    os.makedirs(work, exist_ok=True)
    data = os.path.join(work, 'userdata.img')
    if fresh or not os.path.exists(data):
        for name, size in (('userdata.img', '4G'), ('cache.img', '66M')):
            p = os.path.join(work, name)
            if os.path.exists(p):
                os.remove(p)
            subprocess.run(['truncate', '-s', size, p], check=True)
            subprocess.run(['mkfs.ext4', '-q', '-F', p], check=True)
        subprocess.run(['cp', '--sparse=always', os.path.join(sysdir, 'encryptionkey.img'), os.path.join(work, 'encryptionkey.img')], check=True)
    boot_dev = 'pci0000:00/0000:00:03.0' if arch == 'x86_64' else 'a003600.virtio_mmio'
    console = 'ttyS0' if arch == 'x86_64' else 'hvc0'
    pairs = [('androidboot.hardware', 'ranchu'), ('androidboot.boot_devices', boot_dev), ('androidboot.qemu', '1'),
             ('androidboot.serialno', 'EMULATOR0EMURS0'), ('androidboot.veritymode', 'enforcing'),
             ('androidboot.dalvik.vm.heapsize', '576m'), ('androidboot.logcat', '*:S'), ('androidboot.qemu.skin', '720x1280'),
             ('androidboot.qemu.vsync', '60'), ('androidboot.opengles.version', '196609'), ('androidboot.console', console)]
    pairs += vbmeta(sysdir)
    if graphics:
        pairs += GRAPHICS
    with open(os.path.join(work, 'initrd'), 'wb') as o:
        o.write(open(os.path.join(sysdir, 'ramdisk.img'), 'rb').read() + bootconfig(pairs))


def qemu_args(a, sysdir, work, arch, sock, label):
    image_cmdline = open(os.path.join(sysdir, 'kernel_cmdline.txt')).read().strip()
    uart = 'ttyS0' if arch == 'x86_64' else 'ttyAMA0'
    cmdline = f'no_timer_check console={uart} ndns=2 loop.max_part=7 bootconfig mac80211_hwsim.radios=0 loglevel=1 {image_cmdline} {a.append}'
    accel = f'tcg,thread={a.threads}' + (f',tb-size={a.tb_size}' if a.tb_size else '')
    args = [a.qemu or f'qemu-system-{"x86_64" if arch == "x86_64" else "aarch64"}', '-nodefaults', '-no-user-config',
            '-display', 'none', '-accel', accel, '-cpu', a.cpu or DEFAULT_CPU[arch], '-smp', str(a.cores), '-m', str(a.memory),
            '-kernel', os.path.join(sysdir, 'kernel-ranchu'), '-initrd', os.path.join(work, 'initrd'), '-append', cmdline,
            '-chardev', f'socket,id=sh,path={sock},server=on,wait=off']
    S, W = (lambda n: os.path.join(sysdir, n)), (lambda n: os.path.join(work, n))
    # (file, id, read-only, stock's PCI slot on x86_64)
    disks = [(S('system.img'), 'system', True, 3), (W('cache.img'), 'cache', False, 4), (W('userdata.img'), 'userdata', False, 5),
             (W('encryptionkey.img'), 'metadata', False, 6), (S('vendor.img'), 'vendor', True, 7)]
    drive = lambda f, i, ro: ['-drive', f'file={f},if=none,id={i},format=raw,cache=unsafe,aio=threads' + (',snapshot=on' if ro else '')]
    if arch == 'x86_64':
        args += ['-M', 'pc', '-serial', 'chardev:sh']
        for f, i, ro, slot in disks:
            args += drive(f, i, ro) + ['-device', f'virtio-blk-pci,drive={i},addr={slot}']
        args += ['-device', 'virtio-rng-pci,addr=8']
    else:
        args += ['-M', 'virt,gic-version=3', '-chardev', f'file,id=k,path={W(label + ".kernel.log")}', '-serial', 'chardev:k']
        # virt hands out virtio-mmio slots from 31 down: vendor (31) first, system (27), net, rng.
        for f, i, ro, _ in reversed(disks):
            args += drive(f, i, ro) + ['-device', f'virtio-blk-device,drive={i}']
        args += ['-netdev', 'user,id=n', '-device', 'virtio-net-device,netdev=n', '-device', 'virtio-rng-device',
                 '-device', 'virtio-serial-device', '-device', 'virtconsole,chardev=sh',
                 '-chardev', f'file,id=l,path={W(label + ".logcat")}', '-device', 'virtconsole,chardev=l']
    return args


class Console:
    def __init__(self, path, log):
        self.s = None
        while self.s is None:
            try:
                self.s = socket.socket(socket.AF_UNIX)
                self.s.connect(path)
            except OSError:
                self.s = None
                time.sleep(0.05)
        self.s.settimeout(0.5)
        self.log, self.buf = open(log, 'wb'), b''

    def expect(self, pattern, timeout):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            try:
                d = self.s.recv(65536)
            except socket.timeout:
                d = b''
            if d:
                self.log.write(d)
                self.log.flush()
                self.buf += d
            m = re.search(pattern, self.buf)
            if m:
                self.buf = self.buf[m.end():]
                return m
        return None

    def send(self, line):
        self.s.sendall(line.encode() + b'\n')


def cpu_seconds(pid):
    """utime + stime of `pid`, in seconds."""
    with open(f'/proc/{pid}/stat') as f:
        fields = f.read().rsplit(')', 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf('SC_CLK_TCK')


def peak_rss_mb(pid):
    with open(f'/proc/{pid}/status') as f:
        return int(re.search(r'VmHWM:\s+(\d+)', f.read()).group(1)) // 1024


def boot_emu(a, sysdir, work, arch, label):
    """One boot on emu's devices: time to sys.boot_completed, then idle CPU and peak RSS."""
    import shlex
    sockdir = os.path.join(work, 'sockets')
    args_file = os.path.join(sockdir, 'qemu.args')
    if os.path.exists(args_file):
        os.remove(args_file)
    env = dict(os.environ, EMU_LIB64=a.lib64)
    if a.icd:
        icd = os.path.join(a.lib64, 'vulkan', 'vk_swiftshader_icd.json' if a.icd == 'swiftshader' else 'lvp_icd.json')
        env.update(ANDROID_EMU_VK_ICD=a.icd, VK_ICD_FILENAMES=icd, VK_DRIVER_FILES=icd)
    emu = subprocess.Popen([a.emu_devices, sockdir, str(a.adb_port), 'qemu.sf.lcd_density=320', 'qemu.hw.mainkeys=0'],
                           env=env, stdout=open(os.path.join(work, label + '.devices.log'), 'wb'), stderr=subprocess.STDOUT)
    for _ in range(100):
        if os.path.exists(args_file) and os.path.getsize(args_file) > 0:
            break
        time.sleep(0.1)
    sock = os.path.join(work, 'console.sock')
    if os.path.exists(sock):
        os.remove(sock)
    extra = ['-object', f'memory-backend-memfd,id=mem,size={a.memory}M,share=on', '-machine', 'memory-backend=mem']
    extra += shlex.split(open(args_file).read())
    t0 = time.monotonic()
    q = subprocess.Popen(qemu_args(a, sysdir, work, arch, sock, label) + extra)
    out = {'label': label}
    log = os.path.join(work, label + '.log')
    try:
        c = Console(sock, log)
        while time.monotonic() - t0 < a.budget and not c.expect(rb'console:/ \$', 2):
            c.send('')
        out['shell_s'] = round(time.monotonic() - t0, 1)
        done = None
        while time.monotonic() - t0 < a.budget and done is None:
            c.send('echo BOOT=$(getprop sys.boot_completed)')
            done = c.expect(rb'\nBOOT=1\r?\n', 2)
        if not done:
            out['failed'] = f'no sys.boot_completed within {a.budget} s'
            return out
        out['boot_completed_s'] = round(time.monotonic() - t0, 1)
        if q.poll() is None and emu.poll() is None:
            # Host CPU of QEMU and emu over 60-120 s after boot complete (the window the Mac rows
            # use) and over 240-300 s, once software emulation has finished its post-boot work.
            used = lambda: cpu_seconds(q.pid) + cpu_seconds(emu.pid)
            time.sleep(60)
            before = used()
            time.sleep(60)
            out['idle_cores'] = round((used() - before) / 60, 2)
            time.sleep(120)
            before = used()
            time.sleep(60)
            out['settled_cores'] = round((used() - before) / 60, 2)
            out['qemu_peak_rss_mb'], out['emu_peak_rss_mb'] = peak_rss_mb(q.pid), peak_rss_mb(emu.pid)
        out['emu_alive'] = emu.poll() is None
    except (BrokenPipeError, ConnectionResetError):
        # QEMU went away: a guest panic (its console says why) or a crash.
        tail = open(log, 'rb').read()[-4096:]
        panic = re.search(rb'Kernel panic - not syncing: ([^\r\n]*)', tail)
        out['failed'] = panic.group(1).decode(errors='replace') if panic else 'QEMU exited'
    finally:
        q.terminate()
        q.wait()
        emu.terminate()
        emu.wait()
    return out


def tree(pid):
    """`pid` and its descendants."""
    kids = {}
    for d in os.listdir('/proc'):
        if d.isdigit():
            try:
                with open(f'/proc/{d}/stat') as f:
                    kids.setdefault(int(f.read().rsplit(')', 1)[1].split()[1]), []).append(int(d))
            except OSError:
                pass
    out, todo = [], [pid]
    while todo:
        p = todo.pop()
        out.append(p)
        todo += kids.get(p, [])
    return out


def tree_total(pid, metric):
    total = 0
    for p in tree(pid):
        try:
            total += metric(p)
        except (OSError, AttributeError):
            pass
    return total


def boot_stock(a, label):
    """One boot of an AVD under the SDK emulator's software mode, measured as boot_emu measures."""
    serial = f'emulator-{a.stock_port}'
    adb = lambda *cmd: subprocess.run(['adb', '-s', serial, *cmd], capture_output=True, text=True, timeout=30).stdout.strip()
    env = dict(os.environ, ANDROID_I_WANT_MY_TCG='yes')
    log = open(os.path.join(a.workdir, label + '.stock.log'), 'wb')
    t0 = time.monotonic()
    e = subprocess.Popen([a.stock, '-avd', a.avd, '-port', str(a.stock_port), '-no-window', '-accel', 'off',
                          '-gpu', 'swiftshader_indirect', '-no-snapshot', '-no-audio', '-no-boot-anim', '-no-metrics'],
                         env=env, stdout=log, stderr=subprocess.STDOUT)
    out = {'label': label}
    try:
        done = False
        while time.monotonic() - t0 < a.budget and not done and e.poll() is None:
            try:
                done = adb('shell', 'getprop', 'sys.boot_completed') == '1'
            except subprocess.TimeoutExpired:
                pass
            if not done:
                time.sleep(2)
        if not done:
            out['failed'] = 'emulator exited' if e.poll() is not None else f'no sys.boot_completed within {a.budget} s'
            return out
        out['boot_completed_s'] = round(time.monotonic() - t0, 1)
        used = lambda: tree_total(e.pid, cpu_seconds)
        time.sleep(60)
        before = used()
        time.sleep(60)
        out['idle_cores'] = round((used() - before) / 60, 2)
        time.sleep(120)
        before = used()
        time.sleep(60)
        out['settled_cores'] = round((used() - before) / 60, 2)
        out['peak_rss_mb'] = tree_total(e.pid, peak_rss_mb)
    finally:
        try:
            adb('emu', 'kill')
            e.wait(60)
        except subprocess.TimeoutExpired:
            e.terminate()
            e.wait()
    return out


def boot(a, sysdir, work, arch, label):
    sock = os.path.join(work, 'console.sock')
    if os.path.exists(sock):
        os.remove(sock)
    t0 = time.monotonic()
    q = subprocess.Popen(qemu_args(a, sysdir, work, arch, sock, label))
    out = {'label': label}
    try:
        c = Console(sock, os.path.join(work, label + '.log'))
        while time.monotonic() - t0 < a.budget and not c.expect(rb'console:/ \$', 2):
            c.send('')
        out['shell_s'] = round(time.monotonic() - t0, 1)
        z = None
        while time.monotonic() - t0 < a.budget and z is None:
            c.send('logcat -b events -d | grep -m1 boot_progress_start | sed "s/.*: /ZYG=/"')
            z = c.expect(rb'\nZYG=(\d+)\r?\n', 5)
        out['zygote_uptime_s'] = round(int(z.group(1)) / 1000, 1) if z else None
        c.send('su 0 stop; echo STOPPED')
        c.expect(rb'\nSTOPPED', 60)
        time.sleep(5)
        c.send(BENCH)
        m = c.expect(rb'[\s\S]*\nBDONE', 900)
        times = [int(a) * 60 + float(b) for a, b in re.findall(REAL, m.group(0))] if m else []
        loops, md5s = times[0::2], times[1::2]
        out['loop100k_s'] = statistics.median(loops) if loops else None
        out['md5x4_s'] = statistics.median(md5s) if md5s else None
        with open(f'/proc/{q.pid}/status') as f:
            out['peak_rss_mb'] = int(re.search(r'VmHWM:\s+(\d+)', f.read()).group(1)) // 1024
    finally:
        q.terminate()
        q.wait()
    return out


def main():
    p = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    p.add_argument('sysdir')
    p.add_argument('workdir')
    p.add_argument('--boots', type=int, default=3)
    p.add_argument('--cpu')
    p.add_argument('--qemu')
    p.add_argument('--tb-size', type=int)
    p.add_argument('--threads', default='multi', choices=['multi', 'single'])
    p.add_argument('--append', default='')
    p.add_argument('--cores', type=int, default=4)
    p.add_argument('--memory', type=int, default=2048)
    p.add_argument('--budget', type=float, default=900, help='seconds per boot')
    p.add_argument('--emu-devices', help="emu's device server (soft_devices); boots to sys.boot_completed")
    p.add_argument('--lib64', help="the renderer's lib64, with --emu-devices")
    p.add_argument('--icd', choices=['swiftshader', 'lavapipe'], help="the renderer's software Vulkan")
    p.add_argument('--adb-port', type=int, default=5555)
    p.add_argument('--stock', help="the SDK's emulator binary: boot --avd under its software mode instead")
    p.add_argument('--avd', help='the AVD for --stock')
    p.add_argument('--stock-port', type=int, default=5560)
    a = p.parse_args()
    if a.stock:
        os.makedirs(a.workdir, exist_ok=True)
        print('first boot (not counted):', boot_stock(a, 'first'), flush=True)
        rows = []
        for n in range(a.boots):
            rows.append(boot_stock(a, f'boot{n + 1}'))
            print(rows[-1], flush=True)
        failed = [r for r in rows if 'failed' in r]
        rows = [r for r in rows if 'failed' not in r]
        print(f'failed boots: {len(failed)} of {len(failed) + len(rows)}', flush=True)
        med = lambda k: statistics.median([r[k] for r in rows if r.get(k) is not None] or [float('nan')])
        print(f'MEDIAN stock avd={a.avd} boots={len(rows)} ' +
              ' '.join(f'{k}={med(k):g}' for k in ('boot_completed_s', 'idle_cores', 'settled_cores', 'peak_rss_mb')), flush=True)
        return
    abi = re.search(r'abi=(\S+)', open(os.path.join(a.sysdir, 'source.properties')).read().replace('SystemImage.Abi', 'abi'))
    arch = 'x86_64' if abi and abi.group(1) == 'x86_64' else 'arm64'
    fresh = not os.path.exists(os.path.join(a.workdir, 'userdata.img'))
    prepare(a.sysdir, a.workdir, arch, fresh, graphics=bool(a.emu_devices))
    if a.emu_devices:
        if fresh:
            print('first boot (formats userdata, not counted):', boot_emu(a, a.sysdir, a.workdir, arch, 'first'), flush=True)
        rows = []
        for n in range(a.boots):
            rows.append(boot_emu(a, a.sysdir, a.workdir, arch, f'boot{n + 1}'))
            print(rows[-1], flush=True)
        failed = [r for r in rows if 'failed' in r]
        rows = [r for r in rows if 'failed' not in r]
        print(f'failed boots: {len(failed)} of {len(failed) + len(rows)}', flush=True)
        med = lambda k: statistics.median([r[k] for r in rows if r.get(k) is not None] or [float('nan')])
        print(f'MEDIAN emu devices icd={a.icd} boots={len(rows)} ' +
              ' '.join(f'{k}={med(k):g}' for k in ('shell_s', 'boot_completed_s', 'idle_cores', 'settled_cores', 'qemu_peak_rss_mb', 'emu_peak_rss_mb')), flush=True)
        return
    if fresh:
        print('first boot (formats userdata, not counted):', boot(a, a.sysdir, a.workdir, arch, 'first'), flush=True)
    rows = []
    for n in range(a.boots):
        rows.append(boot(a, a.sysdir, a.workdir, arch, f'boot{n + 1}'))
        print(rows[-1], flush=True)
    med = lambda k: statistics.median([r[k] for r in rows if r.get(k) is not None] or [float('nan')])
    print(f'MEDIAN arch={arch} cpu={a.cpu or DEFAULT_CPU[arch]} boots={len(rows)} ' +
          ' '.join(f'{k}={med(k):g}' for k in ('shell_s', 'zygote_uptime_s', 'loop100k_s', 'md5x4_s', 'peak_rss_mb')), flush=True)


if __name__ == '__main__':
    main()
