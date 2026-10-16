//! A small NVML binding over dlopen, the way gpustat and nvitop use it: every field is
//! optional, every versioned API falls back to the older one, a lost GPU becomes one red
//! row instead of failing the whole server.
//!
//! The expensive calls are cached (measured on 8x A6000): fan speed 1-2 ms -> 10 s TTL,
//! per-process utilization 4 ms per GPU -> 9 s TTL and only when 2+ jobs share a GPU,
//! MIG mode -> 60 s, MPS process lists are skipped for 60 s once a GPU reports none.

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uint, c_ulonglong, c_void};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use libloading::Library;
use serde_json::{json, Value};

type Ret = c_int;
type Device = *mut c_void;

const SUCCESS: Ret = 0;
const ERROR_UNINITIALIZED: Ret = 1;
const ERROR_NOT_FOUND: Ret = 6;
const ERROR_INSUFFICIENT_SIZE: Ret = 7;
const ERROR_DRIVER_NOT_LOADED: Ret = 9;
const ERROR_LIBRARY_NOT_FOUND: Ret = 12;
const ERROR_GPU_IS_LOST: Ret = 15;
const ERROR_LIB_RM_VERSION_MISMATCH: Ret = 18;
const ERROR_UNKNOWN: Ret = 999;
const NOT_AVAILABLE: u64 = u64::MAX; // usedGpuMemory when the driver can't tell
const MEMORY_V2: u32 = 0x0200_0028; // sizeof(nvmlMemory_v2_t) | (2 << 24)
const MIB: f64 = 1024.0 * 1024.0;

pub const TIMEOUT: Duration = Duration::from_secs(15);
const RETRY: Duration = Duration::from_secs(60);
const FAN_TTL: Duration = Duration::from_secs(10);
const SM_TTL: Duration = Duration::from_secs(9);
const STATIC_TTL: Duration = Duration::from_secs(60);

#[repr(C)]
#[derive(Default)]
struct Memory {
    total: u64,
    free: u64,
    used: u64,
}

#[repr(C)]
#[derive(Default)]
struct MemoryV2 {
    version: u32,
    total: u64,
    reserved: u64,
    free: u64,
    used: u64,
}

#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: u32,
    memory: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ProcUtilSample {
    pid: u32,
    ts: u64,
    sm: u32,
    mem: u32,
    enc: u32,
    dec: u32,
}

type FnRet = unsafe extern "C" fn() -> Ret;
type FnErrStr = unsafe extern "C" fn(Ret) -> *const c_char;
type FnSysStr = unsafe extern "C" fn(*mut c_char, c_uint) -> Ret;
type FnSysI32 = unsafe extern "C" fn(*mut c_int) -> Ret;
type FnCount = unsafe extern "C" fn(*mut c_uint) -> Ret;
type FnHandle = unsafe extern "C" fn(c_uint, *mut Device) -> Ret;
type FnDevStr = unsafe extern "C" fn(Device, *mut c_char, c_uint) -> Ret;
type FnTemp = unsafe extern "C" fn(Device, c_int, *mut c_uint) -> Ret;
type FnDevU32 = unsafe extern "C" fn(Device, *mut c_uint) -> Ret;
type FnDevU32x2 = unsafe extern "C" fn(Device, *mut c_uint, *mut c_uint) -> Ret;
type FnUtil = unsafe extern "C" fn(Device, *mut Utilization) -> Ret;
type FnMem = unsafe extern "C" fn(Device, *mut c_void) -> Ret;
type FnProcs = unsafe extern "C" fn(Device, *mut c_uint, *mut c_void) -> Ret;
type FnMigHandle = unsafe extern "C" fn(Device, c_uint, *mut Device) -> Ret;
type FnProcUtil =
    unsafe extern "C" fn(Device, *mut ProcUtilSample, *mut c_uint, c_ulonglong) -> Ret;

fn sym<T: Copy>(lib: &Library, names: &[&str]) -> Option<T> {
    names.iter().find_map(|n| {
        let name = format!("{}\0", n);
        unsafe { lib.get::<T>(name.as_bytes()) }.ok().map(|s| *s)
    })
}

/// Process-list entry points per kind: (symbol, struct size) newest first.
struct ProcApi {
    fns: Vec<(FnProcs, usize)>,
}

struct Api {
    _lib: Library,
    init: FnRet,
    shutdown: Option<FnRet>,
    err_str: Option<FnErrStr>,
    driver: Option<FnSysStr>,
    cuda: Option<FnSysI32>,
    count: FnCount,
    handle: FnHandle,
    name: Option<FnDevStr>,
    uuid: Option<FnDevStr>,
    temp: Option<FnTemp>,
    fan: Option<FnDevU32>,
    util: Option<FnUtil>,
    power: Option<FnDevU32>,
    limit: Option<FnDevU32>,
    enc: Option<FnDevU32x2>,
    dec: Option<FnDevU32x2>,
    mem_v2: Option<FnMem>,
    mem_v1: Option<FnMem>,
    compute: ProcApi,
    graphics: ProcApi,
    mps: ProcApi,
    mig_mode: Option<FnDevU32x2>,
    mig_max: Option<FnDevU32>,
    mig_handle: Option<FnMigHandle>,
    gi: Option<FnDevU32>,
    ci: Option<FnDevU32>,
    proc_util: Option<FnProcUtil>,
}

impl Api {
    fn load() -> Option<Api> {
        let lib = unsafe { Library::new("libnvidia-ml.so.1") }.ok()?;
        // 535.43-535.86 shipped a 32-byte _v3 process struct; detected the way nvitop does
        let legacy_v3 = sym::<FnRet>(&lib, &["nvmlDeviceGetConfComputeMemSizeInfo"]).is_some()
            && sym::<FnRet>(&lib, &["nvmlDeviceGetRunningProcessDetailList"]).is_none();
        let procs = |kind: &str| ProcApi {
            fns: [
                ("_v3", if legacy_v3 { 32 } else { 24 }),
                ("_v2", 24),
                ("", 16),
            ]
            .iter()
            .filter_map(|(suffix, size)| {
                sym::<FnProcs>(
                    &lib,
                    &[&format!("nvmlDevice{}RunningProcesses{}", kind, suffix)],
                )
                .map(|f| (f, *size))
            })
            .collect(),
        };
        Some(Api {
            init: sym(&lib, &["nvmlInit_v2", "nvmlInit"])?,
            shutdown: sym(&lib, &["nvmlShutdown"]),
            err_str: sym(&lib, &["nvmlErrorString"]),
            driver: sym(&lib, &["nvmlSystemGetDriverVersion"]),
            cuda: sym(
                &lib,
                &[
                    "nvmlSystemGetCudaDriverVersion_v2",
                    "nvmlSystemGetCudaDriverVersion",
                ],
            ),
            count: sym(&lib, &["nvmlDeviceGetCount_v2", "nvmlDeviceGetCount"])?,
            handle: sym(
                &lib,
                &[
                    "nvmlDeviceGetHandleByIndex_v2",
                    "nvmlDeviceGetHandleByIndex",
                ],
            )?,
            name: sym(&lib, &["nvmlDeviceGetName"]),
            uuid: sym(&lib, &["nvmlDeviceGetUUID"]),
            temp: sym(&lib, &["nvmlDeviceGetTemperature"]),
            fan: sym(&lib, &["nvmlDeviceGetFanSpeed"]),
            util: sym(&lib, &["nvmlDeviceGetUtilizationRates"]),
            power: sym(&lib, &["nvmlDeviceGetPowerUsage"]),
            limit: sym(&lib, &["nvmlDeviceGetEnforcedPowerLimit"]),
            enc: sym(&lib, &["nvmlDeviceGetEncoderUtilization"]),
            dec: sym(&lib, &["nvmlDeviceGetDecoderUtilization"]),
            mem_v2: sym(&lib, &["nvmlDeviceGetMemoryInfo_v2"]),
            mem_v1: sym(&lib, &["nvmlDeviceGetMemoryInfo"]),
            compute: procs("GetCompute"),
            graphics: procs("GetGraphics"),
            mps: procs("GetMPSCompute"),
            mig_mode: sym(&lib, &["nvmlDeviceGetMigMode"]),
            mig_max: sym(&lib, &["nvmlDeviceGetMaxMigDeviceCount"]),
            mig_handle: sym(&lib, &["nvmlDeviceGetMigDeviceHandleByIndex"]),
            gi: sym(&lib, &["nvmlDeviceGetGpuInstanceId"]),
            ci: sym(&lib, &["nvmlDeviceGetComputeInstanceId"]),
            proc_util: sym(&lib, &["nvmlDeviceGetProcessUtilization"]),
            _lib: lib,
        })
    }

    fn err(&self, ret: Ret) -> String {
        match self.err_str {
            Some(f) => {
                let p = unsafe { f(ret) };
                if p.is_null() {
                    format!("code {}", ret)
                } else {
                    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
                }
            }
            None => format!("code {}", ret),
        }
    }

    fn u32_of(&self, f: Option<FnDevU32>, dev: Device) -> Result<Option<u32>, Lost> {
        let Some(f) = f else { return Ok(None) };
        let mut v: c_uint = 0;
        match unsafe { f(dev, &mut v) } {
            SUCCESS => Ok(Some(v)),
            ERROR_GPU_IS_LOST => Err(Lost("GPU is lost".into())),
            _ => Ok(None),
        }
    }

    fn string_of(&self, f: Option<FnDevStr>, dev: Device) -> Result<Option<String>, Ret> {
        let Some(f) = f else { return Ok(None) };
        let mut buf = [0 as c_char; 96];
        match unsafe { f(dev, buf.as_mut_ptr(), buf.len() as c_uint) } {
            SUCCESS => Ok(Some(
                unsafe { CStr::from_ptr(buf.as_ptr()) }
                    .to_string_lossy()
                    .into_owned(),
            )),
            ret => Err(ret),
        }
    }

    fn memory(&self, dev: Device) -> (Option<f64>, Option<f64>) {
        // v2 excludes driver-reserved memory, matching nvidia-smi on drivers >= 515 (gpustat #141)
        if let Some(f) = self.mem_v2 {
            let mut m = MemoryV2 {
                version: MEMORY_V2,
                ..Default::default()
            };
            if unsafe { f(dev, &mut m as *mut MemoryV2 as *mut c_void) } == SUCCESS {
                return (Some(m.used as f64 / MIB), Some(m.total as f64 / MIB));
            }
        }
        if let Some(f) = self.mem_v1 {
            let mut m = Memory::default();
            if unsafe { f(dev, &mut m as *mut Memory as *mut c_void) } == SUCCESS {
                return (Some(m.used as f64 / MIB), Some(m.total as f64 / MIB));
            }
        }
        (None, None)
    }

    /// (pid, MiB or None) for one kind of process list, from the newest API the driver has;
    /// None if unsupported.
    fn processes(&self, api: &ProcApi, dev: Device) -> Option<Vec<(u32, Option<f64>)>> {
        let &(f, size) = api.fns.first()?;
        let mut count: c_uint = 0;
        let ret = unsafe { f(dev, &mut count, std::ptr::null_mut()) };
        if ret == SUCCESS {
            return Some(vec![]);
        }
        if ret != ERROR_INSUFFICIENT_SIZE {
            return None;
        }
        count = count * 2 + 5; // processes may start between the two calls
        let mut buf = vec![0u8; count as usize * size];
        if unsafe { f(dev, &mut count, buf.as_mut_ptr() as *mut c_void) } != SUCCESS {
            return None;
        }
        // every layout starts with pid (u32) and usedGpuMemory (u64 at offset 8)
        Some(
            (0..count as usize)
                .map(|i| {
                    let e = &buf[i * size..(i + 1) * size];
                    let pid = u32::from_ne_bytes(e[0..4].try_into().unwrap());
                    let used = u64::from_ne_bytes(e[8..16].try_into().unwrap());
                    (pid, (used != NOT_AVAILABLE).then(|| used as f64 / MIB))
                })
                .collect(),
        )
    }
}

struct Lost(String);

/// One entry per pid (a process can be both compute and graphics), typed C / G / C+G like
/// nvidia-smi, so the client can tell a parked Xorg from real work.
pub fn merge_procs(
    compute: Option<&[(u32, Option<f64>)]>,
    graphics: Option<&[(u32, Option<f64>)]>,
) -> Vec<Value> {
    let mut order: Vec<u32> = Vec::new();
    let mut seen: HashMap<u32, (Option<f64>, &'static str)> = HashMap::new();
    for (kind, list) in [("C", compute), ("G", graphics)] {
        for &(pid, mem) in list.unwrap_or(&[]) {
            match seen.get_mut(&pid) {
                Some(e) if e.1 != kind => e.1 = "C+G",
                Some(_) => {}
                None => {
                    seen.insert(pid, (mem, kind));
                    order.push(pid);
                }
            }
        }
    }
    order
        .iter()
        .map(|pid| json!({"pid": pid, "mem": seen[pid].0, "type": seen[pid].1}))
        .collect()
}

/// {pid: mean SM%} of samples newer than `since` (microseconds), and the newest timestamp.
/// Values above 100 are driver noise and dropped, as nvtop does.
pub fn average_sm(samples: &[(u32, u64, u32)], since: u64) -> (HashMap<u32, u32>, u64) {
    let mut acc: HashMap<u32, (u64, u64)> = HashMap::new();
    let mut newest = since;
    for &(pid, ts, sm) in samples {
        if ts <= since || sm > 100 {
            continue;
        }
        let e = acc.entry(pid).or_default();
        e.0 += sm as u64;
        e.1 += 1;
        newest = newest.max(ts);
    }
    (
        acc.into_iter()
            .map(|(pid, (sum, n))| (pid, ((sum as f64) / n as f64).round() as u32))
            .collect(),
        newest,
    )
}

fn nvidia_on_pci() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/bus/pci/devices") else {
        return false;
    };
    dir.flatten().any(|e| {
        let p = e.path();
        let vendor = crate::procfs::read(&p.join("vendor").to_string_lossy());
        let class = crate::procfs::read(&p.join("class").to_string_lossy());
        vendor.trim() == "0x10de" && class.trim().starts_with("0x03")
    })
}

pub struct Nvml {
    api: Option<Api>,
    pub status: &'static str, // init | ok | absent | error
    pub error: Option<String>,
    retry_at: Option<Instant>,
    pub driver: Option<String>,
    pub cuda: Option<String>,
    fans: HashMap<u32, (Instant, Option<u32>)>,
    sm: HashMap<u32, (Instant, u64, HashMap<u32, u32>)>,
    mig_mode: HashMap<u32, (Instant, bool)>,
    mps_skip: HashMap<u32, Instant>,
}

// Device handles never outlive a call, and only one thread uses NVML at a time (Mutex below).
unsafe impl Send for Nvml {}

impl Nvml {
    pub fn new() -> Self {
        Nvml {
            api: None,
            status: "init",
            error: None,
            retry_at: None,
            driver: None,
            cuda: None,
            fans: HashMap::new(),
            sm: HashMap::new(),
            mig_mode: HashMap::new(),
            mps_skip: HashMap::new(),
        }
    }

    fn fail(&mut self, ret: Ret, what: &str) {
        self.retry_at = Some(Instant::now() + RETRY);
        if matches!(ret, ERROR_DRIVER_NOT_LOADED | ERROR_LIBRARY_NOT_FOUND) && !nvidia_on_pci() {
            self.status = "absent";
            self.error = None;
            return;
        }
        let msg = self
            .api
            .as_ref()
            .map(|a| a.err(ret))
            .unwrap_or_else(|| format!("code {}", ret));
        let mut e = format!("{}: {}", what, msg);
        if ret == ERROR_LIB_RM_VERSION_MISMATCH {
            e.push_str(" (driver updated without reloading the kernel module; a reboot fixes it)");
        }
        self.status = "error";
        self.error = Some(e);
    }

    fn init(&mut self) {
        if self.api.is_none() {
            match Api::load() {
                Some(a) => self.api = Some(a),
                None => {
                    self.status = "absent";
                    self.retry_at = Some(Instant::now() + RETRY);
                    return;
                }
            }
        }
        let api = self.api.as_ref().unwrap();
        let ret = unsafe { (api.init)() };
        if ret != SUCCESS {
            self.fail(ret, "nvmlInit");
            return;
        }
        if let Some(f) = api.driver {
            let mut buf = [0 as c_char; 96];
            if unsafe { f(buf.as_mut_ptr(), 96) } == SUCCESS {
                self.driver = Some(
                    unsafe { CStr::from_ptr(buf.as_ptr()) }
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        if let Some(f) = api.cuda {
            let mut v: c_int = 0;
            if unsafe { f(&mut v) } == SUCCESS {
                self.cuda = Some(format!("{}.{}", v / 1000, v % 1000 / 10));
            }
        }
        self.status = "ok";
        self.error = None;
    }

    fn fan(&mut self, dev: Device, index: u32) -> Result<Option<u32>, Lost> {
        let now = Instant::now();
        if let Some(&(t, v)) = self.fans.get(&index) {
            if now - t < FAN_TTL {
                return Ok(v);
            }
        }
        let v = self
            .api
            .as_ref()
            .unwrap()
            .u32_of(self.api.as_ref().unwrap().fan, dev)?;
        self.fans.insert(index, (now, v));
        Ok(v)
    }

    fn mig_enabled(&mut self, dev: Device, index: u32) -> bool {
        let now = Instant::now();
        if let Some(&(t, on)) = self.mig_mode.get(&index) {
            if now - t < STATIC_TTL {
                return on;
            }
        }
        let api = self.api.as_ref().unwrap();
        let on = api.mig_mode.is_some_and(|f| {
            let (mut cur, mut pending) = (0, 0);
            unsafe { f(dev, &mut cur, &mut pending) == SUCCESS && cur == 1 }
        });
        self.mig_mode.insert(index, (now, on));
        on
    }

    fn mig_instances(&self, dev: Device) -> Vec<Value> {
        let api = self.api.as_ref().unwrap();
        let (Some(max), Some(get)) = (api.mig_max, api.mig_handle) else {
            return vec![];
        };
        let mut count: c_uint = 0;
        if unsafe { max(dev, &mut count) } != SUCCESS {
            return vec![];
        }
        let mut out = Vec::new();
        for i in 0..count {
            let mut h: Device = std::ptr::null_mut();
            if unsafe { get(dev, i, &mut h) } != SUCCESS {
                continue; // slot not populated
            }
            let (used, total) = api.memory(h);
            let compute = api.processes(&api.compute, h);
            out.push(json!({
                "gi": api.u32_of(api.gi, h).ok().flatten(),
                "ci": api.u32_of(api.ci, h).ok().flatten(),
                "mem_used": used, "mem_total": total,
                "procs": merge_procs(compute.as_deref(), None),
            }));
        }
        out
    }

    /// {pid: SM%} averaged since the last call; None if the driver can't tell.
    fn sm_util(&mut self, dev: Device, index: u32) -> Option<HashMap<u32, u32>> {
        let now = Instant::now();
        if let Some((t, _, cached)) = self.sm.get(&index) {
            if now - *t < SM_TTL {
                return Some(cached.clone());
            }
        }
        let f = self.api.as_ref().unwrap().proc_util?;
        let wall_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_micros() as u64;
        let since = self
            .sm
            .get(&index)
            .map(|e| e.1)
            .unwrap_or(wall_us.saturating_sub(1_000_000));
        let mut size: c_uint = 128;
        let mut buf = vec![ProcUtilSample::default(); size as usize];
        let mut ret = unsafe { f(dev, buf.as_mut_ptr(), &mut size, since) };
        if ret == ERROR_INSUFFICIENT_SIZE {
            buf = vec![ProcUtilSample::default(); size as usize];
            ret = unsafe { f(dev, buf.as_mut_ptr(), &mut size, since) };
        }
        let (sm, newest) = match ret {
            SUCCESS => {
                let samples: Vec<(u32, u64, u32)> = buf[..size as usize]
                    .iter()
                    .map(|s| (s.pid, s.ts, s.sm))
                    .collect();
                average_sm(&samples, since)
            }
            ERROR_NOT_FOUND => (HashMap::new(), since), // nobody computed since last time
            _ => return None,
        };
        self.sm.insert(index, (now, newest, sm.clone()));
        Some(sm)
    }

    fn gpu(&mut self, index: u32) -> Result<Value, Lost> {
        let api = self.api.as_ref().unwrap();
        let mut dev: Device = std::ptr::null_mut();
        let ret = unsafe { (api.handle)(index, &mut dev) };
        if ret != SUCCESS {
            return Err(Lost(if ret == ERROR_GPU_IS_LOST {
                "GPU is lost".into()
            } else {
                format!("NVML: {}", api.err(ret))
            }));
        }
        let name = match api.string_of(api.name, dev) {
            Ok(n) => n.unwrap_or_else(|| "?".into()),
            Err(ERROR_GPU_IS_LOST) => return Err(Lost("GPU is lost".into())),
            Err(ERROR_UNKNOWN) => return Err(Lost("Unknown Error".into())), // gpustat's InvalidGPU cases
            Err(_) => "?".into(),
        };
        let uuid = api.string_of(api.uuid, dev).ok().flatten();
        let temp = api.temp.and_then(|f| {
            let mut v = 0;
            (unsafe { f(dev, 0, &mut v) } == SUCCESS).then_some(v) // NVML_TEMPERATURE_GPU
        });
        let mut util = Utilization::default();
        let util_ok = api
            .util
            .is_some_and(|f| unsafe { f(dev, &mut util) } == SUCCESS);
        let power = api.u32_of(api.power, dev)?;
        let limit = api.u32_of(api.limit, dev)?;
        let codec = |f: Option<FnDevU32x2>| {
            f.and_then(|f| {
                let (mut u, mut period) = (0, 0);
                (unsafe { f(dev, &mut u, &mut period) } == SUCCESS).then_some(u)
            })
        };
        let (enc, dec) = (codec(api.enc), codec(api.dec));
        let (mut used, mut total) = api.memory(dev);

        let compute = api.processes(&api.compute, dev);
        let graphics = api.processes(&api.graphics, dev);
        let now = Instant::now();
        let mps = if self.mps_skip.get(&index).is_some_and(|&t| now < t) {
            None
        } else {
            api.processes(&api.mps, dev)
        };
        if mps.as_ref().is_none_or(|m| m.is_empty()) {
            self.mps_skip.insert(index, now + STATIC_TTL); // no MPS here: don't ask every poll
        }
        let supported = compute.is_some() || graphics.is_some();
        let all_compute: Option<Vec<(u32, Option<f64>)>> = match (compute, mps) {
            (None, None) => None,
            (c, m) => Some(
                c.unwrap_or_default()
                    .into_iter()
                    .chain(m.unwrap_or_default())
                    .collect(),
            ),
        };
        let mut procs = merge_procs(all_compute.as_deref(), graphics.as_deref());
        let fan = self.fan(dev, index)?;

        let mut mig = Value::Null;
        if self.mig_enabled(dev, index) {
            let instances = self.mig_instances(dev);
            let mut seen: std::collections::HashSet<u64> =
                procs.iter().filter_map(|p| p["pid"].as_u64()).collect();
            for inst in &instances {
                for p in inst["procs"].as_array().into_iter().flatten() {
                    if seen.insert(p["pid"].as_u64().unwrap_or(0)) {
                        procs.push(p.clone());
                    }
                }
            }
            if used.is_none()
                && !instances.is_empty()
                && instances.iter().all(|i| i["mem_used"].is_number())
            {
                used = Some(
                    instances
                        .iter()
                        .filter_map(|i| i["mem_used"].as_f64())
                        .sum(),
                );
                total = Some(
                    instances
                        .iter()
                        .filter_map(|i| i["mem_total"].as_f64())
                        .sum(),
                );
            }
            mig = Value::Array(instances);
        } else if procs.iter().filter(|p| p["type"] != "G").count() >= 2 {
            // several jobs share this GPU: which one computes and which only holds memory?
            if let Some(sm) = self.sm_util(dev, index) {
                for p in procs.iter_mut().filter(|p| p["type"] != "G") {
                    let pid = p["pid"].as_u64().unwrap_or(0) as u32;
                    p["sm"] = json!(sm.get(&pid).copied().unwrap_or(0));
                }
            }
        }

        Ok(json!({
            "index": index, "uuid": uuid, "name": name,
            "temp": temp, "fan": fan,
            "util": util_ok.then_some(util.gpu), "mem_util": util_ok.then_some(util.memory),
            "enc": enc, "dec": dec,
            "mem_used": used, "mem_total": total,
            "power": power.map(|p| p as f64 / 1000.0), "power_limit": limit.map(|p| p as f64 / 1000.0),
            "procs": procs, "procs_supported": supported, "mig": mig,
        }))
    }

    fn collect(&mut self) -> Result<Vec<Value>, String> {
        if self.status != "ok" && self.retry_at.is_none_or(|t| Instant::now() >= t) {
            self.init();
        }
        if self.status != "ok" {
            return Ok(vec![]);
        }
        let api = self.api.as_ref().unwrap();
        let mut count: c_uint = 0;
        let ret = unsafe { (api.count)(&mut count) };
        if ret != SUCCESS {
            if matches!(
                ret,
                ERROR_UNINITIALIZED | ERROR_DRIVER_NOT_LOADED | ERROR_LIB_RM_VERSION_MISMATCH
            ) {
                if let Some(f) = api.shutdown {
                    unsafe { f() }; // driver reloaded or upgraded under us: start over
                }
                self.fail(ret, "nvmlDeviceGetCount");
                return Ok(vec![]);
            }
            return Err(format!("nvmlDeviceGetCount: {}", api.err(ret)));
        }
        let mut gpus = Vec::new();
        for i in 0..count {
            match self.gpu(i) {
                Ok(g) => gpus.push(g),
                Err(Lost(e)) => gpus.push(
                    json!({"index": i, "name": format!("(({}))", e), "invalid": true, "procs": []}),
                ),
            }
        }
        Ok(gpus)
    }
}

/// What a poll gets back from NVML.
pub struct Reading {
    pub gpus: Vec<Value>,
    pub error: Option<String>,
    pub status: &'static str,
    pub driver: Option<String>,
    pub cuda: Option<String>,
}

/// Runs NVML in a worker thread with a timeout, so a hung driver can't freeze the agent.
/// While a call is stuck no new one is made; CPU and RAM keep being reported.
pub struct Worker {
    nvml: Arc<Mutex<Nvml>>,
    stuck: Option<thread::JoinHandle<()>>,
    last: (&'static str, Option<String>, Option<String>),
}

impl Worker {
    pub fn new() -> Self {
        Worker {
            nvml: Arc::new(Mutex::new(Nvml::new())),
            stuck: None,
            last: ("init", None, None),
        }
    }

    pub fn query(&mut self) -> Reading {
        let stale = |w: &Worker, e: &str| Reading {
            gpus: vec![],
            error: Some(e.to_string()),
            status: w.last.0,
            driver: w.last.1.clone(),
            cuda: w.last.2.clone(),
        };
        if let Some(h) = &self.stuck {
            if !h.is_finished() {
                return stale(self, "NVML not responding (driver stuck)");
            }
            self.stuck = None;
        }
        let (tx, rx) = mpsc::channel();
        let nvml = Arc::clone(&self.nvml);
        let handle = thread::spawn(move || {
            let mut n = match nvml.lock() {
                Ok(n) => n,
                Err(p) => p.into_inner(),
            };
            let r = n.collect();
            let _ = tx.send(match r {
                Ok(gpus) => Reading {
                    gpus,
                    error: n.error.clone(),
                    status: n.status,
                    driver: n.driver.clone(),
                    cuda: n.cuda.clone(),
                },
                Err(e) => Reading {
                    gpus: vec![],
                    error: Some(e),
                    status: n.status,
                    driver: n.driver.clone(),
                    cuda: n.cuda.clone(),
                },
            });
        });
        match rx.recv_timeout(TIMEOUT) {
            Ok(r) => {
                let _ = handle.join();
                self.last = (r.status, r.driver.clone(), r.cuda.clone());
                r
            }
            Err(_) => {
                self.stuck = Some(handle);
                stale(
                    self,
                    &format!("NVML timed out after {}s", TIMEOUT.as_secs()),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge() {
        let c = [(10, Some(100.0)), (11, Some(50.0)), (11, Some(50.0))];
        let g = [(11, Some(1.0)), (12, Some(4.0))];
        let out = merge_procs(Some(&c), Some(&g));
        let got: Vec<(u64, &str)> = out
            .iter()
            .map(|p| (p["pid"].as_u64().unwrap(), p["type"].as_str().unwrap()))
            .collect();
        assert_eq!(got, vec![(10, "C"), (11, "C+G"), (12, "G")]);
        assert!(merge_procs(None, None).is_empty());
    }

    #[test]
    fn sm_average() {
        let samples = [
            (1, 100, 80),
            (1, 200, 60),
            (2, 150, 0),
            (2, 50, 90),
            (3, 300, u32::MAX),
        ];
        let (sm, newest) = average_sm(&samples, 60);
        assert_eq!(sm.len(), 2);
        assert_eq!((sm[&1], sm[&2], newest), (70, 0, 200));
    }

    #[test]
    fn struct_layouts_match_nvml() {
        assert_eq!(std::mem::size_of::<MemoryV2>(), 40);
        assert_eq!(MEMORY_V2 & 0xffff, 40);
        assert_eq!(std::mem::size_of::<ProcUtilSample>(), 32);
    }

    #[test]
    fn works_without_a_gpu() {
        let mut w = Worker::new();
        let r = w.query();
        assert!(matches!(r.status, "ok" | "absent" | "error"));
    }
}
