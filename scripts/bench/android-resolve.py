#!/usr/bin/env python3
"""Android device resolution: the time a call spends finding its device (#256).

    python3 -I scripts/bench/android-resolve.py [--avds N] [--running N] [--calls N] [--baseline SIMEE]
        [--simee BIN] [--rounds N] [--quick] [--no-quiet-gate] [--out DIR]
    python3 -I scripts/bench/android-resolve.py --make-home DIR [--avds N]

Builds a private SDK and AVD home with --avds AVDs (default 44: phones, tablets, watches, TVs and
cars, each with the config.ini avdmanager writes), and with --running of them (default 2) held
by a stand-in for the stock emulator: a process whose command line is a qemu's (`-avd NAME -port
N`, N from 5900 up, outside the 5554-5682 range a farm's consoles use) and that holds the AVD's
`hardware-qemu.ini.lock`. No emulator, hypervisor or SDK is needed.
Every call is timed --calls times (default 100), and the table gives p50 and p95 in ms, each
the median over rounds, with the host and the load recorded.

Each round, for each build in turn (this checkout's release simee, and --baseline's for a
before/after, alternating), on a home with nothing running and then on one with --running
emulators:
  kept       over one `simee serve` connection: `devices`, `devices` with booted, `info` of an exact
             AVD name, `info` of `booted`
  one-shot   `simee list`, `simee list --booted`, `simee tap NAME 1 1` and `simee tap booted 1 1`
             as a fresh process each (no server on the socket). The failure is what is timed:
             a tap of a name fails after the device is resolved, before any network I/O (a stopped
             AVD isn't running; a stand-in is a stock emulator, which refuses input over the
             display socket). `booted` fails inside resolution: "no device matches" when nothing
             runs, "matches several devices" with 2 running, so no device is picked then (the kept
             `info` of `booted` likewise).
A one-shot call includes the process's start; `simee --version` is timed too, as its floor.

--make-home DIR only builds the home (aged, nothing running) and prints the two paths the
in-process breakdown takes: wait 3 s (a file changed in the last 2 s is never remembered), then
`SIMEE_BENCH_SDK=DIR/sdk SIMEE_BENCH_AVD_HOME=DIR/avd cargo test --release -p simee-android
listing_cost -- --ignored --nocapture` prints p50 and p95 over 1000 calls of each part of a listing.

Reproduces docs/benchmarks.md "Android device resolution".
"""
import argparse, atexit, importlib.util, json, os, shutil, signal, socket, statistics, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.dont_write_bytecode = True  # no __pycache__ beside the scripts
_spec = importlib.util.spec_from_file_location('summarize', os.path.join(HERE, 'summarize.py'))
summarize = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(summarize)

IMAGES = [  # (sysdir, tag.id, device name, width, height, density)
    ('android-36/google_apis/arm64-v8a', 'google_apis', 'pixel_9', 1080, 2424, 420),
    ('android-36/google_apis_tablet/arm64-v8a', 'google_apis_tablet', 'pixel_tablet', 2560, 1600, 320),
    ('android-34/android-wear/arm64-v8a', 'android-wear', 'wearos_small_round', 384, 384, 320),
    ('android-34/android-tv/arm64-v8a', 'android-tv', 'tv_1080p', 1920, 1080, 213),
    ('android-34-ext9/android-automotive/arm64-v8a', 'android-automotive', 'automotive_1024p_landscape', 1024, 768, 160),
]
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


def config(sysdir, tag, hw, w, h, dpi):
    """What avdmanager writes: ~45 keys, of which resolution reads a handful."""
    return '\n'.join([
        'AvdId=x', 'PlayStore.enabled=false', 'abi.type=arm64-v8a', 'avd.ini.displayname=Bench AVD', 'avd.ini.encoding=UTF-8',
        'disk.dataPartition.size=6G', 'fastboot.chosenSnapshotFile=', 'fastboot.forceChosenSnapshotBoot=no',
        'fastboot.forceColdBoot=no', 'fastboot.forceFastBoot=yes', 'hw.accelerometer=yes', 'hw.arc=false', 'hw.audioInput=yes',
        'hw.battery=yes', 'hw.camera.back=virtualscene', 'hw.camera.front=emulated', 'hw.cpu.arch=arm64', 'hw.cpu.ncore=4',
        'hw.dPad=no', f'hw.device.manufacturer=Google', f'hw.device.name={hw}', 'hw.gps=yes', 'hw.gpu.enabled=yes',
        'hw.gpu.mode=auto', 'hw.initialOrientation=portrait', 'hw.keyboard=yes', f'hw.lcd.density={dpi}', f'hw.lcd.height={h}',
        f'hw.lcd.width={w}', 'hw.mainKeys=no', 'hw.ramSize=2048', 'hw.sdCard=yes', 'hw.sensors.orientation=yes',
        'hw.sensors.proximity=yes', 'hw.trackBall=no', f'image.sysdir.1=system-images/{sysdir}/', 'runtime.network.latency=none',
        'runtime.network.speed=full', 'showDeviceFrame=yes', f'tag.display={tag}', f'tag.id={tag}', 'vm.heapSize=228', ''])


def make_home(root, n):
    sdk, avd = os.path.join(root, 'sdk'), os.path.join(root, 'avd')
    for sysdir, *_ in IMAGES:
        d = os.path.join(sdk, 'system-images', sysdir)
        os.makedirs(d)
        open(os.path.join(d, 'system.img'), 'w').write('s')
        open(os.path.join(d, 'kernel-ranchu'), 'w').write('k')
        open(os.path.join(d, 'source.properties'), 'w').write('AndroidVersion.ApiLevel=36\nPkg.Revision=7\n')
    os.makedirs(avd)
    os.makedirs(os.path.join(root, 'home'))
    names = []
    for i in range(n):
        name = f'Bench_{i:02d}'
        names.append(name)
        d = os.path.join(avd, f'{name}.avd')
        os.makedirs(d)
        open(os.path.join(avd, f'{name}.ini'), 'w').write(
            f'avd.ini.encoding=UTF-8\npath={d}\npath.rel=avd/{name}.avd\ntarget=android-36\n')
        open(os.path.join(d, 'config.ini'), 'w').write(config(*IMAGES[i % len(IMAGES)]))
        for f in ('userdata.img', 'sdcard.img', 'cache.img'):
            open(os.path.join(d, f), 'w').write('')
    # AVDs are made long before a call asks about them: age every file, so that none is "just
    # written" (a file modified in the last 2 s is read again each time, never remembered).
    old = time.time() - 86400
    for top, dirs, files in os.walk(root):
        for f in files:
            os.utime(os.path.join(top, f), (old, old))
    return sdk, avd, names


def start_fakes(root, avd, names, count):
    """`count` stand-ins for stock qemus: a shell whose command line is a qemu's, holding the lock."""
    qemu = os.path.join(root, 'qemu-system-aarch64-headless')
    open(qemu, 'w').write('#!/bin/sh\nwhile :; do sleep 1; done\n')
    os.chmod(qemu, 0o755)
    out = []
    for i, name in enumerate(names[:count]):
        p = subprocess.Popen([qemu, '-avd', name, '-port', str(5900 + 2 * i)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        procs.append(p)
        open(os.path.join(avd, f'{name}.avd', 'hardware-qemu.ini.lock'), 'w').write(f'{p.pid}\0')
        out.append((p, name))
    return out


def stop_fakes(avd, fakes):
    for p, name in fakes:
        p.kill()
        p.wait()
        procs.remove(p)
        os.unlink(os.path.join(avd, f'{name}.avd', 'hardware-qemu.ini.lock'))


class Client:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(path)
        self.r = self.s.makefile('rb', buffering=1 << 20)
        self.id = 0

    def call(self, method, **params):
        self.id += 1
        self.s.sendall((json.dumps({'id': self.id, 'method': method, 'params': params}) + '\n').encode())
        return json.loads(self.r.readline())

    def close(self):
        self.s.close()


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))]


def timed(fn, n):
    fn()  # warm-up: the first call of a process pays for its caches
    out = []
    for _ in range(n):
        t = time.perf_counter()
        fn()
        out.append((time.perf_counter() - t) * 1000)
    return pct(out, 50), pct(out, 95)


def env_for(root, sock):
    env = {k: v for k, v in os.environ.items() if not k.startswith(('SIMEE_', 'ANDROID_'))}
    env.update(HOME=os.path.join(root, 'home'), ANDROID_HOME=os.path.join(root, 'sdk'), ANDROID_AVD_HOME=os.path.join(root, 'avd'),
               SIMEE_SOCKET=sock)
    return env


def serve(simee, env, path):
    p = subprocess.Popen([simee, 'serve'], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    procs.append(p)
    for _ in range(100):
        if os.path.exists(path):
            time.sleep(1)  # a server has been up a while: its host sampler's first scan is over
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


def measure(simee, env, sock, names, running, n):
    """(metric, p50, p95) for one build against the current home."""
    out = []
    exact = names[0]
    state = f'{running} running' if running else 'none running'
    srv = serve(simee, env, sock)
    c = Client(sock)
    for label, call in [
        ('devices', lambda: c.call('devices')),
        ('devices booted', lambda: c.call('devices', booted=True)),
        ('info exact name', lambda: c.call('info', device=exact)),
        ('info booted', lambda: c.call('info', device='booted')),
    ]:
        out.append((f'kept, {state}: {label}',) + timed(call, n))
    c.close()
    stop(srv)
    os.unlink(sock) if os.path.exists(sock) else None
    run = lambda *a: subprocess.run([simee, *a], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for label, args in [
        ('--version (process floor)', ['--version']),
        ('list', ['list']),
        ('list --booted', ['list', '--booted']),
        ('tap exact name', ['tap', exact, '1', '1']),
        ('tap booted', ['tap', 'booted', '1', '1']),
    ]:
        out.append((f'one-shot, {state}: {label}',) + timed(lambda: run(*args), n))
    return out


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--avds', type=int, default=44)
    p.add_argument('--running', type=int, default=2)
    p.add_argument('--calls', type=int, default=100)
    p.add_argument('--baseline')
    p.add_argument('--simee')
    p.add_argument('--rounds', type=int, default=3)
    p.add_argument('--quick', action='store_true')
    p.add_argument('--no-quiet-gate', action='store_true', help='skip the quiet-host gate (a smoke run; its numbers are not published)')
    p.add_argument('--make-home')
    p.add_argument('--out', default=os.path.join(REPO, 'bench', 'results'))
    a = p.parse_args()
    if a.make_home:
        os.makedirs(a.make_home)
        sdk, avd, _ = make_home(a.make_home, a.avds)
        print(f'SIMEE_BENCH_SDK={sdk} SIMEE_BENCH_AVD_HOME={avd}')
        return 0
    rounds = 1 if a.quick else a.rounds
    n = 10 if a.quick else a.calls
    summarize.quiet_gate(a.no_quiet_gate)
    host_start = summarize.host()
    simee = a.simee
    if not simee:
        log('building target/release/simee')
        subprocess.run(['cargo', 'build', '-q', '--release', '-p', 'simee'], cwd=REPO, check=True)
        simee = os.path.join(REPO, 'target', 'release', 'simee')
    builds = [simee] + ([a.baseline] if a.baseline else [])
    label = (lambda b: 'simee') if not a.baseline else (lambda b: 'after' if b == simee else 'before')
    root = tempfile.mkdtemp(prefix='ar.', dir='/tmp')
    cleanups.append(lambda: shutil.rmtree(root, ignore_errors=True))
    sdk, avd, names = make_home(root, a.avds)
    sock = os.path.join(root, 's.sock')
    env = env_for(root, sock)
    samples = []
    for r in range(1, rounds + 1):
        for running in (0, a.running):
            fakes = start_fakes(root, avd, names, running)
            order = builds if r % 2 or len(builds) == 1 else builds[::-1]
            for b in order:
                for metric, p50, p95 in measure(b, env, sock, names, running, n):
                    log(f'round {r} {label(b)} {metric}: p50 {p50:.3f} p95 {p95:.3f}')
                    for q, v in (('p50', p50), ('p95', p95)):
                        samples.append({'round': r, 'arm': label(b), 'metric': f'{metric} ({q})', 'value': round(v, 3),
                                        'load1': round(os.getloadavg()[0], 2)})
            stop_fakes(avd, fakes)
    out = os.path.join(a.out, f'android-resolve-{time.strftime("%Y%m%d-%H%M%S")}.json')
    print(summarize.finish('android-resolve', samples, host_start, summarize.host(), out, ' '.join(sys.argv[1:]),
                           ['before', 'after', 'simee'],
                           f'{a.avds} AVDs, {a.running} stand-in stock emulators when running; {n} calls per metric and round.'))
    return 0


if __name__ == '__main__':
    sys.exit(main())
