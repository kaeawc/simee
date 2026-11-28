#!/usr/bin/env python3
"""The host record and the result format every scripts/bench/ benchmark shares.

    python3 -I scripts/bench/summarize.py host                       # this host, as JSON
    python3 -I scripts/bench/summarize.py finish --name NAME --samples FILE --host-start FILE
        --host-end FILE --out FILE [--args TEXT] [--arms a,b] [--note TEXT]

    python3 -I scripts/bench/summarize.py check FILE...    # is each result publishable?

`host` records what a number depends on: the machine's model and chip, the OS and its version,
the CPU count (logical and physical), memory, the 1-minute load average (from `spectra load
-json` when spectra is installed, else the kernel's), the thermal state and power source (what
quiet.sh reads), and simee's commit (with `-dirty` for uncommitted changes), with the time.

`finish` reads the samples a benchmark recorded, one per line, tab-separated:
`round arm metric value load1`, and writes one JSON file (the host at the start and the end,
every sample, each arm's median per metric, and each measure's spread: p50, p95, IQR, standard
deviation and whether it is over its noise budget, scripts/bench/noise-budgets.tsv) and prints the
Markdown table the docs use: one row per metric, one column per arm, each cell the median with the
runs in brackets. A value that isn't a number (`never`, `timeout`) is kept as a run and left out of
the median. Beside the JSON it writes `<name>-<time>.host.json`: the host record (bench name and
version, rounds, commit, machine, load, thermal state and power at the start and the end, the
quiet gate's outcome) and whether the run is publishable: it passed the gate, ran three or more
rounds, has no noisy measure, and the host was not under thermal pressure or on battery at the
end. Only publishable results go into the published tables; `check` says which are.

Benchmarks written in Python load this file with importlib (python3 -I leaves the script's own
directory off sys.path), so there is one copy of the format.
"""
import datetime
import fnmatch
import hashlib
import json
import math
import os
import platform
import shutil
import statistics
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
HERE = os.path.dirname(os.path.abspath(__file__))
QUIET_SH = os.path.join(HERE, 'quiet.sh')
BUDGETS = os.path.join(HERE, 'noise-budgets.tsv')
MIN_ROUNDS = 3


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


def facts():
    """What quiet.sh reads off this host (thermal, power, ...) as a dict; empty when it can't run."""
    out = _run('bash', QUIET_SH, 'facts', timeout=60)
    return dict(line.split('=', 1) for line in out.splitlines() if '=' in line)


def quiet_gate(disabled=False):
    """The quiet-host gate for a benchmark written in Python: waits for a quiet host and sets
    BENCH_GATE_STATUS (which `finish` records), or exits with status 4 when the host stays busy.
    `disabled` is --no-quiet-gate."""
    if disabled:
        os.environ['BENCH_GATE_STATUS'] = 'disabled (--no-quiet-gate)'
        return os.environ['BENCH_GATE_STATUS']
    env = dict(os.environ, BENCH_QUIET_WHO=os.path.basename(sys.argv[0]))
    r = subprocess.run(['bash', QUIET_SH, 'gate'], env=env, stdout=subprocess.PIPE, text=True)
    if r.returncode:
        sys.exit(4)
    os.environ['BENCH_GATE_STATUS'] = r.stdout.strip()
    return os.environ['BENCH_GATE_STATUS']


def _physical_cores():
    cores = set()
    base = '/sys/devices/system/cpu'
    try:
        for d in os.listdir(base):
            if d.startswith('cpu') and d[3:].isdigit():
                try:
                    cores.add((open(f'{base}/{d}/topology/physical_package_id').read().strip(),
                               open(f'{base}/{d}/topology/core_id').read().strip()))
                except OSError:
                    pass
    except OSError:
        pass
    return len(cores) or os.cpu_count()


def host():
    h = {'date': datetime.datetime.now(datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')}
    if sys.platform == 'darwin':
        h['model'] = _run('sysctl', '-n', 'hw.model')
        h['cpu'] = _run('sysctl', '-n', 'machdep.cpu.brand_string')
        h['os'] = f"macOS {_run('sw_vers', '-productVersion')} ({_run('sw_vers', '-buildVersion')})"
        h['os_version'] = _run('sw_vers', '-productVersion')
        h['cpus'] = int(_run('sysctl', '-n', 'hw.ncpu') or 0)
        h['cores_physical'] = int(_run('sysctl', '-n', 'hw.physicalcpu') or 0)
        h['mem_gb'] = round(int(_run('sysctl', '-n', 'hw.memsize') or 0) / 2**30, 1)
    else:
        h['model'] = (open('/sys/devices/virtual/dmi/id/product_name').read().strip()
                      if os.path.exists('/sys/devices/virtual/dmi/id/product_name') else platform.machine())
        h['cpu'] = next((l.split(':', 1)[1].strip() for l in open('/proc/cpuinfo') if l.startswith('model name')), platform.machine())
        pretty = ''
        if os.path.exists('/etc/os-release'):
            pretty = next((l.split('=', 1)[1].strip().strip('"') for l in open('/etc/os-release') if l.startswith('PRETTY_NAME=')), '')
        h['os'] = f'{pretty or "Linux"} (kernel {platform.release()})'
        h['os_version'] = f'{pretty or "Linux"}, kernel {platform.release()}'
        h['cpus'] = os.cpu_count()
        h['cores_physical'] = _physical_cores()
        mem = next((int(l.split()[1]) for l in open('/proc/meminfo') if l.startswith('MemTotal:')), 0)
        h['mem_gb'] = round(mem / 2**20, 1)
        h['kvm'] = os.path.exists('/dev/kvm')
    h['load1'], h['load_source'] = load1()
    f = facts()
    h['thermal'] = f.get('thermal', 'unavailable')
    h['thermal_detail'] = f.get('thermal_detail', 'quiet.sh could not run')
    h['power'] = f.get('power', 'unknown')
    h['laptop'] = f.get('laptop') == '1'
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


def percentile(values, p):
    """The p-th percentile (0-100) by linear interpolation between closest ranks (numpy's default)."""
    v = sorted(values)
    if not v:
        raise ValueError('no values')
    k = (len(v) - 1) * p / 100
    lo = math.floor(k)
    hi = math.ceil(k)
    return v[lo] + (v[hi] - v[lo]) * (k - lo)


def iqr(values):
    return percentile(values, 75) - percentile(values, 25)


def noise_budget(metric, path=BUDGETS):
    """The largest IQR / p50 `metric` may show, or None when it has no budget. The first matching
    glob of the budgets file wins; BENCH_NOISE_BUDGET replaces them all."""
    env = os.environ.get('BENCH_NOISE_BUDGET')
    if env:
        return float(env)
    name = metric.lower()
    with open(path) as f:
        for line in f:
            if not line.strip() or line.startswith('#'):
                continue
            glob, _, budget = line.rstrip('\n').partition('\t')
            if fnmatch.fnmatchcase(name, glob.lower()):
                return None if budget.strip() == '-' else float(budget)
    return None


def spread(values, budget=None):
    """p50, p95, IQR, standard deviation and relative IQR of the numbers in `values`; `noisy` when
    there are MIN_ROUNDS or more and the relative IQR is over `budget`."""
    n = len(values)
    p50 = percentile(values, 50)
    q = iqr(values)
    rel = (q / abs(p50)) if p50 else (0.0 if q == 0 else math.inf)
    return {'n': n, 'p50': p50, 'p95': percentile(values, 95), 'iqr': q,
            'stddev': statistics.stdev(values) if n > 1 else 0.0,
            'rel_iqr': rel if math.isfinite(rel) else None, 'budget': budget,
            'noisy': bool(budget is not None and n >= MIN_ROUNDS and rel > budget)}


def spreads(samples, arms, metrics, budgets=BUDGETS):
    """{arm: {metric: spread}} over the numeric samples."""
    out = {}
    for arm in arms:
        for m in metrics:
            vals = [_num(str(s['value'])) for s in samples if s['arm'] == arm and s['metric'] == m]
            vals = [v for v in vals if v is not None]
            if vals:
                out.setdefault(arm, {})[m] = spread(vals, noise_budget(m, budgets))
    return out


def script_version(script):
    """`name@sha256[:12]` of the benchmark's own file: a changed method is a changed version."""
    try:
        with open(script, 'rb') as f:
            return f'{os.path.basename(script)}@{hashlib.sha256(f.read()).hexdigest()[:12]}'
    except OSError:
        return os.path.basename(script)


def record(name, host_start, host_end, rounds, args, gate, script, noisy):
    """The host record: the machine, its state around the run, what ran, whether it is publishable."""
    h, e = host_start, host_end
    reasons = []
    if not gate or gate.startswith('disabled'):
        reasons.append(f'the quiet gate did not run ({gate or "not recorded"})')
    if rounds < MIN_ROUNDS or '--quick' in args.split():
        reasons.append(f'{rounds} round{"s" * (rounds != 1)} (publishing needs {MIN_ROUNDS} or more, and --quick is a smoke test)')
    if noisy:
        reasons.append('noisy: ' + '; '.join(noisy))
    if e.get('thermal') == 'pressure':
        reasons.append(f"thermal pressure at the end of the run ({e.get('thermal_detail')})")
    if e.get('power') == 'battery':
        reasons.append('on battery power at the end of the run')
    return {
        'bench': name, 'bench_version': script_version(script), 'rounds': rounds, 'args': args,
        'date': h.get('date'), 'commit': h.get('simee_commit'),
        'model': h.get('model'), 'chip': h.get('cpu'), 'cores': h.get('cpus'),
        'cores_physical': h.get('cores_physical'), 'mem_gb': h.get('mem_gb'),
        'os': h.get('os'), 'os_version': h.get('os_version'), 'kvm': h.get('kvm'),
        'load1_start': h.get('load1'), 'load1_end': e.get('load1'), 'load_source': h.get('load_source'),
        'thermal_start': h.get('thermal'), 'thermal_end': e.get('thermal'),
        'thermal_detail': h.get('thermal_detail'),
        'power_start': h.get('power'), 'power_end': e.get('power'), 'laptop': h.get('laptop'),
        'quiet_gate': gate or 'not recorded',
        'publishable': not reasons, 'not_publishable': reasons,
    }


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
    spread_by_arm = spreads(samples, order_arms, order_metrics)
    noisy = [f"{m} ({arm}) IQR/p50 {sp['rel_iqr']:.2f} over its budget {sp['budget']:.2f}" if sp['rel_iqr'] is not None
             else f"{m} ({arm}) IQR over a median of 0"
             for arm, ms in spread_by_arm.items() for m, sp in ms.items() if sp['noisy']]
    rounds = max((int(s['round']) for s in samples), default=0)
    rec = record(name, host_start, host_end, rounds, args, os.environ.get('BENCH_GATE_STATUS', ''),
                 os.environ.get('BENCH_SCRIPT') or sys.argv[0], noisy)
    rec['noisy'] = noisy
    result = {'benchmark': name, 'args': args, 'note': note, 'host_start': host_start, 'host_end': host_end,
              'record': rec, 'arms': order_arms, 'medians': medians, 'spread': spread_by_arm, 'samples': samples}
    os.makedirs(os.path.dirname(os.path.abspath(out)), exist_ok=True)
    with open(out, 'w') as f:
        json.dump(result, f, indent=1)
        f.write('\n')
    with open(out[:-5] + '.host.json' if out.endswith('.json') else out + '.host.json', 'w') as f:
        json.dump(rec, f, indent=1)
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
    lines.append(f"Quiet gate: {rec['quiet_gate']}; thermal {rec['thermal_start']}, power {rec['power_start']}.")
    if noisy:
        lines.append('NOISY, spread over budget (keep out of the published tables): ' + '; '.join(noisy))
    if rec['not_publishable']:
        lines.append('NOT PUBLISHABLE: ' + '; '.join(rec['not_publishable']))
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


def check(paths):
    """Prints whether each result file is publishable; the status is 1 when any isn't."""
    bad = 0
    for path in paths:
        try:
            with open(path) as f:
                data = json.load(f)
            rec = data.get('record', data)
            ok, why = rec['publishable'], rec['not_publishable']
        except (OSError, ValueError, KeyError) as e:
            ok, why = False, [f'no host record ({e})']
        print(f'{"publishable    " if ok else "NOT publishable"} {path}' + ('' if ok else ': ' + '; '.join(why)))
        bad += not ok
    return 1 if bad else 0


def main():
    if len(sys.argv) < 2 or sys.argv[1] in ('-h', '--help'):
        print(__doc__.strip())
        return 0
    if sys.argv[1] == 'host':
        print(json.dumps(host()))
        return 0
    if sys.argv[1] == 'check':
        return check(sys.argv[2:])
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
