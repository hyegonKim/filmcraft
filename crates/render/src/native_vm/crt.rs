//! Cross-OS C/C++ runtime (`VCRUNTIME140`, `ucrtbase`, `KERNEL32`, `libSystem`, `libm`,
//! Photoshop `PLUGIN.dll`, Illustrator `SPBasic.dll`) and extensible host thunk dispatcher.

use std::collections::HashMap;

use super::aarch64::Arm64Cpu;
use super::mem::PagedMemory;
use super::x86_64::X64Cpu;
use super::{GuestArch, GuestOs, ImportSymbol, Limits, PluginHostError, Result};

/// Calling context passed to every host thunk (`CRT` or Adobe SDK suite callback).
pub struct ThunkCallCtx<'a> {
    pub arch: GuestArch,
    pub os: GuestOs,
    pub args: [u64; 8],
    pub fargs: [f64; 4],
    pub mem: &'a mut PagedMemory,
}

impl ThunkCallCtx<'_> {
    pub fn arg(&self, idx: usize) -> u64 {
        self.args.get(idx).copied().unwrap_or(0)
    }

    pub fn farg_f64(&self, idx: usize) -> f64 {
        self.fargs.get(idx).copied().unwrap_or(0.0)
    }

    pub fn farg_f32(&self, idx: usize) -> f32 {
        let bits = self.fargs.get(idx).copied().unwrap_or(0.0).to_bits() as u32;
        f32::from_bits(bits)
    }
}

/// Return value of a host thunk: integer (`RAX`/`X0`), `f64` (`XMM0`/`D0`), `f32` (`XMM0`/`S0`),
/// or a request for the VM to invoke a guest callback before finishing!
#[derive(Clone, Debug)]
pub enum ThunkOutcome {
    Int(u64),
    Float64(f64),
    Float32(f32),
    InitTerm { start: u64, end: u64, return_status: bool },
    PiThreadsDoAll { worker_fn: u64, handle: u64 },
}

/// Trait implemented by format-specific Adobe Host SDK runtimes (`.8bf`, `.prm`/`.aex`, `.aip`).
pub trait HostCallbackHandler {
    fn invoke_host_thunk(&mut self, thunk_id: u32, ctx: &mut ThunkCallCtx<'_>, vm: &mut GuestVm) -> Result<ThunkOutcome>;
}

pub struct NoopHostHandler;
impl HostCallbackHandler for NoopHostHandler {
    fn invoke_host_thunk(&mut self, thunk_id: u32, _ctx: &mut ThunkCallCtx<'_>, _vm: &mut GuestVm) -> Result<ThunkOutcome> {
        Err(PluginHostError::Abi(format!("unexpected host thunk {thunk_id}")))
    }
}

#[derive(Clone, Debug)]
struct PiThreadPool {
    num_threads: u16,
    params_ptr: u64,
    param_size: usize,
}

#[derive(Clone, Debug)]
enum ThunkBinding {
    Crt(String),
    HostCallback(u32),
}

/// Sandboxed dual-ISA (`x86_64` + `AArch64`) virtual machine with CRT + host callback dispatch.
pub struct GuestVm {
    pub arch: GuestArch,
    pub os: GuestOs,
    pub mem: PagedMemory,
    pub limits: Limits,
    pub fuel_remaining: u64,
    call_depth: usize,
    thunks: Vec<ThunkBinding>,
    crt_by_name: HashMap<String, u32>,
    tls_values: HashMap<u32, u64>,
    next_tls_idx: u32,
    pi_threads: HashMap<u64, PiThreadPool>,
    next_pi_thread_id: u64,
}

impl GuestVm {
    pub fn new(arch: GuestArch, os: GuestOs, mem: PagedMemory, limits: Limits) -> Self {
        Self {
            arch,
            os,
            mem,
            fuel_remaining: limits.instruction_budget,
            limits,
            call_depth: 0,
            thunks: Vec::new(),
            crt_by_name: HashMap::new(),
            tls_values: HashMap::new(),
            next_tls_idx: 1,
            pi_threads: HashMap::new(),
            next_pi_thread_id: 0x5000_0001,
        }
    }

    pub fn reset_fuel(&mut self, budget: u64) {
        self.fuel_remaining = budget;
    }

    /// Registers a host callback thunk and returns its guest callable function pointer address.
    pub fn register_host_callback(&mut self, callback_id: u32) -> u64 {
        let idx = self.thunks.len() as u32;
        self.thunks.push(ThunkBinding::HostCallback(callback_id));
        PagedMemory::thunk_addr(idx)
    }

    /// Registers or reuses a named CRT/OS import thunk and returns its guest function address.
    pub fn register_crt_import(&mut self, sym_name: &str) -> u64 {
        let normalized = normalize_symbol(sym_name);
        if let Some(&idx) = self.crt_by_name.get(&normalized) {
            return PagedMemory::thunk_addr(idx);
        }
        let idx = self.thunks.len() as u32;
        self.thunks.push(ThunkBinding::Crt(normalized.clone()));
        self.crt_by_name.insert(normalized, idx);
        PagedMemory::thunk_addr(idx)
    }

    /// Binds all PE IAT or Mach-O GOT/stub entries in `imports` to sandboxed thunk addresses.
    pub fn bind_imports(&mut self, imports: &[ImportSymbol]) -> Result<()> {
        for imp in imports {
            // Data globals (`___stack_chk_guard`, `_fltused`, etc.) need valid readable data pointers.
            if matches!(imp.name.as_str(), "__stack_chk_guard" | "___stack_chk_guard" | "_fltused") {
                let guard = self.mem.heap_alloc(16, true)?;
                self.mem.write_u64(guard, 0x0000_0ACE_CAFE_BEEF)?;
                let slot = imp.slot_addr & !(1u64 << 62);
                self.mem.write_u64(slot, guard)?;
                continue;
            }
            let thunk_ptr = self.register_crt_import(&imp.name);
            if (imp.slot_addr & (1u64 << 62)) != 0 {
                let stub_addr = imp.slot_addr & !(1u64 << 62);
                self.write_direct_stub_branch(stub_addr, thunk_ptr)?;
            } else {
                self.mem.write_u64(imp.slot_addr, thunk_ptr)?;
            }
        }
        Ok(())
    }

    fn write_direct_stub_branch(&mut self, stub_addr: u64, thunk_ptr: u64) -> Result<()> {
        match self.arch {
            GuestArch::AArch64 => {
                // `LDR X16, #8; BR X16; .quad thunk_ptr` (16 bytes if stride allows, else MOVZ/MOVK + BR)
                // For compact 12-byte Mach-O arm64 stubs (`THUNK_BASE = 0x7ff_0000_0000`):
                // MOVZ X16, #(thunk & 0xFFFF)
                // MOVK X16, #((thunk >> 32) & 0xFFFF), LSL #32
                // BR X16
                let lo16 = (thunk_ptr & 0xFFFF) as u32;
                let hi16 = ((thunk_ptr >> 32) & 0xFFFF) as u32;
                let movz = 0xD280_0010 | (lo16 << 5);
                let movk = 0xF2C0_0010 | (hi16 << 5);
                let br_x16 = 0xD61F_0200u32;
                self.mem.write_u32(stub_addr, movz)?;
                self.mem.write_u32(stub_addr + 4, movk)?;
                self.mem.write_u32(stub_addr + 8, br_x16)?;
            }
            GuestArch::X86_64 => {
                // `MOV RAX, imm64 (48 B8 ...); JMP RAX (FF E0)` = 12 bytes
                let mut code = [0u8; 12];
                code[0] = 0x48;
                code[1] = 0xB8;
                code[2..10].copy_from_slice(&thunk_ptr.to_le_bytes());
                code[10] = 0xFF;
                code[11] = 0xE0;
                self.mem.write_bytes(stub_addr, &code)?;
            }
        }
        Ok(())
    }

    /// Calls `DllMain(image_base, DLL_PROCESS_ATTACH = 1, 0)` if the PE image has an entrypoint.
    pub fn run_dll_attach(&mut self, image_base: u64, dll_entry: Option<u64>) -> Result<()> {
        let Some(entry) = dll_entry else { return Ok(()) };
        let mut noop = NoopHostHandler;
        let _ = self.call_guest(entry, &[image_base, 1, 0], &mut noop)?;
        Ok(())
    }

    /// Invokes a guest function `fn_addr` with up to 6 integer/pointer arguments and returns `RAX`/`X0`.
    pub fn call_guest(&mut self, fn_addr: u64, args: &[u64], handler: &mut dyn HostCallbackHandler) -> Result<u64> {
        if self.call_depth >= self.limits.max_call_depth {
            return Err(PluginHostError::Limit("guest call recursion depth exceeded".into()));
        }
        let depth = self.call_depth;
        // Give each nested call its own 32 KiB stack frame slice so re-entrant callbacks never clobber caller stack!
        let sp_base = super::mem::STACK_TOP - (depth as u64) * 0x8000 - 0x100;
        self.call_depth += 1;
        let res = match self.arch {
            GuestArch::X86_64 => {
                let mut cpu = X64Cpu::new_call(self.os, fn_addr, sp_base, args, &mut self.mem)?;
                cpu.run(self, handler)
            }
            GuestArch::AArch64 => {
                let mut cpu = Arm64Cpu::new_call(fn_addr, sp_base, args, &mut self.mem)?;
                cpu.run(self, handler)
            }
        };
        self.call_depth -= 1;
        res
    }

    pub(crate) fn dispatch_thunk(&mut self, thunk_idx: u32, args: [u64; 8], fargs: [f64; 4], handler: &mut dyn HostCallbackHandler) -> Result<ThunkOutcome> {
        let binding = self.thunks.get(thunk_idx as usize).cloned().ok_or_else(|| PluginHostError::CpuTrap(format!("unregistered thunk index {thunk_idx}")))?;
        let outcome = match binding {
            ThunkBinding::HostCallback(cb_id) => self.invoke_host_thunk_bridge(cb_id, args, fargs, handler)?,
            ThunkBinding::Crt(name) => self.exec_crt_shim(&name, args, fargs)?,
        };
        match outcome {
            ThunkOutcome::InitTerm { start, end, return_status } => {
                let mut cur = start;
                while cur < end && cur.saturating_sub(start) < 8192 {
                    let fp = self.mem.read_u64(cur)?;
                    if fp != 0 {
                        let r = self.call_guest(fp, &[], handler)?;
                        if return_status && (r as i32) != 0 {
                            return Ok(ThunkOutcome::Int(r));
                        }
                    }
                    cur += 8;
                }
                Ok(ThunkOutcome::Int(0))
            }
            ThunkOutcome::PiThreadsDoAll { worker_fn, handle } => {
                if let Some(pool) = self.pi_threads.get(&handle).cloned()
                    && worker_fn != 0
                {
                    for idx in 0..pool.num_threads {
                        let p = pool.params_ptr + u64::from(idx) * (pool.param_size as u64);
                        let _ = self.call_guest(worker_fn, &[p], handler)?;
                    }
                }
                Ok(ThunkOutcome::Int(1))
            }
            other => Ok(other),
        }
    }

    fn invoke_host_thunk_bridge(&mut self, cb_id: u32, args: [u64; 8], fargs: [f64; 4], handler: &mut dyn HostCallbackHandler) -> Result<ThunkOutcome> {
        let mut scratch_mem = PagedMemory::new(64);
        std::mem::swap(&mut self.mem, &mut scratch_mem);
        let mut ctx = ThunkCallCtx { arch: self.arch, os: self.os, args, fargs, mem: &mut scratch_mem };
        let res = handler.invoke_host_thunk(cb_id, &mut ctx, self);
        std::mem::swap(&mut self.mem, &mut scratch_mem);
        res
    }

    fn exec_crt_shim(&mut self, name: &str, args: [u64; 8], fargs: [f64; 4]) -> Result<ThunkOutcome> {
        if let Some(m) = exec_math_shim(name, fargs) {
            return Ok(m);
        }
        match name {
            // Standard C/C++ heap & memory
            "malloc" | "??2@YAPEAX_K@Z" | "_Znwm" | "_Znam" | "SPAllocate" => Ok(ThunkOutcome::Int(self.mem.heap_alloc(args[0] as usize, false)?)),
            "calloc" => {
                let total = (args[0] as usize).saturating_mul(args[1] as usize);
                Ok(ThunkOutcome::Int(self.mem.heap_alloc(total, true)?))
            }
            "realloc" => Ok(ThunkOutcome::Int(self.mem.heap_realloc(args[0], args[1] as usize)?)),
            "free" | "??3@YAXPEAX@Z" | "??3@YAXPEAX_K@Z" | "_ZdlPv" | "_ZdaPv" | "PrDisposePtr" => {
                self.mem.heap_free(args[0]);
                Ok(ThunkOutcome::Int(0))
            }
            "memcpy" | "memmove" | "__memcpy_chk" | "__memmove_chk" => {
                let len = (args[2] as usize).min(64 << 20);
                if len > 0 && args[0] != args[1] {
                    let data = self.mem.read_vec(args[1], len)?;
                    self.mem.write_bytes(args[0], &data)?;
                }
                Ok(ThunkOutcome::Int(args[0]))
            }
            "memset" | "__memset_chk" => {
                let len = (args[2] as usize).min(64 << 20);
                self.mem.fill_byte(args[0], (args[1] & 0xFF) as u8, len)?;
                Ok(ThunkOutcome::Int(args[0]))
            }
            "memcmp" => {
                let len = (args[2] as usize).min(1 << 20);
                let a = self.mem.read_vec(args[0], len)?;
                let b = self.mem.read_vec(args[1], len)?;
                let ord = a.cmp(&b) as i32;
                Ok(ThunkOutcome::Int(ord as i64 as u64))
            }
            "strlen" => {
                let s = self.mem.read_cstring(args[0], 65536)?;
                Ok(ThunkOutcome::Int(s.len() as u64))
            }
            "strcmp" => {
                let a = self.mem.read_cstring(args[0], 4096)?;
                let b = self.mem.read_cstring(args[1], 4096)?;
                Ok(ThunkOutcome::Int((a.cmp(&b) as i32) as i64 as u64))
            }
            "strncmp" => {
                let n = (args[2] as usize).min(4096);
                let a = self.mem.read_cstring(args[0], n)?;
                let b = self.mem.read_cstring(args[1], n)?;
                let al = &a.as_bytes()[..a.len().min(n)];
                let bl = &b.as_bytes()[..b.len().min(n)];
                Ok(ThunkOutcome::Int((al.cmp(bl) as i32) as i64 as u64))
            }
            "strcpy" => {
                let s = self.mem.read_cstring(args[1], 4096)?;
                self.mem.write_bytes(args[0], s.as_bytes())?;
                self.mem.write_u8(args[0] + s.len() as u64, 0)?;
                Ok(ThunkOutcome::Int(args[0]))
            }
            "strncpy" => {
                let n = (args[2] as usize).min(65536);
                let s = self.mem.read_cstring(args[1], n)?;
                let copy_len = s.len().min(n);
                self.mem.write_bytes(args[0], &s.as_bytes()[..copy_len])?;
                if n > copy_len {
                    self.mem.zero_fill(args[0] + copy_len as u64, n - copy_len)?;
                }
                Ok(ThunkOutcome::Int(args[0]))
            }
            // Photoshop PLUGIN.dll exports
            "NewPtr" | "NewPtrClear" => {
                let zeroed = name == "NewPtrClear";
                Ok(ThunkOutcome::Int(self.mem.heap_alloc(args[0] as usize, zeroed)?))
            }
            "DisposePtr" => {
                self.mem.heap_free(args[0]);
                Ok(ThunkOutcome::Int(0))
            }
            "GetPtrSize" => Ok(ThunkOutcome::Int(self.mem.ptr_size(args[0]) as u64)),
            "NewHandle" | "NewHandleClear" => {
                let zeroed = name == "NewHandleClear";
                Ok(ThunkOutcome::Int(self.mem.new_handle(args[0] as usize, zeroed)?))
            }
            "DisposeHandle" => {
                self.mem.dispose_handle(args[0]);
                Ok(ThunkOutcome::Int(0))
            }
            "GetHandleSize" => Ok(ThunkOutcome::Int(self.mem.handle_size(args[0]) as u64)),
            "SetHandleSize" => {
                self.mem.resize_handle(args[0], args[1] as usize)?;
                Ok(ThunkOutcome::Int(0))
            }
            "HLock" => Ok(ThunkOutcome::Int(self.mem.lock_handle(args[0])?)),
            "HUnlock" => {
                self.mem.unlock_handle(args[0]);
                Ok(ThunkOutcome::Int(0))
            }
            "PIThreadsInitWithThreadCount" => {
                let param_size = (args[0] as usize).clamp(8, 4096);
                let num_threads = 1u16;
                let params_ptr = self.mem.heap_alloc(param_size * usize::from(num_threads), true)?;
                let handle = self.next_pi_thread_id;
                self.next_pi_thread_id += 1;
                self.pi_threads.insert(handle, PiThreadPool { num_threads, params_ptr, param_size });
                Ok(ThunkOutcome::Int(handle))
            }
            "PIThreadsGetNumThreads" => {
                let n = self.pi_threads.get(&args[0]).map_or(1, |p| p.num_threads);
                Ok(ThunkOutcome::Int(u64::from(n)))
            }
            "PIThreadsGetParamsPtr" => {
                if let Some(pool) = self.pi_threads.get(&args[0]) {
                    let idx = (args[1] as usize).min(usize::from(pool.num_threads.saturating_sub(1)));
                    Ok(ThunkOutcome::Int(pool.params_ptr + (idx * pool.param_size) as u64))
                } else {
                    Ok(ThunkOutcome::Int(0))
                }
            }
            "PIThreadsDoAll" => {
                // `PIThreadsDoAll(int32 flags, PIThreadsHandle handle, PIThreadsProcPtr worker_fn)`
                let worker_fn = if args[2] != 0 { args[2] } else { args[0] };
                Ok(ThunkOutcome::PiThreadsDoAll { worker_fn, handle: args[1] })
            }
            "PIThreadsEndThreads" => {
                self.pi_threads.remove(&args[0]);
                Ok(ThunkOutcome::Int(1))
            }
            // Illustrator SPBasic.dll exports
            "?SPBasicAllocateBlock@ai@@YAH_KPEAPEAX@Z" => {
                let ptr = self.mem.heap_alloc(args[0] as usize, true)?;
                if args[1] != 0 {
                    self.mem.write_u64(args[1], ptr)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            "?SPBasicFreeBlock@ai@@YAHPEAX@Z" => {
                self.mem.heap_free(args[0]);
                Ok(ThunkOutcome::Int(0))
            }
            "?SPBasicReallocateBlock@ai@@YAHPEAX_KPEAPEAX@Z" => {
                let ptr = self.mem.heap_realloc(args[0], args[1] as usize)?;
                if args[2] != 0 {
                    self.mem.write_u64(args[2], ptr)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            // Win32 / UCRT initialization & threading shims
            "_initterm" => Ok(ThunkOutcome::InitTerm { start: args[0], end: args[1], return_status: false }),
            "_initterm_e" => Ok(ThunkOutcome::InitTerm { start: args[0], end: args[1], return_status: true }),
            "InitializeSListHead" => {
                if args[0] != 0 {
                    self.mem.zero_fill(args[0], 16)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            "DisableThreadLibraryCalls" => Ok(ThunkOutcome::Int(1)),
            "IsProcessorFeaturePresent" => {
                // Report SSE/SSE2/SSE3/COMPARE_EXCHANGE128/FASTFAIL present; AVX-512 absent
                let feat = args[0] as u32;
                let ok = matches!(feat, 6 | 10 | 13 | 14 | 17 | 23);
                Ok(ThunkOutcome::Int(u64::from(ok)))
            }
            "IsDebuggerPresent" => Ok(ThunkOutcome::Int(0)),
            "GetCurrentProcessId" => Ok(ThunkOutcome::Int(1000)),
            "GetCurrentThreadId" => Ok(ThunkOutcome::Int(1001)),
            "GetSystemTimeAsFileTime" => {
                if args[0] != 0 {
                    self.mem.write_u64(args[0], 133_500_000_000_000_000)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            "QueryPerformanceCounter" => {
                if args[0] != 0 {
                    self.mem.write_u64(args[0], 1_000_000)?;
                }
                Ok(ThunkOutcome::Int(1))
            }
            "QueryPerformanceFrequency" => {
                if args[0] != 0 {
                    self.mem.write_u64(args[0], 10_000_000)?;
                }
                Ok(ThunkOutcome::Int(1))
            }
            "TlsAlloc" | "FlsAlloc" => {
                let idx = self.next_tls_idx;
                self.next_tls_idx += 1;
                self.tls_values.insert(idx, 0);
                Ok(ThunkOutcome::Int(u64::from(idx)))
            }
            "TlsGetValue" | "FlsGetValue" => {
                let val = self.tls_values.get(&(args[0] as u32)).copied().unwrap_or(0);
                Ok(ThunkOutcome::Int(val))
            }
            "TlsSetValue" | "FlsSetValue" => {
                self.tls_values.insert(args[0] as u32, args[1]);
                Ok(ThunkOutcome::Int(1))
            }
            "TlsFree" | "FlsFree" => {
                self.tls_values.remove(&(args[0] as u32));
                Ok(ThunkOutcome::Int(1))
            }
            // Safe benign runtime/atexit/lock stubs
            "_initialize_onexit_table"
            | "_register_onexit_function"
            | "_execute_onexit_table"
            | "_cexit"
            | "_configure_narrow_argv"
            | "_initialize_narrow_environment"
            | "_crt_atexit"
            | "_crt_at_quick_exit"
            | "__cxa_atexit"
            | "_Mtx_lock"
            | "_Mtx_unlock"
            | "AcquireSRWLockExclusive"
            | "ReleaseSRWLockExclusive"
            | "EnterCriticalSection"
            | "LeaveCriticalSection"
            | "InitializeCriticalSection"
            | "InitializeCriticalSectionAndSpinCount"
            | "DeleteCriticalSection"
            | "SetUnhandledExceptionFilter"
            | "GetLastError"
            | "SetLastError" => Ok(ThunkOutcome::Int(0)),
            _ => Ok(ThunkOutcome::Int(0)),
        }
    }
}

fn normalize_symbol(sym: &str) -> String {
    if sym.starts_with("??")
        || sym.starts_with('?')
        || sym.starts_with("_init")
        || sym.starts_with("_cexit")
        || sym.starts_with("_Z")
        || sym.starts_with("_Mtx")
    {
        return sym.to_string();
    }
    sym.strip_prefix('_').unwrap_or(sym).to_string()
}

fn f32_arg(v: f64) -> f32 {
    f32::from_bits(v.to_bits() as u32)
}

fn exec_math_shim(name: &str, fargs: [f64; 4]) -> Option<ThunkOutcome> {
    let a = fargs[0];
    let b = fargs[1];
    let af = f32_arg(a);
    let bf = f32_arg(b);
    let out = match name {
        "sin" => ThunkOutcome::Float64(a.sin()),
        "cos" => ThunkOutcome::Float64(a.cos()),
        "tan" => ThunkOutcome::Float64(a.tan()),
        "asin" => ThunkOutcome::Float64(a.asin()),
        "acos" => ThunkOutcome::Float64(a.acos()),
        "atan" => ThunkOutcome::Float64(a.atan()),
        "atan2" => ThunkOutcome::Float64(a.atan2(b)),
        "sinh" => ThunkOutcome::Float64(a.sinh()),
        "cosh" => ThunkOutcome::Float64(a.cosh()),
        "tanh" => ThunkOutcome::Float64(a.tanh()),
        "exp" => ThunkOutcome::Float64(a.exp()),
        "exp2" => ThunkOutcome::Float64(a.exp2()),
        "log" => ThunkOutcome::Float64(a.ln()),
        "log2" => ThunkOutcome::Float64(a.log2()),
        "log10" => ThunkOutcome::Float64(a.log10()),
        "pow" => ThunkOutcome::Float64(a.powf(b)),
        "sqrt" => ThunkOutcome::Float64(a.sqrt()),
        "cbrt" => ThunkOutcome::Float64(a.cbrt()),
        "hypot" => ThunkOutcome::Float64(a.hypot(b)),
        "fabs" => ThunkOutcome::Float64(a.abs()),
        "floor" => ThunkOutcome::Float64(a.floor()),
        "ceil" => ThunkOutcome::Float64(a.ceil()),
        "round" => ThunkOutcome::Float64(a.round()),
        "trunc" => ThunkOutcome::Float64(a.trunc()),
        "fmod" => ThunkOutcome::Float64(a % b),
        "fmin" => ThunkOutcome::Float64(a.min(b)),
        "fmax" => ThunkOutcome::Float64(a.max(b)),
        "sinf" => ThunkOutcome::Float32(af.sin()),
        "cosf" => ThunkOutcome::Float32(af.cos()),
        "tanf" => ThunkOutcome::Float32(af.tan()),
        "asinf" => ThunkOutcome::Float32(af.asin()),
        "acosf" => ThunkOutcome::Float32(af.acos()),
        "atanf" => ThunkOutcome::Float32(af.atan()),
        "atan2f" => ThunkOutcome::Float32(af.atan2(bf)),
        "expf" => ThunkOutcome::Float32(af.exp()),
        "logf" => ThunkOutcome::Float32(af.ln()),
        "log10f" => ThunkOutcome::Float32(af.log10()),
        "powf" => ThunkOutcome::Float32(af.powf(bf)),
        "sqrtf" => ThunkOutcome::Float32(af.sqrt()),
        "fabsf" => ThunkOutcome::Float32(af.abs()),
        "floorf" => ThunkOutcome::Float32(af.floor()),
        "ceilf" => ThunkOutcome::Float32(af.ceil()),
        "roundf" => ThunkOutcome::Float32(af.round()),
        "fmodf" => ThunkOutcome::Float32(af % bf),
        _ => return None,
    };
    Some(out)
}
