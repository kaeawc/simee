#!/usr/bin/env python3
"""iOS API method latency through `simee serve`: round trips over one persistent connection.

    python3 -I scripts/bench/ios-api.py --suite NAME [--calls N] [--baseline SIMEE] [--simee BIN]
        [--device-type TYPE] [--runtime ID] [--bundle ID] [--rounds N] [--quick] [--no-quiet-gate] [--out DIR]
    python3 -I scripts/bench/ios-api.py --list

Creates a simulator (`simctl create`, booted with `simctl bootstatus -b`, 30 s to quieten), and
each round, for each build in turn (this checkout's release simee, and --baseline's for a
before/after, alternating), starts that build's `simee serve` on a private socket, runs the
suite over one connection and stops the server. Each method's number is the median of --calls
round trips (default 10, after one warm-up), and the table gives the median over rounds. The
device is shut down and deleted at the end; nothing else is touched.

Suites (what docs/benchmarks.md and the docs it links publish):
  lookup         location, screen                      "Per-call device lookup on a crowded host"
  introspection  processes, connections, network_state, crash_reports, and kill of a
                 just-launched Settings                "Introspection"
  launch         launch of Settings in each state of the published table   "launch, with and
                 without restart"
  input          tap, button home, key, type 10 / 20 characters, swipe 300 ms   "iOS input
                 defaults" (the timings; recognition needs the test app, which isn't in the repo)
  prefs          prefs_stores, prefs_read of every store and of one, prefs_read fresh, for
                 --bundle                               docs/ios.md's preferences table
  mem            mem_vmmap, mem_heap, mem_leaks of a running Settings (median of --calls 3)
                 docs/api.md's mem_* costs
  pointer        an iPad's `pointer` click: the first click of a fresh server (it homes) and the
                 later ones, at targets spread over the screen with Settings in front (an iPad
                 device type is used)    "iPad pointer click" (timings only; placement needs the
                 test app)

Defaults: --device-type "iPhone 17 Pro" ("iPad Pro 13-inch (M5)" for pointer), --runtime the
newest iOS runtime, --bundle com.apple.Preferences. --quick: one round, 3 calls.
"""
import argparse, atexit, importlib.util, json, os, shutil, signal, socket, statistics, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.dont_write_bytecode = True  # no __pycache__ beside the scripts
_spec = importlib.util.spec_from_file_location('summarize', os.path.join(HERE, 'summarize.py'))
summarize = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(summarize)

SETTINGS = 'com.apple.Preferences'
SUITES = ['lookup', 'introspection', 'launch', 'input', 'prefs', 'mem', 'pointer']
cleanups, procs = [], []


def log(msg):
    print(f'[{time.strftime("%H:%M:%S")}] {msg}', file=sys.stderr, flush=True)


def cleanup():
    for p in procs:
        if p.poll() is None:
            p.send_signal(signal.SIGINT)
            try:
                p.wait(30)
            except subprocess.TimeoutExpired:
                p.terminate()
                p.wait(10)
    while cleanups:
        try:
            cleanups.pop()()
        except Exception as e:  # cleanup must keep going
            log(f'cleanup: {e}')


atexit.register(cleanup)
signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))


def simctl(*args, check=True):
    return subprocess.run(['xcrun', 'simctl', *args], capture_output=True, text=True, check=check).stdout.strip()


def newest_runtime():
    rs = [r for r in json.loads(simctl('list', 'runtimes', '-j'))['runtimes']
          if r.get('isAvailable') and '.iOS-' in r['identifier']]
    rs.sort(key=lambda r: [int(x) for x in r['version'].split('.')])
    return rs[-1]['identifier'] if rs else None


class Client:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(path)
        self.r = self.s.makefile('rb', buffering=1 << 20)
        self.id = 0

    def call(self, method, **params):
        self.id += 1
        self.s.sendall((json.dumps({'id': self.id, 'method': method, 'params': params}) + '\n').encode())
        r = json.loads(self.r.readline())
        if not r.get('ok'):
            raise RuntimeError(f'{method}: {r.get("error", r)}')
        return r['result']

    def close(self):
        self.s.close()


def timed(fn):
    t = time.perf_counter()
    fn()
    return (time.perf_counter() - t) * 1000


def med(fn, n, warm=True):
    if warm:
        fn()
    return statistics.median([timed(fn) for _ in range(n)])


def serve(simee, path):
    env = {k: v for k, v in os.environ.items() if not k.startswith('SIMEE_')}
    env['SIMEE_SOCKET'] = path
    p = subprocess.Popen([simee, 'serve'], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    procs.append(p)
    for _ in range(100):
        if os.path.exists(path):
            return p
        time.sleep(0.1)
    raise SystemExit(f'simee serve did not open {path}')


def stop(p):
    p.send_signal(signal.SIGINT)
    try:
        p.wait(30)
    except subprocess.TimeoutExpired:
        p.terminate()
        p.wait(10)
    procs.remove(p)


def run_suite(c, suite, dev, n, bundle):
    """Yields (metric, ms) for one round."""
    if suite == 'lookup':
        yield 'location, ms', med(lambda: c.call('location', device=dev, latitude=37.3349, longitude=-122.009), n)
        yield 'screen, ms', med(lambda: c.call('screen', device=dev), n)
    elif suite == 'introspection':
        for m in ('processes', 'connections', 'network_state', 'crash_reports'):
            yield f'{m}, ms', med(lambda m=m: c.call(m, device=dev), n)
        ts = []
        for _ in range(n):
            pid = c.call('launch', device=dev, bundle_id=SETTINGS)['pid']
            time.sleep(1.0)
            ts.append(timed(lambda: c.call('kill', device=dev, pid=pid)))
            time.sleep(0.5)
        yield 'kill of a just-launched app, ms', statistics.median(ts)
    elif suite == 'launch':
        def case(prep):
            ts = []
            for _ in range(n):
                restart = prep()
                ts.append(timed(lambda: c.call('launch', device=dev, bundle_id=SETTINGS, restart=restart)))
            return statistics.median(ts)

        def not_running():
            c.call('terminate', device=dev, bundle_id=SETTINGS); time.sleep(1); return True

        def after_terminate():
            c.call('terminate', device=dev, bundle_id=SETTINGS); return True

        def running(restart, background=False):
            def prep():
                c.call('launch', device=dev, bundle_id=SETTINGS); time.sleep(4)
                if background:
                    c.call('button', device=dev, button='home'); time.sleep(1)
                return restart
            return prep

        def after_cold_launch():
            c.call('terminate', device=dev, bundle_id=SETTINGS); time.sleep(1)
            c.call('launch', device=dev, bundle_id=SETTINGS); time.sleep(1); return False

        yield 'not running (1 s after terminate), ms', case(not_running)
        yield 'right after terminate, ms', case(after_terminate)
        yield 'running, restart true, ms', case(running(True))
        yield 'running in the background, restart false, ms', case(running(False, background=True))
        yield 'running in front and settled, restart false, ms', case(running(False))
        yield '1 s after a cold launch, restart false, ms', case(after_cold_launch)
    elif suite == 'input':
        c.call('launch', device=dev, bundle_id=SETTINGS); time.sleep(3)
        yield 'tap (default hold), ms', med(lambda: c.call('tap', device=dev, x=600, y=2500), n)
        yield 'button home (default hold), ms', med(lambda: c.call('button', device=dev, button='home'), n)
        yield 'key, ms', med(lambda: c.call('key', device=dev, usage=4), n)
        yield 'type, 10 characters, ms', med(lambda: c.call('type', device=dev, text='abcdefghij'), n)
        yield 'type, 20 characters, ms', med(lambda: c.call('type', device=dev, text='abcdefghijklmnopqrst'), n)
        yield 'swipe, duration_ms 300, ms', med(lambda: c.call('swipe', device=dev, x1=600, y1=1800, x2=600, y2=1300, duration_ms=300), n)
    elif suite == 'prefs':
        c.call('launch', device=dev, bundle_id=bundle); time.sleep(2)
        stores = c.call('prefs_stores', device=dev, bundle_id=bundle)
        ids = [s.get('id') for s in (stores if isinstance(stores, list) else stores.get('stores', []))]
        yield 'prefs_stores, warm, ms', med(lambda: c.call('prefs_stores', device=dev, bundle_id=bundle), n)
        yield 'prefs_read of every store, ms', med(lambda: c.call('prefs_read', device=dev, bundle_id=bundle), n)
        if ids and ids[0]:
            yield 'prefs_read of one store, ms', med(lambda: c.call('prefs_read', device=dev, bundle_id=bundle, store=ids[0]), n)
        yield 'prefs_read fresh, ms', med(lambda: c.call('prefs_read', device=dev, bundle_id=bundle, fresh=True), min(n, 5))
    elif suite == 'mem':
        c.call('launch', device=dev, bundle_id=SETTINGS); time.sleep(3)
        for m in ('mem_vmmap', 'mem_heap', 'mem_leaks'):
            yield f'{m}, ms', med(lambda m=m: c.call(m, device=dev, bundle_id=SETTINGS), max(1, min(n, 3)), warm=False)
    elif suite == 'pointer':
        size = c.call('screen', device=dev)
        w, h = size.get('width', 2064), size.get('height', 2752)
        targets = [(int(w * (0.15 + 0.7 * ((i * 7) % 20) / 19)), int(h * (0.15 + 0.7 * ((i * 11) % 20) / 19))) for i in range(20)]
        c.call('launch', device=dev, bundle_id=SETTINGS); time.sleep(3)
        x, y = targets[0]
        yield 'first click (homes), ms', timed(lambda: c.call('pointer', device=dev, x=x, y=y, action='click'))
        later = [timed(lambda t=t: c.call('pointer', device=dev, x=t[0], y=t[1], action='click')) for t in targets[1:1 + max(3, n // 3)]]
        yield 'later click, ms', statistics.median(later)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter, usage=argparse.SUPPRESS)
    p.add_argument('--suite', choices=SUITES)
    p.add_argument('--list', action='store_true')
    p.add_argument('--calls', type=int)
    p.add_argument('--baseline')
    p.add_argument('--simee')
    p.add_argument('--device-type')
    p.add_argument('--runtime')
    p.add_argument('--bundle', default=SETTINGS)
    p.add_argument('--rounds', type=int, default=3)
    p.add_argument('--quick', action='store_true')
    p.add_argument('--no-quiet-gate', action='store_true', help='skip the quiet-host gate (a smoke run; its numbers are not published)')
    p.add_argument('--out', default=os.path.join(REPO, 'bench', 'results'))
    a = p.parse_args()
    if a.list:
        print('\n'.join(SUITES))
        return 0
    if not a.suite:
        p.error('--suite is required (--list names them)')
    if sys.platform != 'darwin' or subprocess.run(['xcrun', 'simctl', 'help'], capture_output=True).returncode:
        print('ios-api.py: skipped: iOS needs macOS with Xcode', file=sys.stderr)
        return 3
    rounds = 1 if a.quick else a.rounds
    n = a.calls or (3 if a.quick else 10)
    runtime = a.runtime or newest_runtime()
    if not runtime:
        print('ios-api.py: skipped: no iOS simulator runtime installed', file=sys.stderr)
        return 3
    dtype = a.device_type or ('iPad Pro 13-inch (M5)' if a.suite == 'pointer' else 'iPhone 17 Pro')
    summarize.quiet_gate(a.no_quiet_gate)
    host_start = summarize.host()
    simee = a.simee
    if not simee:
        log('building target/release/simee')
        subprocess.run(['cargo', 'build', '-q', '--release', '-p', 'simee'], cwd=REPO, check=True)
        simee = os.path.join(REPO, 'target', 'release', 'simee')
    builds = [simee] + ([a.baseline] if a.baseline else [])
    label = (lambda b: 'simee') if not a.baseline else (lambda b: 'after' if b == simee else 'before')
    udid = simctl('create', f'simee-bench-api-{a.suite}-{os.getpid()}', dtype, runtime)
    cleanups.append(lambda: (simctl('shutdown', udid, check=False), simctl('delete', udid, check=False), log(f'deleted {udid}')))
    log(f'created {dtype} {runtime} {udid}')
    simctl('bootstatus', udid, '-b')
    time.sleep(10 if a.quick else 30)
    sockdir = tempfile.mkdtemp(prefix='sb.', dir='/tmp')
    cleanups.append(lambda: shutil.rmtree(sockdir, ignore_errors=True))
    samples = []
    for r in range(1, rounds + 1):
        order = builds if r % 2 or len(builds) == 1 else builds[::-1]
        for b in order:
            path = os.path.join(sockdir, f'{label(b)}.sock')
            if os.path.exists(path):
                os.unlink(path)
            srv = serve(b, path)
            c = Client(path)
            try:
                for metric, ms in run_suite(c, a.suite, udid, n, a.bundle):
                    log(f'round {r} {label(b)} {metric}={ms:.2f}')
                    samples.append({'round': r, 'arm': label(b), 'metric': metric, 'value': round(ms, 2), 'load1': round(os.getloadavg()[0], 2)})
            except RuntimeError as e:
                log(f'round {r} {label(b)}: {e}')
                samples.append({'round': r, 'arm': label(b), 'metric': 'error', 'value': 'failed', 'load1': None})
            c.close()
            stop(srv)
    if not samples:
        return 1
    out = os.path.join(a.out, f'ios-api-{a.suite}-{time.strftime("%Y%m%d-%H%M%S")}.json')
    print(summarize.finish(f'ios-api {a.suite}', samples, host_start, summarize.host(), out, ' '.join(sys.argv[1:]),
                           ['before', 'after', 'simee'], f'{dtype}, {runtime}; median of {n} round trips per method and round, one connection.'))
    return 0


if __name__ == '__main__':
    sys.exit(main())
