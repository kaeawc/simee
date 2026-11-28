#!/usr/bin/env bash
# What this host is, as one JSON object on stdout: the fingerprint a benchmark number depends on.
#
#   scripts/bench/host-profile.sh [--window S] [--no-gpu-probe] [--help]
#
# Records: the OS, kernel, CPU model and arch, logical and physical cores, RAM and swap, the disk
# that holds the checkout and $TMPDIR (size, free, filesystem, device, whether the kernel calls it
# rotational), /dev/kvm (exists, and whether a VM can be made on it), the GPU (render nodes,
# Vulkan ICDs, `vulkaninfo --summary` when installed), the cgroup CPU and memory limits (v2 and
# v1, the tightest of the process's cgroup and its parents), CPU steal from /proc/stat (since
# boot, and over --window seconds, default 2; 0 skips the window), the container backend's
# prerequisites (binder, cgroup v2, user namespaces, overlayfs: #210), and the CI the host is.
#
# The first fields are the host record itself, taken from `scripts/bench/summarize.py host` (the
# one every benchmark records: model, cpu, os, os_version, cpus, cores_physical, mem_gb, kvm, load1
# and load_source, thermal and power from quiet.sh, simee_commit), never a copy of it; the rest is
# what a cloud host needs besides.
# Everything a host can't report is null, never a guess. Runs on Linux (a cloud VM, a GitHub
# Actions Ubuntu runner, a container) and degrades on macOS. Needs python3. Reads only.
# docs/research/cloud-hosts.md.
set -uo pipefail
dir=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$dir/../.." && pwd)
window=2
gpu_probe=1
while [ $# -gt 0 ]; do
  case $1 in
    --window)
      [ $# -ge 2 ] || {
        echo "host-profile.sh: --window needs seconds" >&2
        exit 2
      }
      window=$2
      shift 2
      ;;
    --no-gpu-probe)
      gpu_probe=0
      shift
      ;;
    -h | --help)
      awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"
      exit 0
      ;;
    *)
      echo "host-profile.sh: unknown option $1 (--help lists them)" >&2
      exit 2
      ;;
  esac
done
case $window in
  '' | *[!0-9]*)
    echo "host-profile.sh: --window is a whole number of seconds" >&2
    exit 2
    ;;
  *) ;;
esac

# The host record every benchmark writes: one source, so the two can't drift.
record=$(python3 -I "$dir/summarize.py" host) || {
  echo "host-profile.sh: summarize.py host failed" >&2
  exit 1
}

# The Vulkan loader's own summary, when there is one. Bounded: a broken ICD can hang it.
vulkan=""
if [ "$gpu_probe" = 1 ] && command -v vulkaninfo >/dev/null 2>&1; then
  vulkan=$(timeout 30 vulkaninfo --summary 2>&1 | head -80)
fi

HP_RECORD=$record HP_VULKAN=$vulkan HP_WINDOW=$window HP_REPO=$repo \
  exec python3 -I - <<'PY'
import datetime, fcntl, glob, json, os, platform, re, shutil, subprocess, sys, time

env = os.environ


def read(path, default=None):
    try:
        with open(path) as f:
            return f.read().strip()
    except OSError:
        return default


def run(*cmd, timeout=20):
    try:
        r = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, timeout=timeout)
        return r.stdout.strip() if r.returncode == 0 else None
    except (OSError, subprocess.TimeoutExpired):
        return None


def num(text):
    try:
        return int(text)
    except (TypeError, ValueError):
        return None


linux = sys.platform.startswith('linux')
h = json.loads(env['HP_RECORD'])
cpuinfo = read('/proc/cpuinfo', '') if linux else ''
meminfo = read('/proc/meminfo', '') if linux else ''


def meminfo_kb(key):
    m = re.search(rf'^{key}:\s+(\d+) kB', meminfo, re.M)
    return int(m.group(1)) if m else None


# --- more on the CPU and memory ---------------------------------------------------------------
flags = next((l.split(':', 1)[1].split() for l in cpuinfo.splitlines() if l.startswith(('flags', 'Features'))), [])
h['arch'] = platform.machine()
h['kernel'] = platform.release()
h['cpu_mhz'] = next((float(l.split(':', 1)[1]) for l in cpuinfo.splitlines() if l.startswith('cpu MHz')), None)
h['cpu_flags'] = {k: k in flags for k in ('hypervisor', 'vmx', 'svm', 'sse4_2', 'avx2', 'avx512f')} if flags else None
h['virtualization'] = run('systemd-detect-virt') if linux else None
h['dmi_vendor'] = read('/sys/devices/virtual/dmi/id/sys_vendor')
h['mem_available_gb'] = round(meminfo_kb('MemAvailable') / 2**20, 2) if meminfo_kb('MemAvailable') else None
h['swap_gb'] = round((meminfo_kb('SwapTotal') or 0) / 2**20, 1) if linux else None

# --- disk -------------------------------------------------------------------------------------


def disk(path):
    try:
        u = shutil.disk_usage(path)
    except OSError:
        return None
    d = {'path': path, 'total_gb': round(u.total / 1e9, 1), 'free_gb': round(u.free / 1e9, 1)}
    src = run('findmnt', '-no', 'SOURCE,FSTYPE', '-T', path) if linux else None
    if src:
        parts = src.split()
        d['device'] = parts[0]
        d['fstype'] = parts[1] if len(parts) > 1 else None
        # /dev/root and device-mapper names don't map to a /sys/block entry: ask lsblk for the
        # parent disk of the mount's device, else fall back to the name's basename.
        name = run('lsblk', '-no', 'PKNAME', os.path.realpath(parts[0])) or os.path.basename(os.path.realpath(parts[0]))
        name = name.splitlines()[0] if name else ''
        d['block_device'] = name or None
        rot = read(f'/sys/block/{name}/queue/rotational') if name else None
        if rot is None and name:
            m = re.match(r'(.*?)(p?\d+)$', name)
            rot = read(f'/sys/block/{m.group(1)}/queue/rotational') if m else None
        d['rotational'] = (rot == '1') if rot in ('0', '1') else None
        d['virtio_blk'] = bool(name) and name.startswith('vd')
    return d


tmp = env.get('TMPDIR', '/tmp')
h['disk'] = {'repo': disk(env.get('HP_REPO', '.')), 'tmp': disk(tmp)}

# --- /dev/kvm ---------------------------------------------------------------------------------


def kvm():
    k = {'exists': os.path.exists('/dev/kvm'), 'usable': False, 'why': None}
    if not k['exists']:
        k['why'] = '/dev/kvm does not exist'
        return k
    if not os.access('/dev/kvm', os.R_OK | os.W_OK):
        k['why'] = '/dev/kvm exists but this user cannot read and write it'
        return k
    try:
        fd = os.open('/dev/kvm', os.O_RDWR | os.O_CLOEXEC)
    except OSError as e:
        k['why'] = f'open /dev/kvm: {e.strerror}'
        return k
    try:
        # KVM_GET_API_VERSION is 12; KVM_CREATE_VM (type 0) is the call that fails with EBUSY or
        # EINVAL on a host whose hypervisor can't nest.
        api = fcntl.ioctl(fd, 0xAE00)
        k['api_version'] = api
        vm = fcntl.ioctl(fd, 0xAE01, 0)
        os.close(vm)
        k['usable'] = api == 12
        k['why'] = None if k['usable'] else f'KVM API version {api}, not 12'
    except OSError as e:
        k['why'] = f'KVM ioctl: {e.strerror}'
    finally:
        os.close(fd)
    return k


h['kvm_detail'] = kvm() if linux else None
h['kvm_usable'] = h['kvm_detail']['usable'] if h['kvm_detail'] else False

# --- GPU --------------------------------------------------------------------------------------


def icds():
    out = []
    for pattern in ('/usr/share/vulkan/icd.d/*.json', '/etc/vulkan/icd.d/*.json', '/usr/local/share/vulkan/icd.d/*.json'):
        for p in sorted(glob.glob(pattern)):
            try:
                lib = json.load(open(p)).get('ICD', {}).get('library_path')
            except (OSError, ValueError):
                lib = None
            out.append({'file': p, 'library': lib})
    return out


vulkan = env.get('HP_VULKAN', '')
devices = sorted(re.findall(r'deviceName\s*=\s*(.+)', vulkan))
nodes = sorted(glob.glob('/dev/dri/*')) if linux else []
pci = run('lspci') if linux and shutil.which('lspci') else None
gpus = [l for l in (pci or '').splitlines() if re.search(r'VGA|3D controller|Display controller', l)]
h['gpu'] = {
    'render_nodes': nodes,
    'pci_gpus': gpus,
    'vulkan_icds': icds(),
    'vulkaninfo': 'present' if shutil.which('vulkaninfo') else 'absent',
    'vulkan_devices': devices,
    'vulkan_summary': vulkan.splitlines()[:40] if vulkan else None,
    'software_only': (not nodes and not gpus) if linux else None,
}

# --- cgroup limits ----------------------------------------------------------------------------


def cgroup():
    c = {'version': None, 'path': None, 'cpu_limit_cores': None, 'memory_limit_gb': None, 'raw': {}}
    if not linux:
        return c
    mounts = read('/proc/self/cgroup', '') or ''
    v2 = os.path.exists('/sys/fs/cgroup/cgroup.controllers')
    cpu_limit = []
    mem_limit = []
    if v2:
        c['version'] = 2
        path = next((l.split(':', 2)[2] for l in mounts.splitlines() if l.startswith('0::')), '/')
        c['path'] = path
        d = '/sys/fs/cgroup' + path
        # The limit that binds is the tightest of the cgroup and its parents.
        while True:
            cpu_max = read(d + '/cpu.max')
            mem_max = read(d + '/memory.max')
            if cpu_max is not None and d == '/sys/fs/cgroup' + path:
                c['raw']['cpu.max'] = cpu_max
            if mem_max is not None and d == '/sys/fs/cgroup' + path:
                c['raw']['memory.max'] = mem_max
            if cpu_max and not cpu_max.startswith('max'):
                q, p = cpu_max.split()[:2]
                cpu_limit.append(int(q) / int(p))
            if mem_max and mem_max != 'max':
                mem_limit.append(int(mem_max))
            if d in ('/sys/fs/cgroup', ''):
                break
            d = os.path.dirname(d)
        for key in ('pids.max', 'memory.high', 'memory.swap.max'):
            v = read(f'/sys/fs/cgroup{path}/{key}')
            if v is not None:
                c['raw'][key] = v
    elif os.path.isdir('/sys/fs/cgroup/cpu') or os.path.isdir('/sys/fs/cgroup/memory'):
        c['version'] = 1
        sub = {l.split(':')[1]: l.split(':', 2)[2] for l in mounts.splitlines() if l.count(':') >= 2}
        cpath = '/sys/fs/cgroup/cpu' + sub.get('cpu,cpuacct', sub.get('cpu', '/'))
        mpath = '/sys/fs/cgroup/memory' + sub.get('memory', '/')
        quota = num(read(cpath + '/cpu.cfs_quota_us'))
        period = num(read(cpath + '/cpu.cfs_period_us'))
        limit = num(read(mpath + '/memory.limit_in_bytes'))
        c['path'] = sub.get('memory', '/')
        c['raw'] = {'cpu.cfs_quota_us': quota, 'cpu.cfs_period_us': period, 'memory.limit_in_bytes': limit}
        if quota and quota > 0 and period:
            cpu_limit.append(quota / period)
        # An unlimited v1 memory cgroup reports a huge number, not a word.
        if limit and limit < 2**60:
            mem_limit.append(limit)
    c['cpu_limit_cores'] = round(min(cpu_limit), 2) if cpu_limit else None
    c['memory_limit_gb'] = round(min(mem_limit) / 2**30, 2) if mem_limit else None
    return c


h['cgroup'] = cgroup()

# --- steal ------------------------------------------------------------------------------------


def stat_line():
    for l in (read('/proc/stat', '') or '').splitlines():
        if l.startswith('cpu '):
            return [int(x) for x in l.split()[1:]]
    return None


def steal_of(a, b=None):
    # user nice system idle iowait irq softirq steal guest guest_nice; guest time is already in
    # user and nice, so the total is the first eight.
    if a is None or len(a) < 8:
        return None
    if b is not None:
        a = [y - x for x, y in zip(a, b)]
    total = sum(a[:8])
    return {'steal_jiffies': a[7], 'total_jiffies': total, 'steal_pct': round(100 * a[7] / total, 3) if total else None,
            'busy_pct': round(100 * (total - a[3] - a[4]) / total, 2) if total else None}


s0 = stat_line() if linux else None
steal = {'since_boot': steal_of(s0), 'window_s': None, 'window': None}
window = int(env.get('HP_WINDOW', '2'))
if s0 and window > 0:
    time.sleep(window)
    steal['window_s'] = window
    steal['window'] = steal_of(s0, stat_line())
h['steal'] = steal

# --- the container backend's prerequisites (#210) ----------------------------------------------
fs = read('/proc/filesystems', '') or ''
h['container'] = {
    'binderfs': bool(re.search(r'\bbinder(_)?fs\b', fs)),
    'binder_device': bool(glob.glob('/dev/binder*')) or os.path.isdir('/dev/binderfs'),
    'cgroup2': os.path.exists('/sys/fs/cgroup/cgroup.controllers'),
    'overlayfs': bool(re.search(r'\boverlay\b', fs)),
    'user_namespaces': (num(read('/proc/sys/user/max_user_namespaces')) or 0) > 0 if linux else None,
    'memfd': tuple(int(x) for x in re.findall(r'\d+', platform.release())[:2]) >= (3, 17),
} if linux else None

# --- the CI this is ---------------------------------------------------------------------------
h['ci'] = {
    'github_actions': env.get('GITHUB_ACTIONS') == 'true',
    'runner_os': env.get('RUNNER_OS'),
    'runner_name': env.get('RUNNER_NAME'),
    'image': env.get('ImageOS'),
    'image_version': env.get('ImageVersion'),
}

print(json.dumps(h, indent=2, sort_keys=False))
PY
