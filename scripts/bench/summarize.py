#!/usr/bin/env python3
"""The host record and the result format every scripts/bench/ benchmark shares.

    python3 -I scripts/bench/summarize.py host                       # this host, as JSON
    python3 -I scripts/bench/summarize.py finish --name NAME --samples FILE --host-start FILE
        --host-end FILE --out FILE [--args TEXT] [--arms a,b] [--note TEXT]

`host` records what a number depends on: the machine's model and CPU, the OS version, the CPU
count, memory, the 1-minute load average (from `spectra load -json` when spectra is installed,
else the kernel's), and simee's commit (with `-dirty` for uncommitted changes), with the time.

`finish` reads the samples a benchmark recorded, one per line, tab-separated:
`round arm metric value load1`, and writes one JSON file (the host at the start and the end,
every sample, and each arm's median per metric) and prints the Markdown table the docs use: one
row per metric, one column per arm, each cell the median with the runs in brackets. A value
that isn't a number (`never`, `timeout`) is kept as a run and left out of the median.

Benchmarks written in Python load this file with importlib (python3 -I leaves the script's own
directory off sys.path), so there is one copy of the format.
"""
import datetime
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def _run(*cmd, timeout=20):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return ''


def load1():
    """The 1-minute load average and where it came from."""
    if shutil.which('spectra'):
        try:
            return float(json.loads(_run('spectra', 'load', '-json', timeout=30))['load1']), 'spectra'
        except (ValueError, KeyError, TypeError):
            pass
    return round(os.getloadavg()[0], 2), 'loadavg'


def host():
    h = {'date': datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')}
    if sys.platform == 'darwin':
        h['model'] = _run('sysctl', '-n', 'hw.model')
        h['cpu'] = _run('sysctl', '-n', 'machdep.cpu.brand_string')
        h['os'] = f"macOS {_run('sw_vers', '-productVersion')} ({_run('sw_vers', '-buildVersion')})"
        h['cpus'] = int(_run('sysctl', '-n', 'hw.ncpu') or 0)
        h['mem_gb'] = round(int(_run('sysctl', '-n', 'hw.memsize') or 0) / 2**30, 1)
    else:
        h['model'] = (open('/sys/devices/virtual/dmi/id/product_name').read().strip()
                      if os.path.exists('/sys/devices/virtual/dmi/id/product_name') else platform.machine())
        h['cpu'] = next((l.split(':', 1)[1].strip() for l in open('/proc/cpuinfo') if l.startswith('model name')), platform.machine())
        pretty = ''
        if os.path.exists('/etc/os-release'):
            pretty = next((l.split('=', 1)[1].strip().strip('"') for l in open('/etc/os-release') if l.startswith('PRETTY_NAME=')), '')
        h['os'] = f'{pretty or "Linux"} (kernel {platform.release()})'
        h['cpus'] = os.cpu_count()
        mem = next((int(l.split()[1]) for l in open('/proc/meminfo') if l.startswith('MemTotal:')), 0)
        h['mem_gb'] = round(mem / 2**20, 1)
        h['kvm'] = os.path.exists('/dev/kvm')
    h['load1'], h['load_source'] = load1()
    commit = _run('git', '-C', REPO, 'rev-parse', '--short', 'HEAD')
    if commit and _run('git', '-C', REPO, 'status', '--porcelain', '--untracked-files=no'):
        commit += '-dirty'
    h['simee_commit'] = commit or 'unknown'
    return h


def _num(v):
    try:
        return float(v)
    except ValueError:
        return None


def _fmt(v):
    return f'{v:.3f}'.rstrip('0').rstrip('.') if isinstance(v, float) else str(v)


def finish(name, samples, host_start, host_end, out, args='', arms=None, note=''):
    """Writes the JSON result to `out` and returns the Markdown table. `samples` are dicts with
    round, arm, metric, value (a number or a word) and load1."""
    order_arms, order_metrics = [], []
    for s in samples:
        if s['arm'] not in order_arms:
            order_arms.append(s['arm'])
        if s['metric'] not in order_metrics:
            order_metrics.append(s['metric'])
    if arms:
        order_arms = [a for a in arms if a in order_arms] + [a for a in order_arms if a not in arms]
    medians = {}
    for arm in order_arms:
        for m in order_metrics:
            vals = [_num(str(s['value'])) for s in samples if s['arm'] == arm and s['metric'] == m]
            vals = [v for v in vals if v is not None]
            if vals:
                medians.setdefault(arm, {})[m] = statistics.median(vals)
    result = {'benchmark': name, 'args': args, 'note': note, 'host_start': host_start, 'host_end': host_end,
              'arms': order_arms, 'medians': medians, 'samples': samples}
    os.makedirs(os.path.dirname(os.path.abspath(out)), exist_ok=True)
    with open(out, 'w') as f:
        json.dump(result, f, indent=1)
        f.write('\n')
    lines = [f'| Measure | {" | ".join(order_arms)} |', '| --- |' + ' --- |' * len(order_arms)]
    for m in order_metrics:
        cells = []
        for arm in order_arms:
            runs = [str(s['value']) for s in samples if s['arm'] == arm and s['metric'] == m]
            if not runs:
                cells.append('')
                continue
            med = medians.get(arm, {}).get(m)
            head = _fmt(med) if med is not None else runs[0]
            cells.append(head if len(runs) == 1 else f'{head} ({", ".join(_fmt(_num(r)) if _num(r) is not None else r for r in runs)})')
        lines.append(f'| {m} | {" | ".join(cells)} |')
    h = host_start
    loads = [x.get('load1') for x in (host_start, host_end) if x.get('load1') is not None]
    lines.append('')
    lines.append(f"{name}: {h.get('model')} ({h.get('cpu')}, {h.get('cpus')} CPUs, {h.get('mem_gb')} GB), {h.get('os')}, "
                 f"simee {h.get('simee_commit')}, {h.get('date')}, 1-minute load {'-'.join(_fmt(float(l)) for l in loads)} "
                 f"({h.get('load_source')}); medians of {max((int(s['round']) for s in samples), default=0)} rounds. "
                 f"JSON: {out}")
    if note:
        lines.append(note)
    return '\n'.join(lines)


def read_samples(path):
    samples = []
    with open(path) as f:
        for line in f:
            parts = line.rstrip('\n').split('\t')
            if len(parts) < 4:
                continue
            r, arm, metric, value = parts[:4]
            v = _num(value)
            samples.append({'round': int(r), 'arm': arm, 'metric': metric,
                            'value': value if v is None else (int(v) if v.is_integer() and '.' not in value else v),
                            'load1': _num(parts[4]) if len(parts) > 4 and parts[4] else None})
    return samples


def main():
    if len(sys.argv) < 2 or sys.argv[1] in ('-h', '--help'):
        print(__doc__.strip())
        return 0
    if sys.argv[1] == 'host':
        print(json.dumps(host()))
        return 0
    if sys.argv[1] != 'finish':
        print(f'unknown command {sys.argv[1]}', file=sys.stderr)
        return 2
    opts, rest = {}, sys.argv[2:]
    while rest:
        k = rest.pop(0)
        if not k.startswith('--') or not rest:
            print(f'bad argument {k}', file=sys.stderr)
            return 2
        opts[k[2:]] = rest.pop(0)
    samples = read_samples(opts['samples'])
    if not samples:
        print(f"{opts['name']}: no samples recorded", file=sys.stderr)
        return 1
    hs = json.load(open(opts['host-start']))
    he = json.load(open(opts['host-end'])) if os.path.exists(opts.get('host-end', '')) else hs
    arms = [a for a in opts.get('arms', '').split(',') if a]
    print(finish(opts['name'], samples, hs, he, opts['out'], opts.get('args', ''), arms, opts.get('note', '')))
    return 0


if __name__ == '__main__':
    sys.exit(main())
