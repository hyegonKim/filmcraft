//! Adobe After Effects `.aex` & Premiere Pro `.prm` C++ Effect Host SDK runtime.
//!
//! Supports:
//! 1. **AE / Premiere Unified Effect ABI** (`EffectMain` / `EntryPointFunc` / `MainEntry` with
//!    `PF_Cmd_GLOBAL_SETUP`, `PF_Cmd_PARAMS_SETUP` live `add_param` parameter discovery,
//!    `PF_Cmd_RENDER` over 8-bit `PF_Pixel` ARGB / 32-bit `PF_PixelFloat` `PF_EffectWorld`,
//!    `SPBasicSuite` `pica_basicP` @ `+0x180`, `PF_Iterate8Suite1`, `PF_WorldTransformSuite1`,
//!    `PF_HandleSuite1`, and `PF_ANSICallbacksSuite1`).
//! 2. **Premiere Classic Video Filter ABI** (`xFilter` with `fsInitSpec` / `fsExecute` `VideoRecord`).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::crt::{GuestVm, HostCallbackHandler, ThunkCallCtx, ThunkOutcome};
use super::mem::PagedMemory;
use super::{Limits, PluginDescriptor, PluginFormat, PluginHostError, Result, load_any_image, pipl, resolve_plugin_entrypoint};

const PF_CMD_GLOBAL_SETUP: u64 = 1;
const PF_CMD_PARAMS_SETUP: u64 = 2;
const PF_CMD_SEQUENCE_SETUP: u64 = 5;
const PF_CMD_FRAME_SETUP: u64 = 10;
const PF_CMD_RENDER: u64 = 11;

const PF_PARAM_FIX_SLIDER: i32 = 2;
const PF_PARAM_ANGLE: i32 = 3;
const PF_PARAM_CHECKBOX: i32 = 4;
const PF_PARAM_COLOR: i32 = 5;
const PF_PARAM_POINT: i32 = 6;
const PF_PARAM_POPUP: i32 = 7;
const PF_PARAM_FLOAT_SLIDER: i32 = 8;

const PARAM_DEF_SIZE: usize = 0xB0; // 176 bytes — verified against ThresholdRGB.aex

const CB_ADD_PARAM: u32 = 100;
const CB_CHECKOUT_PARAM: u32 = 101;
const CB_CHECKIN_PARAM: u32 = 102;
const CB_ABORT: u32 = 103;
const CB_PROGRESS: u32 = 104;
const CB_PICA_ACQUIRE: u32 = 110;
const CB_PICA_RELEASE: u32 = 111;
const CB_PICA_ISEQUAL: u32 = 112;
const CB_PICA_ALLOC: u32 = 113;
const CB_PICA_FREE: u32 = 114;
const CB_PICA_REALLOC: u32 = 115;
const CB_ITERATE8: u32 = 120;
const CB_ITERATE_FLOAT: u32 = 121;
const CB_WORLD_COPY: u32 = 130;
const CB_WORLD_FILL: u32 = 131;
const CB_HDL_NEW: u32 = 140;
const CB_HDL_DISPOSE: u32 = 141;
const CB_HDL_LOCK: u32 = 142;
const CB_HDL_UNLOCK: u32 = 143;
const CB_HDL_SIZE: u32 = 144;

/// Discovered UI parameter kind from a compiled `.aex` or `.prm` C++ binary's `PF_Cmd_PARAMS_SETUP`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AeParamKind {
    Slider { default: f64, min: f64, max: f64 },
    Angle { default: f64 },
    Checkbox { default: bool },
    Color { default: [f64; 4] },
    Point { default: [f64; 2] },
    Popup { default: u32, count: u32 },
}

/// One parameter registered by `in_data->inter.add_param` during `PF_Cmd_PARAMS_SETUP`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AeParamSpec {
    pub id: i32,
    pub name: String,
    pub raw_type: i32,
    pub kind: AeParamKind,
}

/// Host RGBA frame (`[R, G, B, A]` straight or premultiplied `f32` per pixel).
#[derive(Clone, Debug)]
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<[f32; 4]>,
}

impl RgbaFrame {
    pub fn new(width: u32, height: u32, pixels: Vec<[f32; 4]>) -> Result<Self> {
        let expected = (width as usize).checked_mul(height as usize).ok_or_else(|| PluginHostError::InvalidBinary("frame dimensions overflow".into()))?;
        if pixels.len() != expected {
            return Err(PluginHostError::Abi(format!("expected {expected} pixels, got {}", pixels.len())));
        }
        Ok(Self { width, height, pixels })
    }
}

struct AeHostState {
    discovered_params: Vec<AeParamSpec>,
    suite_cache: HashMap<String, u64>,
}

impl AeHostState {
    fn new() -> Self {
        Self { discovered_params: Vec::new(), suite_cache: HashMap::new() }
    }

    fn parse_added_param(&mut self, mem: &PagedMemory, def_ptr: u64) -> Result<()> {
        if def_ptr == 0 || self.discovered_params.len() >= 128 {
            return Ok(());
        }
        let uu_id = mem.read_i32(def_ptr)?;
        let param_type = mem.read_i32(def_ptr + 0x0C)?;
        let raw_name = mem.read_cstring(def_ptr + 0x10, 32)?;
        let name = if raw_name.is_empty() { format!("Param {}", self.discovered_params.len() + 1) } else { raw_name };
        let u_off = def_ptr + 0x38;
        let kind = match param_type {
            PF_PARAM_FIX_SLIDER => {
                let val = f64::from(mem.read_i32(u_off)?) / 65536.0;
                let vmin = f64::from(mem.read_i32(u_off + 0x44)?) / 65536.0;
                let vmax = f64::from(mem.read_i32(u_off + 0x48)?) / 65536.0;
                let dephault = f64::from(mem.read_i32(u_off + 0x54)?) / 65536.0;
                let def = if dephault != 0.0 { dephault } else { val };
                let max = if vmax > vmin { vmax } else { 100.0 };
                AeParamKind::Slider { default: def, min: vmin, max }
            }
            PF_PARAM_FLOAT_SLIDER => {
                let val = mem.read_f64(u_off)?;
                let vmin = mem.read_f32(u_off + 0x48)? as f64;
                let vmax = mem.read_f32(u_off + 0x50)? as f64;
                let dephault = mem.read_f64(u_off + 0x58)?;
                let def = if dephault.is_finite() && dephault != 0.0 {
                    dephault
                } else if val.is_finite() {
                    val
                } else {
                    0.0
                };
                let max = if vmax.is_finite() && vmax > vmin { vmax } else { 100.0 };
                AeParamKind::Slider { default: def, min: if vmin.is_finite() { vmin } else { 0.0 }, max }
            }
            PF_PARAM_ANGLE => {
                let val = f64::from(mem.read_i32(u_off)?) / 65536.0;
                AeParamKind::Angle { default: val }
            }
            PF_PARAM_CHECKBOX => {
                let val = mem.read_i32(u_off)? != 0 || mem.read_u8(u_off + 4)? != 0;
                AeParamKind::Checkbox { default: val }
            }
            PF_PARAM_COLOR => {
                let a = f64::from(mem.read_u8(u_off)?) / 255.0;
                let r = f64::from(mem.read_u8(u_off + 1)?) / 255.0;
                let g = f64::from(mem.read_u8(u_off + 2)?) / 255.0;
                let b = f64::from(mem.read_u8(u_off + 3)?) / 255.0;
                AeParamKind::Color { default: [r, g, b, if a > 0.0 { a } else { 1.0 }] }
            }
            PF_PARAM_POINT => {
                let x = f64::from(mem.read_i32(u_off)?) / 65536.0;
                let y = f64::from(mem.read_i32(u_off + 4)?) / 65536.0;
                AeParamKind::Point { default: [x, y] }
            }
            PF_PARAM_POPUP => {
                let v = mem.read_i32(u_off)?.max(1) as u32;
                let n = mem.read_i16(u_off + 4)?.max(1) as u32;
                AeParamKind::Popup { default: v - 1, count: n }
            }
            _ => return Ok(()),
        };
        let id = if uu_id != 0 { uu_id } else { (self.discovered_params.len() + 1) as i32 };
        self.discovered_params.push(AeParamSpec { id, name, raw_type: param_type, kind });
        Ok(())
    }

    fn build_suite(&mut self, suite_name: &str, ctx: &mut ThunkCallCtx<'_>, vm: &mut GuestVm) -> Result<u64> {
        if let Some(&cached) = self.suite_cache.get(suite_name) {
            return Ok(cached);
        }
        let ptr = match suite_name {
            "PF Iterate8 Suite" => {
                let s = ctx.mem.heap_alloc(64, true)?;
                let iter_fn = vm.register_host_callback(CB_ITERATE8);
                // `iterate`, `iterate_origin`, `iterate_lut`, `iterate_origin_non_clip_src`, `iterate_generic`
                ctx.mem.write_u64(s, iter_fn)?;
                ctx.mem.write_u64(s + 8, iter_fn)?;
                s
            }
            "PF iterateFloat Suite" | "PF IterateFloat Suite" => {
                let s = ctx.mem.heap_alloc(48, true)?;
                let iter_fn = vm.register_host_callback(CB_ITERATE_FLOAT);
                ctx.mem.write_u64(s, iter_fn)?;
                ctx.mem.write_u64(s + 8, iter_fn)?;
                s
            }
            "PF World Transform Suite" | "PF World Suite" => {
                let s = ctx.mem.heap_alloc(96, true)?;
                let copy_fn = vm.register_host_callback(CB_WORLD_COPY);
                let fill_fn = vm.register_host_callback(CB_WORLD_FILL);
                ctx.mem.write_u64(s, copy_fn)?;
                ctx.mem.write_u64(s + 8, copy_fn)?;
                ctx.mem.write_u64(s + 16, copy_fn)?;
                ctx.mem.write_u64(s + 24, fill_fn)?;
                s
            }
            "PF Handle Suite" => {
                let s = ctx.mem.heap_alloc(64, true)?;
                let h_new = vm.register_host_callback(CB_HDL_NEW);
                let h_lock = vm.register_host_callback(CB_HDL_LOCK);
                let h_unlock = vm.register_host_callback(CB_HDL_UNLOCK);
                let h_disp = vm.register_host_callback(CB_HDL_DISPOSE);
                let h_sz = vm.register_host_callback(CB_HDL_SIZE);
                ctx.mem.write_u64(s, h_new)?;
                ctx.mem.write_u64(s + 8, h_lock)?;
                ctx.mem.write_u64(s + 16, h_unlock)?;
                ctx.mem.write_u64(s + 24, h_disp)?;
                ctx.mem.write_u64(s + 32, h_sz)?;
                s
            }
            "PF ANSI Suite" => {
                let s = ctx.mem.heap_alloc(200, true)?;
                let math_names =
                    ["atan", "atan2", "ceil", "cos", "exp", "fabs", "floor", "fmod", "hypot", "log", "log10", "pow", "sin", "sqrt", "tan", "strcpy", "strlen"];
                for (i, m) in math_names.iter().enumerate() {
                    let fn_ptr = vm.register_crt_import(m);
                    ctx.mem.write_u64(s + (i as u64) * 8, fn_ptr)?;
                }
                s
            }
            _ => {
                // Generic 32-slot zero-returning suite vtable so optional UI/AEGP suite queries succeed cleanly
                let s = ctx.mem.heap_alloc(256, true)?;
                let stub = vm.register_host_callback(CB_ABORT);
                for i in 0..32 {
                    ctx.mem.write_u64(s + i * 8, stub)?;
                }
                s
            }
        };
        self.suite_cache.insert(suite_name.to_string(), ptr);
        Ok(ptr)
    }
}

impl HostCallbackHandler for AeHostState {
    fn invoke_host_thunk(&mut self, thunk_id: u32, ctx: &mut ThunkCallCtx<'_>, vm: &mut GuestVm) -> Result<ThunkOutcome> {
        match thunk_id {
            CB_ADD_PARAM => {
                self.parse_added_param(ctx.mem, ctx.arg(2))?;
                Ok(ThunkOutcome::Int(0))
            }
            CB_CHECKOUT_PARAM | CB_CHECKIN_PARAM | CB_ABORT | CB_PROGRESS => Ok(ThunkOutcome::Int(0)),
            CB_PICA_ACQUIRE => {
                let name = ctx.mem.read_cstring(ctx.arg(0), 256)?;
                let out_suite_ptr = ctx.arg(2);
                let suite_addr = self.build_suite(&name, ctx, vm)?;
                if out_suite_ptr != 0 {
                    ctx.mem.write_u64(out_suite_ptr, suite_addr)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_PICA_RELEASE => Ok(ThunkOutcome::Int(0)),
            CB_PICA_ISEQUAL => {
                let a = ctx.mem.read_cstring(ctx.arg(0), 256)?;
                let b = ctx.mem.read_cstring(ctx.arg(1), 256)?;
                Ok(ThunkOutcome::Int(u64::from(a == b)))
            }
            CB_PICA_ALLOC => {
                let ptr = ctx.mem.heap_alloc(ctx.arg(0) as usize, true)?;
                if ctx.arg(1) != 0 {
                    ctx.mem.write_u64(ctx.arg(1), ptr)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_PICA_FREE => {
                ctx.mem.heap_free(ctx.arg(0));
                Ok(ThunkOutcome::Int(0))
            }
            CB_PICA_REALLOC => {
                let ptr = ctx.mem.heap_realloc(ctx.arg(0), ctx.arg(1) as usize)?;
                if ctx.arg(2) != 0 {
                    ctx.mem.write_u64(ctx.arg(2), ptr)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_HDL_NEW => Ok(ThunkOutcome::Int(ctx.mem.new_handle(ctx.arg(0) as usize, true)?)),
            CB_HDL_LOCK => Ok(ThunkOutcome::Int(ctx.mem.lock_handle(ctx.arg(0))?)),
            CB_HDL_UNLOCK => {
                ctx.mem.unlock_handle(ctx.arg(0));
                Ok(ThunkOutcome::Int(0))
            }
            CB_HDL_DISPOSE => {
                ctx.mem.dispose_handle(ctx.arg(0));
                Ok(ThunkOutcome::Int(0))
            }
            CB_HDL_SIZE => Ok(ThunkOutcome::Int(ctx.mem.handle_size(ctx.arg(0)) as u64)),
            CB_WORLD_COPY => {
                // `copy(effect_ref, src_world, dst_world, src_rect, dst_rect)` or `(src_world, dst_world, ...)`
                let (src_w, dst_w) = if ctx.arg(0) == 0xAE00_0001 { (ctx.arg(1), ctx.arg(2)) } else { (ctx.arg(0), ctx.arg(1)) };
                copy_effect_world(ctx.mem, src_w, dst_w)?;
                Ok(ThunkOutcome::Int(0))
            }
            CB_WORLD_FILL => Ok(ThunkOutcome::Int(0)),
            CB_ITERATE8 => {
                // `iterate(in_data, progress_base, progress_final, src_world, area, refcon, pix_fn, dst_world)`
                let src_world = ctx.arg(3);
                let refcon = ctx.arg(5);
                let pix_fn = ctx.arg(6);
                let dst_world = ctx.arg(7);
                run_iterate8_loop(ctx, vm, src_world, dst_world, refcon, pix_fn, self)?;
                Ok(ThunkOutcome::Int(0))
            }
            CB_ITERATE_FLOAT => {
                let src_world = ctx.arg(3);
                let refcon = ctx.arg(5);
                let pix_fn = ctx.arg(6);
                let dst_world = ctx.arg(7);
                run_iterate_float_loop(ctx, vm, src_world, dst_world, refcon, pix_fn, self)?;
                Ok(ThunkOutcome::Int(0))
            }
            other => Err(PluginHostError::Abi(format!("unknown AE/PRM callback {other}"))),
        }
    }
}

fn copy_effect_world(mem: &mut PagedMemory, src_world: u64, dst_world: u64) -> Result<()> {
    if src_world == 0 || dst_world == 0 {
        return Ok(());
    }
    let src_data = mem.read_u64(src_world + 0x18)?;
    let dst_data = mem.read_u64(dst_world + 0x18)?;
    let rb = mem.read_i32(src_world + 0x20)?.max(0) as usize;
    let h = mem.read_i32(src_world + 0x28)?.max(0) as usize;
    let bytes = rb.saturating_mul(h).min(64 << 20);
    if src_data != 0 && dst_data != 0 && bytes > 0 {
        let buf = mem.read_vec(src_data, bytes)?;
        mem.write_bytes(dst_data, &buf)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_iterate8_loop(
    ctx: &mut ThunkCallCtx<'_>,
    vm: &mut GuestVm,
    src_world: u64,
    dst_world: u64,
    refcon: u64,
    pix_fn: u64,
    handler: &mut AeHostState,
) -> Result<()> {
    if dst_world == 0 || pix_fn == 0 {
        return Ok(());
    }
    let w = ctx.mem.read_i32(dst_world + 0x24)?.max(0) as u32;
    let h = ctx.mem.read_i32(dst_world + 0x28)?.max(0) as u32;
    let dst_data = ctx.mem.read_u64(dst_world + 0x18)?;
    let dst_rb = ctx.mem.read_i32(dst_world + 0x20)?.max(0) as u64;
    let (src_data, src_rb) =
        if src_world != 0 { (ctx.mem.read_u64(src_world + 0x18)?, ctx.mem.read_i32(src_world + 0x20)?.max(0) as u64) } else { (dst_data, dst_rb) };
    // Swap `ctx.mem` back into `vm.mem` temporarily while calling guest pixel callback!
    std::mem::swap(&mut vm.mem, ctx.mem);
    let mut call_err: Option<PluginHostError> = None;
    'outer: for y in 0..h {
        for x in 0..w {
            let in_p = src_data + u64::from(y) * src_rb + u64::from(x) * 4;
            let out_p = dst_data + u64::from(y) * dst_rb + u64::from(x) * 4;
            match vm.call_guest(pix_fn, &[refcon, u64::from(x), u64::from(y), in_p, out_p], handler) {
                Ok(err_code) if (err_code as i32) != 0 => break 'outer,
                Ok(_) => {}
                Err(e) => {
                    call_err = Some(e);
                    break 'outer;
                }
            }
        }
    }
    std::mem::swap(&mut vm.mem, ctx.mem);
    if let Some(e) = call_err {
        return Err(e);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_iterate_float_loop(
    ctx: &mut ThunkCallCtx<'_>,
    vm: &mut GuestVm,
    src_world: u64,
    dst_world: u64,
    refcon: u64,
    pix_fn: u64,
    handler: &mut AeHostState,
) -> Result<()> {
    if dst_world == 0 || pix_fn == 0 {
        return Ok(());
    }
    let w = ctx.mem.read_i32(dst_world + 0x24)?.max(0) as u32;
    let h = ctx.mem.read_i32(dst_world + 0x28)?.max(0) as u32;
    let dst_data = ctx.mem.read_u64(dst_world + 0x18)?;
    let dst_rb = ctx.mem.read_i32(dst_world + 0x20)?.max(0) as u64;
    let (src_data, src_rb) =
        if src_world != 0 { (ctx.mem.read_u64(src_world + 0x18)?, ctx.mem.read_i32(src_world + 0x20)?.max(0) as u64) } else { (dst_data, dst_rb) };
    std::mem::swap(&mut vm.mem, ctx.mem);
    let mut call_err: Option<PluginHostError> = None;
    'outer: for y in 0..h {
        for x in 0..w {
            let in_p = src_data + u64::from(y) * src_rb + u64::from(x) * 16;
            let out_p = dst_data + u64::from(y) * dst_rb + u64::from(x) * 16;
            match vm.call_guest(pix_fn, &[refcon, u64::from(x), u64::from(y), in_p, out_p], handler) {
                Ok(err_code) if (err_code as i32) != 0 => break 'outer,
                Ok(_) => {}
                Err(e) => {
                    call_err = Some(e);
                    break 'outer;
                }
            }
        }
    }
    std::mem::swap(&mut vm.mem, ctx.mem);
    if let Some(e) = call_err {
        return Err(e);
    }
    Ok(())
}

struct AePreparedSession {
    vm: GuestVm,
    state: AeHostState,
    ep_addr: u64,
    in_data: u64,
    out_data: u64,
    active_specs: Vec<AeParamSpec>,
    params_ptr_table: u64,
    world_dims: (u32, u32),
    src_world: u64,
    src_pixels_ptr: u64,
    dst_world: u64,
    dst_pixels_ptr: u64,
}

/// Cross-platform After Effects `.aex` & Premiere Pro `.prm` C++ binary host.
pub struct AePrmHost {
    binary: Vec<u8>,
    descriptor: PluginDescriptor,
    params: Vec<AeParamSpec>,
    is_legacy_premiere_xfilter: bool,
    limits: Limits,
    session: Option<AePreparedSession>,
    global_setup_count: u64,
}

impl Clone for AePrmHost {
    fn clone(&self) -> Self {
        Self {
            binary: self.binary.clone(),
            descriptor: self.descriptor.clone(),
            params: self.params.clone(),
            is_legacy_premiere_xfilter: self.is_legacy_premiere_xfilter,
            limits: self.limits,
            session: None,
            global_setup_count: 0,
        }
    }
}

impl AePrmHost {
    pub fn from_bytes(bytes: Vec<u8>, file_hint: Option<&str>, limits: Limits) -> Result<Self> {
        if bytes.len() > limits.max_binary_bytes {
            return Err(PluginHostError::Limit(format!("binary size {} exceeds limit", bytes.len())));
        }
        let mut mem = PagedMemory::new(limits.max_memory_bytes);
        let (info, rsrc_blobs) = load_any_image(&bytes, &mut mem)?;
        let pipl = pipl::parse_pipl_from_blobs(&rsrc_blobs, &bytes);
        let fmt = file_hint.and_then(PluginFormat::from_extension).or(pipl.format_hint).unwrap_or(PluginFormat::AfterEffectsAex);
        let (ep_name, ep_addr) = resolve_plugin_entrypoint(&info, &pipl, fmt)?;
        let is_legacy_premiere_xfilter = ep_name == "xFilter";
        let descriptor = PluginDescriptor {
            plugin_format: fmt,
            binary_format: info.format,
            arch: info.arch,
            guest_os: info.os,
            entrypoint_symbol: ep_name,
            entrypoint_addr: ep_addr,
            pipl,
            exported_symbols: info.exports.keys().cloned().collect(),
        };

        let mut host = Self { binary: bytes, descriptor, params: Vec::new(), is_legacy_premiere_xfilter, limits, session: None, global_setup_count: 0 };
        if !is_legacy_premiere_xfilter {
            // Initialize persistent VM (`GLOBAL_SETUP` + `PARAMS_SETUP` + `SEQUENCE_SETUP`) once!
            if host.ensure_session().is_ok()
                && let Some(sess) = &host.session
            {
                host.params = sess.active_specs.clone();
            }
        }
        Ok(host)
    }

    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    pub fn params(&self) -> &[AeParamSpec] {
        &self.params
    }

    /// Number of times `PF_Cmd_GLOBAL_SETUP` has executed on this host instance.
    pub fn global_setup_count(&self) -> u64 {
        self.global_setup_count
    }

    fn ensure_session(&mut self) -> Result<()> {
        if self.session.is_some() {
            return Ok(());
        }
        let mut mem = PagedMemory::new(self.limits.max_memory_bytes);
        let (info, _) = load_any_image(&self.binary, &mut mem)?;
        let (_, ep_addr) = resolve_plugin_entrypoint(&info, &self.descriptor.pipl, self.descriptor.plugin_format)?;
        let mut vm = GuestVm::new(info.arch, info.os, mem, self.limits);
        vm.bind_imports(&info.imports)?;
        vm.run_dll_attach(info.image_base, info.dll_entry)?;

        let (in_data, out_data) = build_in_out_data(&mut vm, 64, 64, 0.0, self.descriptor.plugin_format)?;
        let mut state = AeHostState::new();
        let _ = vm.call_guest(ep_addr, &[PF_CMD_GLOBAL_SETUP, in_data, out_data, 0, 0, 0], &mut state)?;
        self.global_setup_count += 1;
        copy_handles_out_to_in(&mut vm.mem, in_data, out_data)?;
        vm.reset_fuel(self.limits.instruction_budget);
        let _ = vm.call_guest(ep_addr, &[PF_CMD_PARAMS_SETUP, in_data, out_data, 0, 0, 0], &mut state)?;
        let active_specs = if !state.discovered_params.is_empty() { state.discovered_params.clone() } else { self.params.clone() };

        let count = active_specs.len() + 1;
        let params_ptr_table = vm.mem.heap_alloc(count * 8, true)?;
        for idx in 0..count {
            let pdef = vm.mem.heap_alloc(PARAM_DEF_SIZE, true)?;
            vm.mem.write_u64(params_ptr_table + (idx as u64) * 8, pdef)?;
        }
        vm.mem.write_i32(in_data + 0xCC, count as i32)?;
        vm.reset_fuel(self.limits.instruction_budget);
        let _ = vm.call_guest(ep_addr, &[PF_CMD_SEQUENCE_SETUP, in_data, out_data, params_ptr_table, 0, 0], &mut state);
        copy_handles_out_to_in(&mut vm.mem, in_data, out_data)?;

        self.session = Some(AePreparedSession {
            vm,
            state,
            ep_addr,
            in_data,
            out_data,
            active_specs,
            params_ptr_table,
            world_dims: (0, 0),
            src_world: 0,
            src_pixels_ptr: 0,
            dst_world: 0,
            dst_pixels_ptr: 0,
        });
        Ok(())
    }

    /// Renders a frame reusing the persistent `PF_Cmd_GLOBAL_SETUP` / `PF_Cmd_SEQUENCE_SETUP` VM state!
    pub fn render_reusing_setup(&mut self, input: &RgbaFrame, param_values: &[f64], time_seconds: f64, fps: f64) -> Result<RgbaFrame> {
        if input.width == 0 || input.height == 0 {
            return Ok(input.clone());
        }
        if self.is_legacy_premiere_xfilter {
            return self.render_legacy_premiere_xfilter(input);
        }
        self.ensure_session()?;
        let budget = self.limits.instruction_budget;
        let outcome = if let Some(sess) = &mut self.session {
            exec_prepared_render(sess, input, param_values, time_seconds, fps, budget)
        } else {
            Err(PluginHostError::Abi("missing prepared AE/PRM session".into()))
        };
        if outcome.is_err() {
            // Reset persistent VM on trap/error so future frames recover cleanly.
            self.session = None;
        }
        outcome
    }

    /// Renders a frame through the C++ `.aex` or `.prm` binary plugin!
    /// `param_values` provides flattened user values corresponding to [`Self::params`].
    pub fn render(&self, input: &RgbaFrame, param_values: &[f64], time_seconds: f64) -> Result<RgbaFrame> {
        if input.width == 0 || input.height == 0 {
            return Ok(input.clone());
        }
        if self.is_legacy_premiere_xfilter {
            return self.render_legacy_premiere_xfilter(input);
        }
        let mut mem = PagedMemory::new(self.limits.max_memory_bytes);
        let (info, _) = load_any_image(&self.binary, &mut mem)?;
        let (_, ep_addr) = resolve_plugin_entrypoint(&info, &self.descriptor.pipl, self.descriptor.plugin_format)?;
        let mut vm = GuestVm::new(info.arch, info.os, mem, self.limits);
        vm.bind_imports(&info.imports)?;
        vm.run_dll_attach(info.image_base, info.dll_entry)?;

        let (in_data, out_data) = build_in_out_data(&mut vm, input.width, input.height, time_seconds, self.descriptor.plugin_format)?;
        let mut state = AeHostState::new();

        // 1. GLOBAL_SETUP & PARAMS_SETUP & SEQUENCE_SETUP
        let _ = vm.call_guest(ep_addr, &[PF_CMD_GLOBAL_SETUP, in_data, out_data, 0, 0, 0], &mut state)?;
        copy_handles_out_to_in(&mut vm.mem, in_data, out_data)?;
        vm.reset_fuel(self.limits.instruction_budget);
        let _ = vm.call_guest(ep_addr, &[PF_CMD_PARAMS_SETUP, in_data, out_data, 0, 0, 0], &mut state)?;
        let active_specs = if !state.discovered_params.is_empty() { state.discovered_params.clone() } else { self.params.clone() };

        // 2. Build input & output 8-bit ARGB `PF_EffectWorld` + `params[]` pointer array
        let (src_world, src_pixels_ptr) = alloc_argb8_world(&mut vm.mem, input)?;
        let (dst_world, dst_pixels_ptr) = alloc_argb8_world(&mut vm.mem, input)?;
        let params_array_ptr = build_params_array(&mut vm.mem, src_world, &active_specs, param_values)?;
        vm.mem.write_i32(in_data + 0xCC, (active_specs.len() + 1) as i32)?;

        let _ = vm.call_guest(ep_addr, &[PF_CMD_SEQUENCE_SETUP, in_data, out_data, params_array_ptr, dst_world, 0], &mut state);
        copy_handles_out_to_in(&mut vm.mem, in_data, out_data)?;
        let _ = vm.call_guest(ep_addr, &[PF_CMD_FRAME_SETUP, in_data, out_data, params_array_ptr, dst_world, 0], &mut state);
        copy_handles_out_to_in(&mut vm.mem, in_data, out_data)?;

        // 3. PF_Cmd_RENDER (11)
        vm.reset_fuel(self.limits.instruction_budget);
        let err = vm.call_guest(ep_addr, &[PF_CMD_RENDER, in_data, out_data, params_array_ptr, dst_world, 0], &mut state)? as i32;
        if err != 0 {
            return Err(PluginHostError::PluginError(err));
        }

        let _ = src_pixels_ptr;
        read_argb8_world(&vm.mem, dst_pixels_ptr, input.width, input.height)
    }

    fn render_legacy_premiere_xfilter(&self, input: &RgbaFrame) -> Result<RgbaFrame> {
        let mut mem = PagedMemory::new(self.limits.max_memory_bytes);
        let (info, _) = load_any_image(&self.binary, &mut mem)?;
        let (_, ep_addr) = resolve_plugin_entrypoint(&info, &self.descriptor.pipl, PluginFormat::PremierePrm)?;
        let mut vm = GuestVm::new(info.arch, info.os, mem, self.limits);
        vm.bind_imports(&info.imports)?;
        vm.run_dll_attach(info.image_base, info.dll_entry)?;

        // Classic Premiere `VideoRecord` (`theData` is a `Handle` -> `VideoRecord*`):
        // `+0x00`: `src_ppix_ptr`, `+0x08`: `dst_ppix_ptr`, `+0x10`: `width: i32`, `+0x14`: `height: i32`, `+0x18`: `rowbytes: i32`
        let rowbytes = (input.width as usize) * 4;
        let total = rowbytes * (input.height as usize);
        let src_px = vm.mem.heap_alloc(total, true)?;
        let dst_px = vm.mem.heap_alloc(total, true)?;
        let mut raw = vec![0u8; total];
        for (i, px) in input.pixels.iter().enumerate() {
            // Premiere BGRA8 layout `[B, G, R, A]`
            raw[i * 4] = (px[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            raw[i * 4 + 1] = (px[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            raw[i * 4 + 2] = (px[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            raw[i * 4 + 3] = (px[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
        vm.mem.write_bytes(src_px, &raw)?;
        vm.mem.write_bytes(dst_px, &raw)?;

        let video_rec_hdl = vm.mem.new_handle(64, true)?;
        let video_rec = vm.mem.lock_handle(video_rec_hdl)?;
        vm.mem.write_u64(video_rec, src_px)?;
        vm.mem.write_u64(video_rec + 0x08, dst_px)?;
        vm.mem.write_i32(video_rec + 0x10, input.width as i32)?;
        vm.mem.write_i32(video_rec + 0x14, input.height as i32)?;
        vm.mem.write_i32(video_rec + 0x18, rowbytes as i32)?;

        let mut state = AeHostState::new();
        // selector 2 = fsExecute
        let err = vm.call_guest(ep_addr, &[2, video_rec_hdl], &mut state)? as i32;
        if err < 0 {
            return Err(PluginHostError::PluginError(err));
        }
        let out_raw = vm.mem.read_vec(dst_px, total)?;
        let mut pixels = Vec::with_capacity(input.pixels.len());
        for c in out_raw.as_chunks::<4>().0 {
            pixels.push([f32::from(c[2]) / 255.0, f32::from(c[1]) / 255.0, f32::from(c[0]) / 255.0, f32::from(c[3]) / 255.0]);
        }
        Ok(RgbaFrame { width: input.width, height: input.height, pixels })
    }
}

fn build_in_out_data(vm: &mut GuestVm, width: u32, height: u32, time_seconds: f64, fmt: PluginFormat) -> Result<(u64, u64)> {
    let in_data = vm.mem.heap_alloc(0x200, true)?;
    let out_data = vm.mem.heap_alloc(0x1A0, true)?;
    let utils = vm.mem.heap_alloc(0x100, true)?;
    let sp_basic = vm.mem.heap_alloc(64, true)?;

    let checkout_fn = vm.register_host_callback(CB_CHECKOUT_PARAM);
    let checkin_fn = vm.register_host_callback(CB_CHECKIN_PARAM);
    let add_param_fn = vm.register_host_callback(CB_ADD_PARAM);
    let abort_fn = vm.register_host_callback(CB_ABORT);
    let prog_fn = vm.register_host_callback(CB_PROGRESS);

    vm.mem.write_u64(in_data, checkout_fn)?;
    vm.mem.write_u64(in_data + 0x08, checkin_fn)?;
    vm.mem.write_u64(in_data + 0x10, add_param_fn)?;
    vm.mem.write_u64(in_data + 0x18, abort_fn)?;
    vm.mem.write_u64(in_data + 0x20, prog_fn)?;

    // PICA SPBasicSuite at `in_data + 0x180`
    let acq = vm.register_host_callback(CB_PICA_ACQUIRE);
    let rel = vm.register_host_callback(CB_PICA_RELEASE);
    let eq = vm.register_host_callback(CB_PICA_ISEQUAL);
    let al = vm.register_host_callback(CB_PICA_ALLOC);
    let fr = vm.register_host_callback(CB_PICA_FREE);
    let ra = vm.register_host_callback(CB_PICA_REALLOC);
    vm.mem.write_u64(sp_basic, acq)?;
    vm.mem.write_u64(sp_basic + 0x08, rel)?;
    vm.mem.write_u64(sp_basic + 0x10, eq)?;
    vm.mem.write_u64(sp_basic + 0x18, al)?;
    vm.mem.write_u64(sp_basic + 0x20, fr)?;
    vm.mem.write_u64(sp_basic + 0x28, ra)?;

    // PF_UtilCallbacks basic pointers
    let iter8 = vm.register_host_callback(CB_ITERATE8);
    let wcopy = vm.register_host_callback(CB_WORLD_COPY);
    vm.mem.write_u64(utils + 0x18, iter8)?;
    vm.mem.write_u64(utils + 0x28, wcopy)?;

    vm.mem.write_u64(in_data + 0x98, utils)?;
    vm.mem.write_u64(in_data + 0xB8, 0xAE00_0001)?; // effect_ref
    vm.mem.write_i32(in_data + 0xBC, 1)?; // PF_Quality_HI
    vm.mem.write_u16(in_data + 0xC0, 13)?;
    vm.mem.write_u16(in_data + 0xC2, 28)?;
    let appl_id = if fmt == PluginFormat::PremierePrm { 0x5072_4D72 } else { 0x4658_5443 }; // 'PrMr' vs 'FXTC'
    vm.mem.write_i32(in_data + 0xC8, appl_id)?;
    let cur_time = (time_seconds.max(0.0) * 1000.0).round() as i32;
    vm.mem.write_i32(in_data + 0xE0, cur_time)?;
    vm.mem.write_i32(in_data + 0xE4, 33)?;
    vm.mem.write_i32(in_data + 0xE8, 60_000)?;
    vm.mem.write_u32(in_data + 0xF0, 1000)?;
    vm.mem.write_i32(in_data + 0xFC, width as i32)?;
    vm.mem.write_i32(in_data + 0x100, height as i32)?;
    vm.mem.write_i32(in_data + 0x10C, width as i32)?;
    vm.mem.write_i32(in_data + 0x110, height as i32)?;
    vm.mem.write_i16(in_data + 0x11C, 1)?;
    vm.mem.write_i16(in_data + 0x11E, 1)?;
    vm.mem.write_i16(in_data + 0x120, 1)?;
    vm.mem.write_i16(in_data + 0x122, 1)?;
    vm.mem.write_i32(in_data + 0x124, 1)?;
    vm.mem.write_i32(in_data + 0x128, 1)?;
    vm.mem.write_u64(in_data + 0x180, sp_basic)?;

    vm.mem.write_i32(out_data + 0x50, width as i32)?;
    vm.mem.write_i32(out_data + 0x54, height as i32)?;
    Ok((in_data, out_data))
}

fn copy_handles_out_to_in(mem: &mut PagedMemory, in_data: u64, out_data: u64) -> Result<()> {
    let g = mem.read_u64(out_data + 0x28)?;
    if g != 0 {
        mem.write_u64(in_data + 0x130, g)?;
    }
    let s = mem.read_u64(out_data + 0x38)?;
    if s != 0 {
        mem.write_u64(in_data + 0x138, s)?;
    }
    let f = mem.read_u64(out_data + 0x48)?;
    if f != 0 {
        mem.write_u64(in_data + 0x140, f)?;
    }
    Ok(())
}

fn alloc_argb8_world(mem: &mut PagedMemory, frame: &RgbaFrame) -> Result<(u64, u64)> {
    let world = mem.heap_alloc(0x58, true)?;
    let rowbytes = (frame.width as usize) * 4;
    let total = rowbytes * (frame.height as usize);
    let data_ptr = mem.heap_alloc(total.max(16), true)?;
    let mut raw = vec![0u8; total];
    for (i, px) in frame.pixels.iter().enumerate() {
        // After Effects 8-bit `PF_Pixel` order is `[alpha, red, green, blue]`!
        raw[i * 4] = (px[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        raw[i * 4 + 1] = (px[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        raw[i * 4 + 2] = (px[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        raw[i * 4 + 3] = (px[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    }
    mem.write_bytes(data_ptr, &raw)?;
    mem.write_u32(world + 0x10, 2)?; // PF_WorldFlag_WRITEABLE
    mem.write_u64(world + 0x18, data_ptr)?;
    mem.write_i32(world + 0x20, rowbytes as i32)?;
    mem.write_i32(world + 0x24, frame.width as i32)?;
    mem.write_i32(world + 0x28, frame.height as i32)?;
    mem.write_i32(world + 0x34, frame.width as i32)?;
    mem.write_i32(world + 0x38, frame.height as i32)?;
    mem.write_i32(world + 0x3C, 1)?;
    mem.write_i32(world + 0x40, 1)?;
    Ok((world, data_ptr))
}

fn read_argb8_world(mem: &PagedMemory, data_ptr: u64, width: u32, height: u32) -> Result<RgbaFrame> {
    let total = (width as usize) * (height as usize) * 4;
    let raw = mem.read_vec(data_ptr, total)?;
    let mut pixels = Vec::with_capacity((width as usize) * (height as usize));
    for c in raw.as_chunks::<4>().0 {
        pixels.push([f32::from(c[1]) / 255.0, f32::from(c[2]) / 255.0, f32::from(c[3]) / 255.0, f32::from(c[0]) / 255.0]);
    }
    Ok(RgbaFrame { width, height, pixels })
}

fn build_params_array(mem: &mut PagedMemory, src_world: u64, specs: &[AeParamSpec], values: &[f64]) -> Result<u64> {
    let count = specs.len() + 1;
    let ptr_table = mem.heap_alloc(count * 8, true)?;

    // Slot 0: input layer (`PF_LayerDef` copied into `param0 + 0x38`)
    let p0 = mem.heap_alloc(PARAM_DEF_SIZE, true)?;
    let world_bytes = mem.read_vec(src_world, 0x58)?;
    mem.write_i32(p0 + 0x0C, 1)?; // PF_Param_LAYER
    mem.write_bytes(p0 + 0x38, &world_bytes)?;
    mem.write_u64(ptr_table, p0)?;

    let mut val_cursor = 0usize;
    for (idx, spec) in specs.iter().enumerate() {
        let pdef = mem.heap_alloc(PARAM_DEF_SIZE, true)?;
        mem.write_i32(pdef, spec.id)?;
        mem.write_i32(pdef + 0x0C, spec.raw_type)?;
        let u_off = pdef + 0x38;
        match &spec.kind {
            AeParamKind::Slider { default, .. } => {
                let v = values.get(val_cursor).copied().unwrap_or(*default);
                val_cursor += 1;
                if spec.raw_type == PF_PARAM_FLOAT_SLIDER {
                    mem.write_f64(u_off, v)?;
                } else {
                    mem.write_i32(u_off, (v * 65536.0).round() as i32)?;
                }
            }
            AeParamKind::Angle { default } => {
                let v = values.get(val_cursor).copied().unwrap_or(*default);
                val_cursor += 1;
                mem.write_i32(u_off, (v * 65536.0).round() as i32)?;
            }
            AeParamKind::Checkbox { default } => {
                let v = values.get(val_cursor).map_or(*default, |&x| x != 0.0);
                val_cursor += 1;
                mem.write_i32(u_off, i32::from(v))?;
            }
            AeParamKind::Color { default } => {
                let r = values.get(val_cursor).copied().unwrap_or(default[0]);
                let g = values.get(val_cursor + 1).copied().unwrap_or(default[1]);
                let b = values.get(val_cursor + 2).copied().unwrap_or(default[2]);
                let a = values.get(val_cursor + 3).copied().unwrap_or(default[3]);
                val_cursor += 4;
                mem.write_u8(u_off, (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)?;
                mem.write_u8(u_off + 1, (r.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)?;
                mem.write_u8(u_off + 2, (g.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)?;
                mem.write_u8(u_off + 3, (b.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)?;
            }
            AeParamKind::Point { default } => {
                let x = values.get(val_cursor).copied().unwrap_or(default[0]);
                let y = values.get(val_cursor + 1).copied().unwrap_or(default[1]);
                val_cursor += 2;
                mem.write_i32(u_off, (x * 65536.0).round() as i32)?;
                mem.write_i32(u_off + 4, (y * 65536.0).round() as i32)?;
            }
            AeParamKind::Popup { default, .. } => {
                let v = values.get(val_cursor).map_or(*default, |&x| x.max(0.0) as u32);
                val_cursor += 1;
                mem.write_i32(u_off, (v + 1) as i32)?;
            }
        }
        mem.write_u64(ptr_table + ((idx + 1) as u64) * 8, pdef)?;
    }
    Ok(ptr_table)
}

fn write_argb8_pixels_into(mem: &mut PagedMemory, world: u64, data_ptr: u64, frame: &RgbaFrame) -> Result<()> {
    let rowbytes = (frame.width as usize) * 4;
    let total = rowbytes * (frame.height as usize);
    let mut raw = vec![0u8; total];
    for (i, px) in frame.pixels.iter().enumerate() {
        raw[i * 4] = (px[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        raw[i * 4 + 1] = (px[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        raw[i * 4 + 2] = (px[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        raw[i * 4 + 3] = (px[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    }
    mem.write_bytes(data_ptr, &raw)?;
    mem.write_u32(world + 0x10, 2)?;
    mem.write_u64(world + 0x18, data_ptr)?;
    mem.write_i32(world + 0x20, rowbytes as i32)?;
    mem.write_i32(world + 0x24, frame.width as i32)?;
    mem.write_i32(world + 0x28, frame.height as i32)?;
    mem.write_i32(world + 0x34, frame.width as i32)?;
    mem.write_i32(world + 0x38, frame.height as i32)?;
    mem.write_i32(world + 0x3C, 1)?;
    mem.write_i32(world + 0x40, 1)?;
    Ok(())
}

fn update_params_table_in_place(mem: &mut PagedMemory, ptr_table: u64, src_world: u64, specs: &[AeParamSpec], values: &[f64]) -> Result<()> {
    let p0 = mem.read_u64(ptr_table)?;
    let world_bytes = mem.read_vec(src_world, 0x58)?;
    mem.write_i32(p0 + 0x0C, 1)?;
    mem.write_bytes(p0 + 0x38, &world_bytes)?;

    let mut val_cursor = 0usize;
    for (idx, spec) in specs.iter().enumerate() {
        let pdef = mem.read_u64(ptr_table + ((idx + 1) as u64) * 8)?;
        mem.write_i32(pdef, spec.id)?;
        mem.write_i32(pdef + 0x0C, spec.raw_type)?;
        let u_off = pdef + 0x38;
        match &spec.kind {
            AeParamKind::Slider { default, .. } => {
                let v = values.get(val_cursor).copied().unwrap_or(*default);
                val_cursor += 1;
                if spec.raw_type == PF_PARAM_FLOAT_SLIDER {
                    mem.write_f64(u_off, v)?;
                } else {
                    mem.write_i32(u_off, (v * 65536.0).round() as i32)?;
                }
            }
            AeParamKind::Angle { default } => {
                let v = values.get(val_cursor).copied().unwrap_or(*default);
                val_cursor += 1;
                mem.write_i32(u_off, (v * 65536.0).round() as i32)?;
            }
            AeParamKind::Checkbox { default } => {
                let v = values.get(val_cursor).map_or(*default, |&x| x != 0.0);
                val_cursor += 1;
                mem.write_i32(u_off, i32::from(v))?;
            }
            AeParamKind::Color { default } => {
                let r = values.get(val_cursor).copied().unwrap_or(default[0]);
                let g = values.get(val_cursor + 1).copied().unwrap_or(default[1]);
                let b = values.get(val_cursor + 2).copied().unwrap_or(default[2]);
                let a = values.get(val_cursor + 3).copied().unwrap_or(default[3]);
                val_cursor += 4;
                mem.write_u8(u_off, (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)?;
                mem.write_u8(u_off + 1, (r.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)?;
                mem.write_u8(u_off + 2, (g.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)?;
                mem.write_u8(u_off + 3, (b.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)?;
            }
            AeParamKind::Point { default } => {
                let x = values.get(val_cursor).copied().unwrap_or(default[0]);
                let y = values.get(val_cursor + 1).copied().unwrap_or(default[1]);
                val_cursor += 2;
                mem.write_i32(u_off, (x * 65536.0).round() as i32)?;
                mem.write_i32(u_off + 4, (y * 65536.0).round() as i32)?;
            }
            AeParamKind::Popup { default, .. } => {
                let v = values.get(val_cursor).map_or(*default, |&x| x.max(0.0) as u32);
                val_cursor += 1;
                mem.write_i32(u_off, (v + 1) as i32)?;
            }
        }
    }
    Ok(())
}

fn exec_prepared_render(sess: &mut AePreparedSession, input: &RgbaFrame, param_values: &[f64], time_seconds: f64, fps: f64, budget: u64) -> Result<RgbaFrame> {
    if sess.world_dims != (input.width, input.height) || sess.src_world == 0 {
        if sess.src_pixels_ptr != 0 {
            sess.vm.mem.heap_free(sess.src_pixels_ptr);
            sess.vm.mem.heap_free(sess.dst_pixels_ptr);
        }
        if sess.src_world == 0 {
            sess.src_world = sess.vm.mem.heap_alloc(0x58, true)?;
            sess.dst_world = sess.vm.mem.heap_alloc(0x58, true)?;
        }
        let total = (input.width as usize).saturating_mul(input.height as usize).saturating_mul(4).max(16);
        sess.src_pixels_ptr = sess.vm.mem.heap_alloc(total, true)?;
        sess.dst_pixels_ptr = sess.vm.mem.heap_alloc(total, true)?;
        sess.world_dims = (input.width, input.height);
    }

    write_argb8_pixels_into(&mut sess.vm.mem, sess.src_world, sess.src_pixels_ptr, input)?;
    write_argb8_pixels_into(&mut sess.vm.mem, sess.dst_world, sess.dst_pixels_ptr, input)?;
    update_params_table_in_place(&mut sess.vm.mem, sess.params_ptr_table, sess.src_world, &sess.active_specs, param_values)?;

    let safe_fps = if fps.is_finite() && fps > 0.0 { fps.clamp(1.0, 240.0) } else { 30.0 };
    let cur_time = (time_seconds.max(0.0) * 1000.0).round() as i32;
    let time_step = (1000.0 / safe_fps).round().max(1.0) as i32;
    sess.vm.mem.write_i32(sess.in_data + 0xCC, (sess.active_specs.len() + 1) as i32)?;
    sess.vm.mem.write_i32(sess.in_data + 0xE0, cur_time)?;
    sess.vm.mem.write_i32(sess.in_data + 0xE4, time_step)?;
    sess.vm.mem.write_i32(sess.in_data + 0xFC, input.width as i32)?;
    sess.vm.mem.write_i32(sess.in_data + 0x100, input.height as i32)?;
    sess.vm.mem.write_i32(sess.in_data + 0x10C, input.width as i32)?;
    sess.vm.mem.write_i32(sess.in_data + 0x110, input.height as i32)?;
    sess.vm.mem.write_i32(sess.out_data + 0x50, input.width as i32)?;
    sess.vm.mem.write_i32(sess.out_data + 0x54, input.height as i32)?;

    sess.vm.reset_fuel(budget);
    let _ = sess.vm.call_guest(sess.ep_addr, &[PF_CMD_FRAME_SETUP, sess.in_data, sess.out_data, sess.params_ptr_table, sess.dst_world, 0], &mut sess.state);
    copy_handles_out_to_in(&mut sess.vm.mem, sess.in_data, sess.out_data)?;

    sess.vm.reset_fuel(budget);
    let err =
        sess.vm.call_guest(sess.ep_addr, &[PF_CMD_RENDER, sess.in_data, sess.out_data, sess.params_ptr_table, sess.dst_world, 0], &mut sess.state)? as i32;
    if err != 0 {
        return Err(PluginHostError::PluginError(err));
    }
    read_argb8_world(&sess.vm.mem, sess.dst_pixels_ptr, input.width, input.height)
}
