"""Remote agent for gpuwho.

Started once per SSH connection with `python3 -u -c <this file> IDLE_SEC`. Protocol: the
client writes one line per poll and the agent answers with one JSON line. Must stay
stdlib-only and Python 3.6 compatible.

GPU data comes straight from NVML via ctypes (the approach gpustat takes with pynvml)
instead of spawning nvidia-smi: one nvmlInit for the agent's lifetime, no fork/exec per
poll, and GPUs without persistence mode are not re-initialised every poll.

Built to be left running without bothering the server:
  * all of /proc is scanned every FULL_SCAN_EVERY polls; in between only the processes
    that mattered last time are re-read. CPU% is the delta since the previous poll
  * NVML runs in a worker thread with a timeout; while a call is stuck in the driver
    no new call is made, CPU/RAM keep being reported. A failed nvmlInit is retried
  * files are read as bytes and decoded leniently, so odd bytes in a command line
    can't break a poll
  * exits on stdin EOF (client gone), after IDLE_SEC without a request, or after
    MAX_LIFETIME (the client transparently restarts it)
"""
import ctypes
import json
import os
import pwd
import re
import select
import socket
import sys
import threading
import time

FIRST_SAMPLE_SEC = 0.5
NVML_TIMEOUT = 15
NVML_RETRY_SEC = 60
FULL_SCAN_EVERY = 5
FAN_TTL = 10
MAX_LIFETIME = 6 * 3600
CLK_TCK = os.sysconf("SC_CLK_TCK")
PAGE_KB = os.sysconf("SC_PAGE_SIZE") // 1024
MIB = 1024 * 1024


def num(s):
    try:
        return float(s)
    except (TypeError, ValueError):
        return None


def read(path):
    """A file as text, "" if unreadable. Read as bytes and decoded leniently: a command line
    with CP949 bytes, or a comm cut in the middle of a UTF-8 character, must not raise."""
    try:
        with open(path, "rb") as f:
            return f.read().decode("utf-8", "replace")
    except (OSError, ValueError):
        return ""


def cpu_times():
    """(total, idle) jiffies. guest and guest_nice are already inside user and nice."""
    vals = [int(v) for v in read("/proc/stat").splitlines()[0].split()[1:9]]
    idle = vals[3] + (vals[4] if len(vals) > 4 else 0)
    return sum(vals), idle


def proc_stats(pids=None):
    """{pid: (state, utime+stime ticks, rss pages, starttime)} for the given pids, or for every
    process. This is most of a poll's cost (thousands of files), so it skips Python's
    buffered text I/O: ~25% less CPU."""
    if pids is None:
        pids = [p for p in os.listdir("/proc") if p.isdigit()]
    out = {}
    for pid in pids:
        try:
            fd = os.open("/proc/%s/stat" % pid, os.O_RDONLY)
            try:
                stat = os.read(fd, 4096)
            finally:
                os.close(fd)
        except OSError:  # exited in between, or hidden by hidepid
            continue
        rest = stat[stat.rfind(b")") + 1:].split()  # comm may contain spaces and ")"
        try:
            out[int(pid)] = (rest[0].decode("ascii", "replace"), int(rest[11]) + int(rest[12]),
                             int(rest[21]), int(rest[19]))
        except (IndexError, ValueError):
            pass
    return out


_uid_cache = {}


def owner_name(uid):
    if uid not in _uid_cache:
        try:
            _uid_cache[uid] = pwd.getpwuid(uid).pw_name
        except KeyError:
            _uid_cache[uid] = str(uid)
    return _uid_cache[uid]


def owner(pid):
    try:
        return owner_name(os.stat("/proc/%d" % pid).st_uid)
    except OSError:
        return None


def container_id(cgroup):
    """Short container id from /proc/PID/cgroup text (docker, containerd, podman, cri-o)."""
    m = re.search(r"(?:docker[-/]|containerd[-/]|libpod-|crio-)([0-9a-f]{12})", cgroup)
    return m.group(1) if m else None


def container_of(pid):
    return container_id(read("/proc/%d/cgroup" % pid))


def cmdline(pid):
    return read("/proc/%d/cmdline" % pid).replace("\x00", " ").strip()[:300]


def proc_state(pid, stats, user):
    """Why a GPU process may show no owner or command: gone, zombie, D state, or hidden."""
    if pid not in stats:
        return "gone"  # NVML still lists it: typically workers holding a dead parent's context
    st = stats[pid][0]
    if st == "Z":
        return "zombie"
    if st == "D":
        return "D"
    return "hidden" if user is None else None


# ---------------------------------------------------------------- RAM per user
#
# Summing RSS over a user's processes overcounts badly: forked DataLoader workers share
# their parent's pages copy-on-write and /dev/shm tensors show up in every process that
# maps them, so 30 workers can "use" 30x the real memory. The kernel charges each page to
# exactly one cgroup, so per-user RAM is read from cgroups instead (user-UID.slice for
# SSH sessions, docker-*.scope for containers) and shared out by RSS when one cgroup
# holds several users. Without cgroup memory accounting it falls back to summing RSS.

def memcg_dirs(cgroup):
    """Memory cgroup directories named in /proc/PID/cgroup text: cgroup v2 first, then v1.
    The root cgroup is skipped: its counters cover the whole machine."""
    dirs = []
    for line in cgroup.splitlines():
        parts = line.split(":", 2)
        if len(parts) != 3 or parts[2].rstrip("/") == "":
            continue
        if parts[0] == "0" and parts[1] == "":
            dirs.append("/sys/fs/cgroup" + parts[2].rstrip("/"))
        elif "memory" in parts[1].split(","):
            dirs.append("/sys/fs/cgroup/memory" + parts[2].rstrip("/"))
    return dirs


def memcg_of(pid):
    for d in memcg_dirs(read("/proc/%d/cgroup" % pid)):
        if os.path.exists(d + "/memory.stat"):
            return d
    return None


def charged_kb(memory_stat):
    """Anonymous + shared memory charged to a cgroup, in kB, from its memory.stat text.
    Page cache is left out: it is reclaimable and not what people mean by "using RAM"."""
    st = {}
    for line in memory_stat.splitlines():
        parts = line.split()
        if len(parts) == 2:
            st[parts[0]] = parts[1]
    anon = st.get("anon", st.get("rss"))  # cgroup v2, v1
    if anon is None:
        return None
    return (int(anon) + int(st.get("shmem", 0))) // 1024


def split_by_rss(groups, charged):
    """{user: kB}. groups is {cgroup dir or None: {user: RSS kB}}; charged(dir) gives the
    cgroup's memory in kB, or None if unreadable, in which case RSS is used as is."""
    out = {}
    for d, shares in groups.items():
        used = charged(d) if d else None
        total = sum(shares.values())
        for user, rss in shares.items():
            kb = rss if used is None or not total else used * rss / total
            out[user] = out.get(user, 0) + kb
    return out


# ---------------------------------------------------------------- NVML (ctypes)

NVML_SUCCESS = 0
NVML_ERROR_UNINITIALIZED = 1
NVML_ERROR_NOT_FOUND = 6
NVML_ERROR_INSUFFICIENT_SIZE = 7
NVML_ERROR_DRIVER_NOT_LOADED = 9
NVML_ERROR_LIBRARY_NOT_FOUND = 12
NVML_ERROR_GPU_IS_LOST = 15
NVML_ERROR_LIB_RM_VERSION_MISMATCH = 18
NVML_ERROR_UNKNOWN = 999
NVML_REINIT = (NVML_ERROR_UNINITIALIZED, NVML_ERROR_DRIVER_NOT_LOADED, NVML_ERROR_LIB_RM_VERSION_MISMATCH)
NVML_NOT_AVAILABLE = 0xFFFFFFFFFFFFFFFF  # usedGpuMemory when the driver can't tell
NVML_MEMORY_V2 = 0x02000028               # sizeof(nvmlMemory_v2_t) | (2 << 24)


class Memory(ctypes.Structure):
    _fields_ = [("total", ctypes.c_ulonglong), ("free", ctypes.c_ulonglong), ("used", ctypes.c_ulonglong)]


class MemoryV2(ctypes.Structure):
    _fields_ = [("version", ctypes.c_uint), ("total", ctypes.c_ulonglong), ("reserved", ctypes.c_ulonglong),
                ("free", ctypes.c_ulonglong), ("used", ctypes.c_ulonglong)]


class Utilization(ctypes.Structure):
    _fields_ = [("gpu", ctypes.c_uint), ("memory", ctypes.c_uint)]


class ProcessV1(ctypes.Structure):
    _fields_ = [("pid", ctypes.c_uint), ("usedGpuMemory", ctypes.c_ulonglong)]


class ProcessV2(ctypes.Structure):  # also the layout of the _v3 API
    _fields_ = [("pid", ctypes.c_uint), ("usedGpuMemory", ctypes.c_ulonglong),
                ("gpuInstanceId", ctypes.c_uint), ("computeInstanceId", ctypes.c_uint)]


class ProcessV3Legacy(ctypes.Structure):  # _v3 on drivers 535.43-535.86 (nvitop, gpustat)
    _fields_ = ProcessV2._fields_ + [("usedGpuCcProtectedMemory", ctypes.c_ulonglong)]


class ProcessUtilSample(ctypes.Structure):
    _fields_ = [("pid", ctypes.c_uint), ("timeStamp", ctypes.c_ulonglong), ("smUtil", ctypes.c_uint),
                ("memUtil", ctypes.c_uint), ("encUtil", ctypes.c_uint), ("decUtil", ctypes.c_uint)]


class GpuLost(Exception):
    pass


def merge_procs(compute, graphics):
    """List each pid once (a process can be both compute and graphics, as gpustat notes),
    typed C / G / C+G like nvidia-smi so the client can tell a parked Xorg from real work."""
    procs, seen = [], {}
    for kind, found in (("C", compute), ("G", graphics)):
        for pid, used in found or []:
            if pid in seen:
                if seen[pid]["type"] != kind:
                    seen[pid]["type"] = "C+G"
            else:
                seen[pid] = {"pid": pid, "mem": used, "type": kind}
                procs.append(seen[pid])
    return procs


def average_sm(samples, since):
    """{pid: mean SM%} from process utilization samples newer than `since` (microseconds),
    and the newest timestamp seen. Values above 100 are driver noise and dropped (nvtop)."""
    acc, newest = {}, since
    for pid, ts, sm in samples:
        if ts <= since or sm > 100:
            continue
        acc.setdefault(pid, []).append(sm)
        newest = max(newest, ts)
    return {pid: round(sum(v) / len(v)) for pid, v in acc.items()}, newest


def nvidia_on_pci():
    """True if an NVIDIA display/3D controller sits on the PCI bus, driver loaded or not."""
    base = "/sys/bus/pci/devices"
    try:
        devices = os.listdir(base)
    except OSError:
        return False
    for dev in devices:
        if read(os.path.join(base, dev, "vendor")).strip() == "0x10de" and \
                read(os.path.join(base, dev, "class")).strip().startswith("0x03"):
            return True
    return False


class Nvml:
    """Minimal NVML binding. Every field is optional, like gpustat's safe calls."""

    def __init__(self):
        self.lib = None
        self.status = "init"   # init | ok | absent | error
        self.error = None
        self.retry_at = 0.0
        self.driver = None
        self.cuda = None
        self.legacy_v3 = False
        self.worker = None     # thread stuck inside the driver, if any
        self.fans = {}         # index -> (monotonic time, fan %): reading it costs 1-2 ms
        self.sm_seen = {}      # index -> newest process-utilization timestamp

    def _fn(self, *names):
        """First available symbol, for APIs that gained _v2/_v3 variants over driver versions."""
        for name in names:
            try:
                return getattr(self.lib, name)
            except AttributeError:
                continue
        return None

    def errstr(self, ret):
        try:
            return self.lib.nvmlErrorString(ret).decode("utf-8", "replace")
        except Exception:
            return "code %d" % ret

    def _fail(self, ret, where):
        """nvmlInit or a whole-system call failed: say why, retry in a while."""
        self.retry_at = time.monotonic() + NVML_RETRY_SEC
        if ret in (NVML_ERROR_DRIVER_NOT_LOADED, NVML_ERROR_LIBRARY_NOT_FOUND) and not nvidia_on_pci():
            self.status, self.error = "absent", None
            return
        self.status, self.error = "error", "%s: %s" % (where, self.errstr(ret))
        if ret == NVML_ERROR_LIB_RM_VERSION_MISMATCH:
            self.error += " (driver updated without reloading the kernel module; a reboot fixes it)"

    def _init(self):
        if self.lib is None:
            try:
                self.lib = ctypes.CDLL("libnvidia-ml.so.1")
            except OSError:
                self.status, self.retry_at = "absent", time.monotonic() + NVML_RETRY_SEC
                return
            self.lib.nvmlErrorString.restype = ctypes.c_char_p
        ret = self._fn("nvmlInit_v2", "nvmlInit")()
        if ret != NVML_SUCCESS:
            self._fail(ret, "nvmlInit")
            return
        buf = ctypes.create_string_buffer(96)
        if self.lib.nvmlSystemGetDriverVersion(buf, 96) == NVML_SUCCESS:
            self.driver = buf.value.decode("utf-8", "replace")
        v = ctypes.c_int()
        fn = self._fn("nvmlSystemGetCudaDriverVersion_v2", "nvmlSystemGetCudaDriverVersion")
        if fn and fn(ctypes.byref(v)) == NVML_SUCCESS:
            self.cuda = "%d.%d" % (v.value // 1000, v.value % 1000 // 10)
        # 535.43-535.86 shipped a 32-byte _v3 process struct; detected the way nvitop does
        self.legacy_v3 = (self._fn("nvmlDeviceGetConfComputeMemSizeInfo") is not None
                          and self._fn("nvmlDeviceGetRunningProcessDetailList") is None)
        self.status, self.error = "ok", None

    def _get(self, fn, handle, ctype, *extra):
        """Call fn(handle, *extra, &out) and return out.value; None if unsupported. Raises GpuLost."""
        out = ctype()
        ret = fn(handle, *(extra + (ctypes.byref(out),)))
        if ret == NVML_ERROR_GPU_IS_LOST:
            raise GpuLost("GPU is lost")
        return out.value if ret == NVML_SUCCESS else None

    def _processes(self, handle, kind):
        """Compute, graphics or MPS processes, trying the _v3, _v2, then v1 API (cf. gpustat #107)."""
        v3 = ProcessV3Legacy if self.legacy_v3 else ProcessV2
        for suffix, struct in (("_v3", v3), ("_v2", ProcessV2), ("", ProcessV1)):
            fn = self._fn("nvmlDevice%sRunningProcesses%s" % (kind, suffix))
            if fn is None:
                continue
            count = ctypes.c_uint(0)
            ret = fn(handle, ctypes.byref(count), None)
            if ret == NVML_SUCCESS:
                return []
            if ret != NVML_ERROR_INSUFFICIENT_SIZE:
                return None
            count.value = count.value * 2 + 5  # processes may start in between the two calls
            arr = (struct * count.value)()
            if fn(handle, ctypes.byref(count), arr) != NVML_SUCCESS:
                return None
            return [(arr[i].pid, None if arr[i].usedGpuMemory == NVML_NOT_AVAILABLE
                     else arr[i].usedGpuMemory / MIB) for i in range(count.value)]
        return None

    def _all_processes(self, handle):
        compute = self._processes(handle, "GetCompute")
        mps = self._processes(handle, "GetMPSCompute")
        graphics = self._processes(handle, "GetGraphics")
        if compute is not None or mps is not None:
            compute = (compute or []) + (mps or [])
        return merge_procs(compute, graphics), (compute is not None or graphics is not None)

    def _memory(self, handle):
        """(used, total) MiB. v2 excludes driver-reserved memory, matching nvidia-smi on
        drivers >= 515 (cf. gpustat #141)."""
        fn_v2 = self._fn("nvmlDeviceGetMemoryInfo_v2")
        if fn_v2 is not None:
            mem = MemoryV2(version=NVML_MEMORY_V2)
            if fn_v2(handle, ctypes.byref(mem)) == NVML_SUCCESS:
                return mem.used / MIB, mem.total / MIB
        mem = Memory()
        if self.lib.nvmlDeviceGetMemoryInfo(handle, ctypes.byref(mem)) == NVML_SUCCESS:
            return mem.used / MIB, mem.total / MIB
        return None, None

    def _fan(self, handle, index):
        now = time.monotonic()
        cached = self.fans.get(index)
        if cached is None or now - cached[0] > FAN_TTL:
            cached = self.fans[index] = (now, self._get(self.lib.nvmlDeviceGetFanSpeed, handle, ctypes.c_uint))
        return cached[1]

    def _mig(self, handle):
        """MIG instances, or None when MIG is off or unsupported. With MIG on, the parent
        handle has no utilization and often no processes, so each instance is read."""
        fn = self._fn("nvmlDeviceGetMigMode")
        cur, pending = ctypes.c_uint(), ctypes.c_uint()
        if fn is None or fn(handle, ctypes.byref(cur), ctypes.byref(pending)) != NVML_SUCCESS or cur.value != 1:
            return None
        lib, count = self.lib, ctypes.c_uint()
        if lib.nvmlDeviceGetMaxMigDeviceCount(handle, ctypes.byref(count)) != NVML_SUCCESS:
            return []
        out = []
        for i in range(count.value):
            h = ctypes.c_void_p()
            if lib.nvmlDeviceGetMigDeviceHandleByIndex(handle, i, ctypes.byref(h)) != NVML_SUCCESS:
                continue  # slot not populated
            used, total = self._memory(h)
            procs, _ = self._all_processes(h)
            out.append({"gi": self._get(lib.nvmlDeviceGetGpuInstanceId, h, ctypes.c_uint),
                        "ci": self._get(lib.nvmlDeviceGetComputeInstanceId, h, ctypes.c_uint),
                        "mem_used": used, "mem_total": total, "procs": procs})
        return out

    def _sm_util(self, handle, index):
        """{pid: SM%} averaged since the last poll, or None if the driver can't tell."""
        fn = self._fn("nvmlDeviceGetProcessUtilization")
        if fn is None:
            return None
        since = self.sm_seen.get(index) or int(time.time() * 1e6) - 1000000
        size = ctypes.c_uint(128)
        arr = (ProcessUtilSample * size.value)()
        ret = fn(handle, arr, ctypes.byref(size), ctypes.c_ulonglong(since))
        if ret == NVML_ERROR_INSUFFICIENT_SIZE:
            arr = (ProcessUtilSample * size.value)()
            ret = fn(handle, arr, ctypes.byref(size), ctypes.c_ulonglong(since))
        if ret == NVML_ERROR_NOT_FOUND:
            return {}  # no samples since last time: nobody computed
        if ret != NVML_SUCCESS:
            return None
        sm, self.sm_seen[index] = average_sm(
            [(arr[i].pid, arr[i].timeStamp, arr[i].smUtil) for i in range(size.value)], since)
        return sm

    def _gpu(self, index):
        lib = self.lib
        handle = ctypes.c_void_p()
        ret = self._fn("nvmlDeviceGetHandleByIndex_v2", "nvmlDeviceGetHandleByIndex")(index, ctypes.byref(handle))
        if ret != NVML_SUCCESS:
            raise GpuLost("GPU is lost" if ret == NVML_ERROR_GPU_IS_LOST else "NVML: %s" % self.errstr(ret))

        buf = ctypes.create_string_buffer(96)
        ret = lib.nvmlDeviceGetName(handle, buf, 96)
        if ret in (NVML_ERROR_GPU_IS_LOST, NVML_ERROR_UNKNOWN):  # gpustat's InvalidGPU cases
            raise GpuLost("GPU is lost" if ret == NVML_ERROR_GPU_IS_LOST else "Unknown Error")
        name = buf.value.decode("utf-8", "replace") if ret == NVML_SUCCESS else "?"
        uuid = buf.value.decode("utf-8", "replace") if lib.nvmlDeviceGetUUID(handle, buf, 96) == NVML_SUCCESS \
            else None

        util = Utilization()
        util_ok = lib.nvmlDeviceGetUtilizationRates(handle, ctypes.byref(util)) == NVML_SUCCESS
        power = self._get(lib.nvmlDeviceGetPowerUsage, handle, ctypes.c_uint)
        limit = self._get(lib.nvmlDeviceGetEnforcedPowerLimit, handle, ctypes.c_uint)
        used, total = self._memory(handle)
        procs, supported = self._all_processes(handle)

        mig = self._mig(handle)
        if mig:
            seen = set(p["pid"] for p in procs)
            for inst in mig:
                procs.extend(p for p in inst["procs"] if p["pid"] not in seen)
                seen.update(p["pid"] for p in inst["procs"])
            supported = supported or any(inst["procs"] is not None for inst in mig)
            if used is None and all(inst["mem_used"] is not None for inst in mig):
                used = sum(inst["mem_used"] for inst in mig)
                total = sum(inst["mem_total"] for inst in mig)
        elif sum(1 for p in procs if p["type"] != "G") >= 2:
            # several jobs share this GPU: which one is computing and which only holds memory?
            sm = self._sm_util(handle, index)
            if sm is not None:
                for p in procs:
                    if p["type"] != "G":
                        p["sm"] = sm.get(p["pid"], 0)

        return {
            "index": index, "uuid": uuid, "name": name,
            "temp": self._get(lib.nvmlDeviceGetTemperature, handle, ctypes.c_uint, 0),  # NVML_TEMPERATURE_GPU
            "fan": self._fan(handle, index),
            "util": util.gpu if util_ok else None,
            "mem_util": util.memory if util_ok else None,
            "enc": self._codec(lib.nvmlDeviceGetEncoderUtilization, handle),
            "dec": self._codec(lib.nvmlDeviceGetDecoderUtilization, handle),
            "mem_used": used, "mem_total": total,
            "power": power / 1000.0 if power is not None else None,
            "power_limit": limit / 1000.0 if limit is not None else None,
            "procs": procs,
            "procs_supported": supported,
            "mig": mig,
        }

    def _codec(self, fn, handle):
        util, period = ctypes.c_uint(), ctypes.c_uint()
        return util.value if fn(handle, ctypes.byref(util), ctypes.byref(period)) == NVML_SUCCESS else None

    def _collect(self):
        if self.status in ("init", "absent", "error") and time.monotonic() >= self.retry_at:
            self._init()
        if self.status != "ok":
            return []
        count = ctypes.c_uint()
        ret = self._fn("nvmlDeviceGetCount_v2", "nvmlDeviceGetCount")(ctypes.byref(count))
        if ret != NVML_SUCCESS:
            if ret in NVML_REINIT:  # driver reloaded or upgraded under us: start over
                self.lib.nvmlShutdown()
                self._fail(ret, "nvmlDeviceGetCount")
                return []
            raise RuntimeError("nvmlDeviceGetCount: %s" % self.errstr(ret))
        gpus = []
        for i in range(count.value):
            try:
                gpus.append(self._gpu(i))
            except GpuLost as e:  # show the broken GPU instead of failing the whole host
                gpus.append({"index": i, "name": "((%s))" % e, "invalid": True, "procs": []})
        return gpus

    def query(self):
        """(gpus, error). Runs in a worker thread so a hung driver can't freeze the agent."""
        if self.worker is not None:
            if self.worker.is_alive():
                return [], "NVML not responding (driver stuck)"
            self.worker = None
        box = {}

        def run():
            try:
                box["gpus"] = self._collect()
            except Exception as e:
                box["error"] = "%s: %s" % (type(e).__name__, e)

        t = threading.Thread(target=run)
        t.daemon = True
        t.start()
        t.join(NVML_TIMEOUT)
        if t.is_alive():
            self.worker = t
            return [], "NVML timed out after %ds" % NVML_TIMEOUT
        if "error" in box:
            return [], box["error"]
        return box["gpus"], self.error


# ---------------------------------------------------------------- claims

CLAIMS_DIR = "/var/tmp/gpuwho-claims"


def read_claims(now=None, directory=CLAIMS_DIR):
    """Soft reservations (`gpuwho claim`): one JSON file per user in a sticky world-writable
    directory. The owner is the file's owner, not its name, so nobody can claim as someone else."""
    now = time.time() if now is None else now
    out = []
    try:
        names = os.listdir(directory)
    except OSError:
        return out
    for name in names:
        if not name.endswith(".json"):
            continue
        path = os.path.join(directory, name)
        try:
            uid = os.stat(path).st_uid
            doc = json.loads(read(path) or "{}")
        except (OSError, ValueError):
            continue
        user = owner_name(uid)
        for c in doc.get("claims", []) if isinstance(doc, dict) else []:
            try:
                if float(c["until"]) > now:
                    out.append({"user": user, "gpus": [int(g) for g in c["gpus"]], "until": float(c["until"]),
                                "since": c.get("since"), "note": str(c.get("note") or "")[:200]})
            except (KeyError, TypeError, ValueError):
                continue
    return out


# ---------------------------------------------------------------- sampling

def significant(pct, rss_pages):
    """Worth listing in the per-user summary: busy, or holding at least 512 MiB."""
    return pct >= 1 or rss_pages * PAGE_KB >= 512 * 1024


class Sampler:
    def __init__(self):
        self.prev = None   # (monotonic time, cpu total, cpu idle, proc_stats()) of the last poll
        self.last_full = None  # (monotonic time, proc_stats()) of the last full scan
        self.polls = 0
        self.watch = []    # pids re-read between full scans
        self.nvml = Nvml()
        model = re.search(r"model name\s*:\s*(.+)", read("/proc/cpuinfo"))
        self.model = model.group(1).strip() if model else ""

    def sample(self):
        if self.prev is None:  # first poll: take a short baseline for CPU%
            self.prev = (time.monotonic(),) + cpu_times() + (proc_stats(),)
            self.last_full = (self.prev[0], self.prev[3])
            time.sleep(FIRST_SAMPLE_SEC)
        full = self.polls % FULL_SCAN_EVERY == 0
        self.polls += 1
        now = time.monotonic()
        t1, i1 = cpu_times()
        p1 = proc_stats(None if full else self.watch)
        t_prev, t0, i0, p0 = self.prev

        cpu_usage = 100.0 * (1 - (i1 - i0) / max(t1 - t0, 1))
        wall_ticks = max(now - t_prev, 0.1) * CLK_TCK

        # per-process CPU% where 100% == one full core; a reused pid starts over. Processes
        # that weren't watched since the last full scan are measured against that scan.
        full_at, full_stats = self.last_full
        full_ticks = max(now - full_at, 0.1) * CLK_TCK
        per_pid_cpu, users, groups, keep = {}, {}, {}, []
        for pid, (_, ticks, rss, start) in p1.items():
            prev, span = p0.get(pid), wall_ticks
            if not prev or prev[3] != start:
                prev, span = full_stats.get(pid), full_ticks
            pct = max(0.0, 100.0 * (ticks - prev[1]) / span) if prev and prev[3] == start else 0.0
            per_pid_cpu[pid] = round(pct, 1)
            if not significant(pct, rss):
                continue  # ignore idle, small processes in the user summary
            u = owner(pid)
            if u is None:
                continue
            keep.append(str(pid))
            agg = users.setdefault(u, {"user": u, "cpu": 0.0, "rss_kb": 0, "nproc": 0})
            agg["cpu"] += pct
            agg["rss_kb"] += rss * PAGE_KB
            agg["nproc"] += 1
            shares = groups.setdefault(memcg_of(pid), {})
            shares[u] = shares.get(u, 0) + rss * PAGE_KB
        for u, kb in split_by_rss(groups, lambda d: charged_kb(read(d + "/memory.stat"))).items():
            users[u]["mem_kb"] = int(kb)

        gpus, gpu_err = self.nvml.query()
        gpu_pids = [p["pid"] for g in gpus for p in g["procs"]]
        missing = [str(pid) for pid in gpu_pids if pid not in p1]
        if missing:  # GPU jobs that started since the last full scan
            p1.update(proc_stats(missing))
        for g in gpus:
            for p in g["procs"]:
                pid = p["pid"]
                try:
                    user = owner(pid)
                    p.update(user=user, cpu=per_pid_cpu.get(pid), container=container_of(pid), cmd=cmdline(pid),
                             rss_kb=p1[pid][2] * PAGE_KB if pid in p1 else None, state=proc_state(pid, p1, user),
                             start=p1[pid][3] if pid in p1 else None)
                except Exception as e:  # one odd process must not blank the whole server
                    p.setdefault("state", "error: %s" % e)

        if full:
            self.watch = keep
            self.last_full = (now, p1)
        self.watch = sorted(set(self.watch) | set(str(pid) for pid in gpu_pids))
        self.prev = (now, t1, i1, p1)

        mem = {}
        for line in read("/proc/meminfo").splitlines():
            k, v = line.split(":", 1)
            mem[k] = int(v.split()[0])

        return {
            "hostname": socket.gethostname(),
            "time": time.time(),
            "uptime": num(read("/proc/uptime").split()[0]),
            "cpu": {
                "count": os.cpu_count() or 1, "usage": round(cpu_usage, 1),
                "load": [num(x) for x in read("/proc/loadavg").split()[:3]],
                "model": self.model,
            },
            "mem": {"total_kb": mem.get("MemTotal", 0), "avail_kb": mem.get("MemAvailable", 0)},
            "users": sorted(users.values(), key=lambda x: -x["cpu"]),
            "gpus": gpus,
            "gpu_error": gpu_err,
            "nvml": self.nvml.status,
            "driver": self.nvml.driver,
            "cuda": self.nvml.cuda,
            "claims": read_claims(),
            "clk_tck": CLK_TCK,
            "agent": {"pid": os.getpid(), "full_scan": full, "impl": "python"},
        }


def main():
    idle_sec = float(sys.argv[1]) if len(sys.argv) > 1 else 90
    sampler = Sampler()
    started = time.time()
    stdin = sys.stdin.fileno()
    while time.time() - started < MAX_LIFETIME:
        if not select.select([stdin], [], [], idle_sec)[0]:
            break  # client went quiet (e.g. network dropped): don't linger
        if not os.read(stdin, 4096):
            break  # EOF: client closed the connection
        try:
            out = sampler.sample()
        except Exception as e:  # report instead of dying, the client shows it
            out = {"fatal": "%s: %s" % (type(e).__name__, e)}
        sys.stdout.write(json.dumps(out) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":  # also true when piped in through `python3 -c`
    try:
        main()
    except (BrokenPipeError, KeyboardInterrupt):
        pass
