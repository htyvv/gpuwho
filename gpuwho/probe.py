"""Remote agent for gpuwho.

Started once per SSH connection with `python3 -u -c <this file> IDLE_SEC`. Protocol: the
client writes one line per poll and the agent answers with one JSON line. Must stay
stdlib-only and Python 3.6 compatible.

GPU data comes straight from NVML via ctypes (the approach gpustat takes with pynvml)
instead of spawning nvidia-smi: one nvmlInit for the agent's lifetime, no fork/exec per
poll, and GPUs without persistence mode are not re-initialised every poll.

Built to be left running without bothering the server:
  * /proc is scanned once per poll; CPU% is the delta since the previous poll
  * NVML runs in a worker thread with a timeout; while a call is stuck in the driver
    no new call is made, CPU/RAM keep being reported
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
    try:
        with open(path) as f:
            return f.read()
    except OSError:
        return ""


def cpu_times():
    vals = [int(v) for v in read("/proc/stat").splitlines()[0].split()[1:]]
    idle = vals[3] + (vals[4] if len(vals) > 4 else 0)
    return sum(vals), idle


def proc_ticks():
    out = {}
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        stat = read("/proc/%s/stat" % pid)
        if not stat:
            continue
        rest = stat.rsplit(")", 1)[-1].split()
        try:
            out[int(pid)] = (int(rest[11]) + int(rest[12]), int(rest[21]))  # utime+stime, rss pages
        except (IndexError, ValueError):
            pass
    return out


_uid_cache = {}


def owner(pid):
    try:
        uid = os.stat("/proc/%d" % pid).st_uid
    except OSError:
        return None
    if uid not in _uid_cache:
        try:
            _uid_cache[uid] = pwd.getpwuid(uid).pw_name
        except KeyError:
            _uid_cache[uid] = str(uid)
    return _uid_cache[uid]


def container_id(cgroup):
    """Short container id from /proc/PID/cgroup text (docker, containerd, podman, cri-o)."""
    m = re.search(r"(?:docker[-/]|containerd[-/]|libpod-|crio-)([0-9a-f]{12})", cgroup)
    return m.group(1) if m else None


def container_of(pid):
    return container_id(read("/proc/%d/cgroup" % pid))


def cmdline(pid):
    return read("/proc/%d/cmdline" % pid).replace("\x00", " ").strip()[:300]


# ---------------------------------------------------------------- NVML (ctypes)

NVML_SUCCESS = 0
NVML_ERROR_DRIVER_NOT_LOADED = 9
NVML_ERROR_INSUFFICIENT_SIZE = 7
NVML_ERROR_LIBRARY_NOT_FOUND = 12
NVML_ERROR_GPU_IS_LOST = 15
NVML_ERROR_UNKNOWN = 999
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


class GpuLost(Exception):
    pass


def merge_procs(compute, graphics):
    """List each pid once (a process can be both compute and graphics, as gpustat notes),
    typed C / G / C+G like nvidia-smi so the client can tell a parked Xorg from real work."""
    procs, seen = [], {}
    for kind, found in (("C", compute), ("G", graphics)):
        for pid, used in found or []:
            if pid in seen:
                seen[pid]["type"] = "C+G"
            else:
                seen[pid] = {"pid": pid, "mem": used, "type": kind}
                procs.append(seen[pid])
    return procs


class Nvml:
    """Minimal NVML binding. Every field is optional, like gpustat's safe calls."""

    def __init__(self):
        self.lib = None
        self.status = "init"   # init | ok | absent | error
        self.error = None
        self.driver = None
        self.cuda = None
        self.worker = None     # thread stuck inside the driver, if any

    def _fn(self, *names):
        """First available symbol, for APIs that gained _v2/_v3 variants over driver versions."""
        for name in names:
            try:
                return getattr(self.lib, name)
            except AttributeError:
                continue
        return None

    def _init(self):
        try:
            self.lib = ctypes.CDLL("libnvidia-ml.so.1")
        except OSError:
            self.status = "absent"
            return
        init = self._fn("nvmlInit_v2", "nvmlInit")
        ret = init()
        if ret in (NVML_ERROR_DRIVER_NOT_LOADED, NVML_ERROR_LIBRARY_NOT_FOUND):
            self.status = "absent"
            return
        if ret != NVML_SUCCESS:
            self.status, self.error = "error", "nvmlInit failed (code %d)" % ret
            return
        buf = ctypes.create_string_buffer(96)
        if self.lib.nvmlSystemGetDriverVersion(buf, 96) == NVML_SUCCESS:
            self.driver = buf.value.decode()
        v = ctypes.c_int()
        fn = self._fn("nvmlSystemGetCudaDriverVersion_v2", "nvmlSystemGetCudaDriverVersion")
        if fn and fn(ctypes.byref(v)) == NVML_SUCCESS:
            self.cuda = "%d.%d" % (v.value // 1000, v.value % 1000 // 10)
        self.status = "ok"

    def _get(self, fn, handle, ctype, *extra):
        """Call fn(handle, *extra, &out); None if unsupported/failed. Raises GpuLost."""
        out = ctype()
        ret = fn(handle, *(extra + (ctypes.byref(out),)))
        if ret == NVML_ERROR_GPU_IS_LOST:
            raise GpuLost("GPU is lost")
        return out if ret == NVML_SUCCESS else None

    def _processes(self, handle, kind):
        """Compute or graphics processes, trying the _v3, _v2, then v1 API (cf. gpustat #107)."""
        for suffix, struct in (("_v3", ProcessV2), ("_v2", ProcessV2), ("", ProcessV1)):
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

    def _gpu(self, index):
        lib = self.lib
        handle = ctypes.c_void_p()
        ret = self._fn("nvmlDeviceGetHandleByIndex_v2", "nvmlDeviceGetHandleByIndex")(index, ctypes.byref(handle))
        if ret != NVML_SUCCESS:
            raise GpuLost("GPU is lost" if ret == NVML_ERROR_GPU_IS_LOST else "NVML error %d" % ret)

        buf = ctypes.create_string_buffer(96)
        ret = lib.nvmlDeviceGetName(handle, buf, 96)
        if ret in (NVML_ERROR_GPU_IS_LOST, NVML_ERROR_UNKNOWN):  # gpustat's InvalidGPU cases
            raise GpuLost("GPU is lost" if ret == NVML_ERROR_GPU_IS_LOST else "Unknown Error")
        name = buf.value.decode() if ret == NVML_SUCCESS else "?"
        uuid = buf.value.decode() if lib.nvmlDeviceGetUUID(handle, buf, 96) == NVML_SUCCESS else None

        temp = self._get(lib.nvmlDeviceGetTemperature, handle, ctypes.c_uint, 0)  # NVML_TEMPERATURE_GPU
        fan = self._get(lib.nvmlDeviceGetFanSpeed, handle, ctypes.c_uint)
        util = self._get(lib.nvmlDeviceGetUtilizationRates, handle, Utilization)
        power = self._get(lib.nvmlDeviceGetPowerUsage, handle, ctypes.c_uint)
        limit = self._get(lib.nvmlDeviceGetEnforcedPowerLimit, handle, ctypes.c_uint)
        enc, dec = (self._codec(fn, handle) for fn in (lib.nvmlDeviceGetEncoderUtilization,
                                                        lib.nvmlDeviceGetDecoderUtilization))

        # v2 excludes driver-reserved memory, matching nvidia-smi on drivers >= 515 (cf. gpustat #141)
        mem = None
        fn_v2 = self._fn("nvmlDeviceGetMemoryInfo_v2")
        if fn_v2 is not None:
            mem = MemoryV2(version=NVML_MEMORY_V2)
            if fn_v2(handle, ctypes.byref(mem)) != NVML_SUCCESS:
                mem = None
        if mem is None:
            mem = self._get(lib.nvmlDeviceGetMemoryInfo, handle, Memory)

        compute, graphics = self._processes(handle, "GetCompute"), self._processes(handle, "GetGraphics")
        procs = merge_procs(compute, graphics)

        return {
            "index": index, "uuid": uuid, "name": name,
            "temp": temp.value if temp else None,
            "fan": fan.value if fan else None,
            "util": util.gpu if util else None,
            "mem_util": util.memory if util else None,
            "enc": enc, "dec": dec,
            "mem_used": mem.used / MIB if mem else None,
            "mem_total": mem.total / MIB if mem else None,
            "power": power.value / 1000.0 if power else None,
            "power_limit": limit.value / 1000.0 if limit else None,
            "procs": procs,
            "procs_supported": compute is not None or graphics is not None,
        }

    def _codec(self, fn, handle):
        util, period = ctypes.c_uint(), ctypes.c_uint()
        return util.value if fn(handle, ctypes.byref(util), ctypes.byref(period)) == NVML_SUCCESS else None

    def _collect(self):
        if self.status == "init":
            self._init()
        if self.status != "ok":
            return []
        count = ctypes.c_uint()
        ret = self._fn("nvmlDeviceGetCount_v2", "nvmlDeviceGetCount")(ctypes.byref(count))
        if ret != NVML_SUCCESS:
            raise RuntimeError("nvmlDeviceGetCount failed (code %d)" % ret)
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


# ---------------------------------------------------------------- sampling

class Sampler:
    def __init__(self):
        self.prev = None  # (wall time, cpu total, cpu idle, {pid: (ticks, rss)})
        self.nvml = Nvml()

    def sample(self):
        if self.prev is None:  # first poll: take a short baseline for CPU%
            self.prev = (time.time(),) + cpu_times() + (proc_ticks(),)
            time.sleep(FIRST_SAMPLE_SEC)
        now = time.time()
        t1, i1 = cpu_times()
        p1 = proc_ticks()
        t_prev, t0, i0, p0 = self.prev
        self.prev = (now, t1, i1, p1)

        cpu_usage = 100.0 * (1 - (i1 - i0) / max(t1 - t0, 1))
        wall_ticks = max(now - t_prev, 0.1) * CLK_TCK

        # per-process CPU% where 100% == one full core
        per_pid_cpu, users = {}, {}
        for pid, (ticks, rss) in p1.items():
            prev = p0.get(pid)
            pct = max(0.0, 100.0 * (ticks - prev[0]) / wall_ticks) if prev else 0.0
            per_pid_cpu[pid] = round(pct, 1)
            if pct < 1 and rss * PAGE_KB < 512 * 1024:
                continue  # ignore idle, small processes in the user summary
            u = owner(pid)
            if u is None:
                continue
            agg = users.setdefault(u, {"user": u, "cpu": 0.0, "rss_kb": 0, "nproc": 0})
            agg["cpu"] += pct
            agg["rss_kb"] += rss * PAGE_KB
            agg["nproc"] += 1

        gpus, gpu_err = self.nvml.query()
        for g in gpus:
            for p in g["procs"]:
                pid = p["pid"]
                p.update(user=owner(pid), cpu=per_pid_cpu.get(pid), container=container_of(pid),
                         cmd=cmdline(pid), rss_kb=p1[pid][1] * PAGE_KB if pid in p1 else None)

        mem = {}
        for line in read("/proc/meminfo").splitlines():
            k, v = line.split(":", 1)
            mem[k] = int(v.split()[0])
        model = re.search(r"model name\s*:\s*(.+)", read("/proc/cpuinfo"))

        return {
            "hostname": socket.gethostname(),
            "time": now,
            "uptime": num(read("/proc/uptime").split()[0]),
            "cpu": {
                "count": os.cpu_count() or 1, "usage": round(cpu_usage, 1),
                "load": [num(x) for x in read("/proc/loadavg").split()[:3]],
                "model": model.group(1).strip() if model else "",
            },
            "mem": {"total_kb": mem.get("MemTotal", 0), "avail_kb": mem.get("MemAvailable", 0)},
            "users": sorted(users.values(), key=lambda x: -x["cpu"]),
            "gpus": gpus,
            "gpu_error": gpu_err,
            "nvml": self.nvml.status,
            "driver": self.nvml.driver,
            "cuda": self.nvml.cuda,
            "agent": {"pid": os.getpid()},
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
