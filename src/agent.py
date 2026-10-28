# rgtop remote agent: runs on each server as `python3 -c <this file>`, fed by ssh.
# Every request line on stdin ("s" system+GPUs, "p" + GPU processes, "h" + top CPU
# users) gets exactly one frame on stdout. GPUs are read through NVML directly
# (~1 ms for 8 GPUs, vs ~250 ms to start nvidia-smi), /proc is read in-process:
# a sample forks nothing. Python 3.4+ stdlib only. Every non-GPU line is tagged
# @@x so shell-rc noise can't be mistaken for data.
import ctypes, os, pwd, sys, threading, time

TCK = os.sysconf("SC_CLK_TCK")
PAGE_KB = os.sysconf("SC_PAGE_SIZE") // 1024
NA = 2 ** 64 - 1  # NVML_VALUE_NOT_AVAILABLE
LEVELS = "0123456789abcdefghijklmnopqrstuvwxyz"  # per-core busy, 0..35


def rd(path):
    try:
        with open(path, "rb") as f:
            return f.read().decode("utf-8", "replace")  # any locale, any bytes in argv
    except OSError:
        return ""


def mib(b):
    return "%d" % ((b + 524288) // 1048576)  # rounded, like nvidia-smi


try:
    me = pwd.getpwuid(os.getuid()).pw_name
except KeyError:  # uid without a passwd entry (containers, flaky SSSD)
    me = str(os.getuid())
names = {}


def user(uid):
    u = names.get(uid)
    if u is None:
        try:
            u = pwd.getpwuid(uid).pw_name
        except KeyError:
            u = str(uid)
        names[uid] = u
    return u


# static host info, read once
info = rd("/proc/cpuinfo").splitlines()
model = next((l.split(":", 1)[1].strip() for l in info if l.startswith("model name")), "")
sockets = len(set(l for l in info if l.startswith("physical id"))) or 1
del info
temp_files = []
try:
    for h in sorted(os.listdir("/sys/class/hwmon")):
        if rd("/sys/class/hwmon/%s/name" % h).strip() in ("coretemp", "k10temp", "zenpower"):
            temp_files.append("/sys/class/hwmon/%s/temp1_input" % h)  # package / Tctl
except OSError:
    pass

# ── NVML ──────────────────────────────────────────────────────────────────────
c_uint, c_ull, byref = ctypes.c_uint, ctypes.c_ulonglong, ctypes.byref


class Util(ctypes.Structure):
    _fields_ = [("gpu", c_uint), ("memory", c_uint)]


class Mem1(ctypes.Structure):
    _fields_ = [("total", c_ull), ("free", c_ull), ("used", c_ull)]


class Mem2(ctypes.Structure):  # v2 "used" excludes driver-reserved memory, like nvidia-smi
    _fields_ = [("version", c_uint), ("total", c_ull), ("reserved", c_ull), ("free", c_ull), ("used", c_ull)]


class Proc2(ctypes.Structure):
    _fields_ = [("pid", c_uint), ("mem", c_ull), ("gi", c_uint), ("ci", c_uint)]


class Proc1(ctypes.Structure):
    _fields_ = [("pid", c_uint), ("mem", c_ull)]


nv, nv_err, devs = None, "", []
try:
    nv = ctypes.CDLL("libnvidia-ml.so.1")
    rc = nv.nvmlInit_v2()
    if rc:
        raise OSError("nvmlInit failed with code %d" % rc)
    n = c_uint()
    nv.nvmlDeviceGetCount_v2(byref(n))
    buf = ctypes.create_string_buffer(96)
    for i in range(n.value):
        h = ctypes.c_void_p()
        if nv.nvmlDeviceGetHandleByIndex_v2(i, byref(h)):
            continue
        name = buf.value.decode(errors="replace") if nv.nvmlDeviceGetName(h, buf, 96) == 0 else "[N/A]"
        uuid = buf.value.decode(errors="replace") if nv.nvmlDeviceGetUUID(h, buf, 96) == 0 else "GPU-%d" % i
        devs.append((i, h, uuid, name))
except (OSError, AttributeError) as e:
    nv, nv_err = None, str(e)

if nv is not None:
    mem2 = hasattr(nv, "nvmlDeviceGetMemoryInfo_v2")
    M = Mem2() if mem2 else Mem1()
    if mem2:
        M.version = ctypes.sizeof(Mem2) | (2 << 24)
    get_mem = nv.nvmlDeviceGetMemoryInfo_v2 if mem2 else nv.nvmlDeviceGetMemoryInfo
    # the enforced limit is ~1 us; nvmlDeviceGetPowerManagementLimit ~190 us per GPU
    get_limit = getattr(nv, "nvmlDeviceGetEnforcedPowerLimit", None) or nv.nvmlDeviceGetPowerManagementLimit
    get_procs, PROC = nv.nvmlDeviceGetComputeRunningProcesses, Proc1
    for fn in ("nvmlDeviceGetComputeRunningProcesses_v3", "nvmlDeviceGetComputeRunningProcesses_v2"):
        if hasattr(nv, fn):
            get_procs, PROC = getattr(nv, fn), Proc2
            break
U, T, P, L = Util(), c_uint(), c_uint(), c_uint()


def gpu_rows(w):
    for i, h, uuid, name in devs:
        util = str(U.gpu) if nv.nvmlDeviceGetUtilizationRates(h, byref(U)) == 0 else "[N/A]"
        if get_mem(h, byref(M)) == 0:
            used, total = mib(M.used), mib(M.total)
        else:
            used = total = "[N/A]"
        temp = str(T.value) if nv.nvmlDeviceGetTemperature(h, 0, byref(T)) == 0 else "[N/A]"
        power = "%.2f" % (P.value / 1000.0) if nv.nvmlDeviceGetPowerUsage(h, byref(P)) == 0 else "[N/A]"
        limit = "%.2f" % (L.value / 1000.0) if get_limit(h, byref(L)) == 0 else "[N/A]"
        w("%d, %s, %s, %s, %s, %s, %s, %s, %s\n" % (i, uuid, name, util, used, total, temp, power, limit))


def gpu_procs(h):
    n = 64
    while True:
        arr, c = (PROC * n)(), c_uint(n)
        rc = get_procs(h, byref(c), arr)
        if rc == 7 and n < 65536:  # NVML_ERROR_INSUFFICIENT_SIZE
            n *= 4
            continue
        return [] if rc else [(arr[k].pid, arr[k].mem) for k in range(c.value)]


# ── /proc ─────────────────────────────────────────────────────────────────────
def cpu_stat(per_core):
    """-> (the "cpu" total line, [(total, idle + iowait)] per core or None).
    Parsing 200 core lines costs more than reading the file: only on request."""
    text = rd("/proc/stat")
    if not per_core:
        return text[: text.find("\n")], None
    lines = text.split("\n")
    cores = []
    for l in lines[1:]:
        if not l.startswith("cpu"):
            break
        v = [int(x) for x in l.split()[1:9]]
        cores.append((sum(v), v[3] + v[4]))
    return lines[0], cores


def pid_stat(pid):
    """-> (cpu ticks, start ticks, rss kB), or None if the process isn't visible"""
    try:
        f = rd("/proc/%d/stat" % pid).rsplit(")", 1)[1].split()  # comm may contain spaces / parens
        return int(f[11]) + int(f[12]), int(f[19]), int(f[21]) * PAGE_KB
    except (IndexError, ValueError):
        return None


def meminfo():
    m = {}
    for l in rd("/proc/meminfo").splitlines():
        k, _, v = l.partition(":")
        m[k] = int(v.split()[0]) if v.split() else 0
    g = m.get
    cache = g("Buffers", 0) + g("Cached", 0) + g("SReclaimable", 0) - g("Shmem", 0)
    return g("MemTotal", 0), g("MemAvailable", 0), max(cache, 0), g("SwapTotal", 0), g("SwapFree", 0)


prev_tot, prev_cores = cpu_stat(True)
t_start = time.monotonic()
core_t, core_levels = t_start - 1.0, "0" * len(prev_cores)  # the first frame computes the strip too  # per-core strip: refreshed every 0.5 s
proc_t, proc_lines = 0.0, []  # GPU processes: refreshed every 1 s (NVML: ~0.4 ms per GPU)
prev_pid = {}  # GPU process pid -> (cpu ticks, wall time)
host_lines, host_wanted, host_gap, host_thread = [], 0.0, 0.0, None


def scan_host_users():
    """Top CPU users of the whole host, from /proc deltas (ps %cpu is a lifetime
    average). Thousands of processes, so this runs in a background thread every
    5 s and frames never wait for it; it stops when no longer asked for."""
    global host_lines
    prev, t0, nap = None, 0.0, 1.0
    while time.monotonic() - host_wanted < max(15.0, 3 * host_gap):  # outlives slow refresh intervals
        now, cur = time.monotonic(), {}
        for d in os.listdir("/proc"):
            st = pid_stat(int(d)) if d.isdigit() else None
            if st is None:
                continue
            try:
                cur[int(d)] = (os.stat("/proc/" + d).st_uid, st[0], st[2])
            except OSError:
                pass  # exited meanwhile
        if prev is not None and now > t0:
            agg = {}
            for pid, (uid, ticks, rss) in cur.items():
                p = prev.get(pid)
                a = agg.setdefault(uid, [0, 0])
                a[0] += ticks - (p[1] if p is not None and p[0] == uid else 0)  # new pid: all of it is new
                a[1] += rss
            top = sorted(((100.0 * a[0] / TCK / (now - t0), a[1], uid) for uid, a in agg.items()), reverse=True)
            host_lines = ["@@H %s %.0f %d\n" % (user(uid), c, rss) for c, rss, uid in top[:6] if c >= 1]
        prev, t0 = cur, now
        time.sleep(nap)
        nap = 5.0
    host_lines = []


def gpu_proc_lines(now, uptime):
    global prev_pid
    out = ["@@X\n"]
    pids = []
    for i, h, uuid, name in devs:
        for pid, mem in gpu_procs(h):
            out.append("@@A %s, %d, %s\n" % (uuid, pid, "[N/A]" if mem >= NA else mib(mem)))
            pids.append(pid)
    seen = {}
    for pid in set(pids):
        st = pid_stat(pid)
        if st is None:
            continue  # other PID namespace, or exited without freeing VRAM
        try:
            uid = os.stat("/proc/%d" % pid).st_uid
        except OSError:
            continue
        cmd = rd("/proc/%d/cmdline" % pid).replace("\0", " ").replace("\n", " ").replace("\r", " ").strip()
        out.append("@@P %d %s %d %s\n" % (pid, user(uid), max(0, int(uptime - st[1] / TCK)), cmd[:300]))
        p = prev_pid.get(pid)
        cpu = "%.1f" % (100.0 * (st[0] - p[0]) / TCK / (now - p[1])) if p and now > p[1] else "-"
        out.append("@@Q %d %s %d\n" % (pid, cpu, st[2]))
        seen[pid] = (st[0], now)
    prev_pid = seen
    return out


def frame(req):
    global prev_tot, prev_cores, core_t, core_levels, proc_t, proc_lines, host_wanted, host_gap, host_thread
    now = time.monotonic()
    if now - t_start < 0.3:  # first frame: give CPU % a window long enough to mean something
        time.sleep(0.3 - (now - t_start))
        now = time.monotonic()
    out = ["@@B\n", "@@S %s\n" % prev_tot]
    w = out.append
    if nv is not None:
        gpu_rows(w)
        w("@@G 0\n")
    else:
        w("@@G 127\n")
    tot, cores = cpu_stat(now - core_t >= 0.5)
    if cores is not None:
        lv = []
        for (t, i), (pt, pi) in zip(cores, prev_cores):
            dt = t - pt
            busy = int(round(35.0 * (dt - (i - pi)) / dt)) if dt > 0 else 0
            lv.append(LEVELS[min(35, max(0, busy))])  # idle counters can step backwards
        prev_cores, core_t, core_levels = cores, now, "".join(lv)
    prev_tot = tot
    total, avail, cache, swap_t, swap_f = meminfo()
    w("@@S %s\n@@L %s\n@@M %d %d\n" % (tot, rd("/proc/loadavg").strip(), total, avail))
    w("@@W %d %d %d\n@@N %d\n@@U %s\n@@C %s\n" % (cache, swap_t, swap_f, len(core_levels), me, core_levels))
    temps = [int(t) / 1000.0 for t in (rd(f).strip() for f in temp_files) if t.isdigit()]
    if temps:
        w("@@T %.0f\n" % max(temps))
    up = rd("/proc/uptime").split()
    if up:
        w("@@O %s\n" % up[0])
    w("@@I %d %s\n" % (sockets, model))
    if req != "s" and nv is not None:
        if now - proc_t >= 1.0:
            proc_lines, proc_t = gpu_proc_lines(now, float(up[0]) if up else 0.0), now
        out.extend(proc_lines)
        if req == "h":
            host_wanted, host_gap = now, now - host_wanted
            if host_thread is None or not host_thread.is_alive():
                host_thread = threading.Thread(target=scan_host_users, daemon=True)
                host_thread.start()
            out.extend(host_lines)
    w("@@E\n")
    return "".join(out)


if nv is None and len(sys.argv) > 1:
    # No usable NVML (no driver, library off the loader path, driver/library
    # mismatch ...): hand over to the nvidia-smi agent (argv[1]), which also
    # reports whatever nvidia-smi says is wrong. Nothing has been written yet.
    sys.stdout.flush()
    os.execvp("sh", ["sh", "-c", sys.argv[1]])
out = sys.stdout.buffer
while True:
    req = sys.stdin.readline()
    if not req:
        break
    try:
        data = frame(req.strip())
    except Exception as e:  # never die mid-session: report it inside a frame
        data = "@@B\nagent error: %s: %s\n@@G 1\n@@E\n" % (type(e).__name__, e)
    out.write(data.encode("utf-8", "replace"))  # lone surrogates (odd user names) -> "?"
    out.flush()
