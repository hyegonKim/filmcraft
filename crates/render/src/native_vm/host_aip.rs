//! Adobe Illustrator `.aip` SweetPea / PICA C++ Plugin Host SDK runtime.
//!
//! Marshals `SPMessageData` (`check = 'SPCk' = 0x5350436b`, `basic: SPBasicSuite*` @ `+0x18`),
//! `AIFilterSuite`, `AILiveEffectSuite`, `AIMenuSuite`, `AIArtSuite`, `AIMatchingArtSuite`,
//! `AIMdMemorySuite`, and `AIPathSuite` (`AIPathSegment` 56-byte `AIReal` Bezier anchors) so
//! compiled C++ `.aip` vector filters & live effects execute deterministically on any OS.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::crt::{GuestVm, HostCallbackHandler, ThunkCallCtx, ThunkOutcome};
use super::mem::PagedMemory;
use super::{Limits, PluginDescriptor, PluginFormat, PluginHostError, Result, load_any_image, pipl, resolve_plugin_entrypoint};

const SP_CHECK_MAGIC: i32 = 0x5350_436B; // 'SPCk' — verified against AddAnchor.aip
const ART_HANDLE_BASE: u64 = 0xA1A7_0000;
const PATH_SEG_BYTES: u64 = 56; // 6 * f64 (p, in, out) + i32 corner + 4-byte align

const CB_SP_ACQUIRE: u32 = 200;
const CB_SP_RELEASE: u32 = 201;
const CB_SP_ISEQUAL: u32 = 202;
const CB_SP_ALLOC: u32 = 203;
const CB_SP_FREE: u32 = 204;
const CB_SP_REALLOC: u32 = 205;
const CB_NOOP_OK: u32 = 206;

const CB_FILTER_ADD: u32 = 210;
const CB_LIVEEFFECT_ADD: u32 = 211;
const CB_MENU_ADD: u32 = 212;

const CB_ART_FIRST: u32 = 220;
const CB_ART_TYPE: u32 = 221;
const CB_ART_NEW: u32 = 222;
const CB_ART_DISPOSE: u32 = 223;
const CB_MATCH_SELECTED: u32 = 225;

const CB_MD_NEW: u32 = 230;
const CB_MD_DISPOSE: u32 = 231;
const CB_MD_SIZE: u32 = 232;
const CB_MD_RESIZE: u32 = 233;
const CB_MD_LOCK: u32 = 234;
const CB_MD_UNLOCK: u32 = 235;

const CB_PATH_GET_COUNT: u32 = 240;
const CB_PATH_SET_COUNT: u32 = 241;
const CB_PATH_GET_SEGS: u32 = 242;
const CB_PATH_SET_SEGS: u32 = 243;
const CB_PATH_INSERT_SEGS: u32 = 244;
const CB_PATH_REMOVE_SEGS: u32 = 245;
const CB_PATH_GET_CLOSED: u32 = 246;
const CB_PATH_SET_CLOSED: u32 = 247;

/// One cubic Bezier anchor in Illustrator's `AIPathSegment` coordinate model.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AipBezierSegment {
    pub p: [f64; 2],
    pub h_in: [f64; 2],
    pub h_out: [f64; 2],
    pub corner: bool,
}

/// One vector subpath passed into and returned from [`AipHost::execute_paths`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AipSubpath {
    pub closed: bool,
    pub segments: Vec<AipBezierSegment>,
}

#[derive(Clone, Debug)]
struct GuestArtObject {
    alive: bool,
    subpath: AipSubpath,
}

struct AipRuntimeState {
    arts: Vec<GuestArtObject>,
    registered_filters: Vec<(u64, String)>,
    registered_effects: Vec<(u64, String)>,
    registered_menus: Vec<u64>,
    suite_cache: HashMap<String, u64>,
}

impl AipRuntimeState {
    fn new(inputs: &[AipSubpath]) -> Self {
        let arts = inputs.iter().cloned().map(|subpath| GuestArtObject { alive: true, subpath }).collect();
        Self { arts, registered_filters: Vec::new(), registered_effects: Vec::new(), registered_menus: Vec::new(), suite_cache: HashMap::new() }
    }

    fn art_idx(handle: u64) -> Option<usize> {
        if ((ART_HANDLE_BASE + 1)..(ART_HANDLE_BASE + 1_000_000)).contains(&handle) { Some((handle - ART_HANDLE_BASE - 1) as usize) } else { None }
    }

    fn handle_for(idx: usize) -> u64 {
        ART_HANDLE_BASE + 1 + (idx as u64)
    }

    fn build_suite(&mut self, name: &str, ctx: &mut ThunkCallCtx<'_>, vm: &mut GuestVm) -> Result<u64> {
        if let Some(&cached) = self.suite_cache.get(name) {
            return Ok(cached);
        }
        let noop = vm.register_host_callback(CB_NOOP_OK);
        let ptr = match name {
            "AI Filter Suite" => {
                let s = ctx.mem.heap_alloc(128, true)?;
                let add_fn = vm.register_host_callback(CB_FILTER_ADD);
                ctx.mem.write_u64(s, add_fn)?;
                for i in 1..16 {
                    ctx.mem.write_u64(s + i * 8, noop)?;
                }
                s
            }
            "AI Live Effect Suite" => {
                let s = ctx.mem.heap_alloc(128, true)?;
                let add_fn = vm.register_host_callback(CB_LIVEEFFECT_ADD);
                ctx.mem.write_u64(s, add_fn)?;
                for i in 1..16 {
                    ctx.mem.write_u64(s + i * 8, noop)?;
                }
                s
            }
            "AI Menu Suite" => {
                let s = ctx.mem.heap_alloc(160, true)?;
                let add_fn = vm.register_host_callback(CB_MENU_ADD);
                ctx.mem.write_u64(s, add_fn)?;
                for i in 1..20 {
                    ctx.mem.write_u64(s + i * 8, noop)?;
                }
                s
            }
            "AI Art Suite" => {
                let s = ctx.mem.heap_alloc(256, true)?;
                let new_art = vm.register_host_callback(CB_ART_NEW);
                let disp_art = vm.register_host_callback(CB_ART_DISPOSE);
                let first_art = vm.register_host_callback(CB_ART_FIRST);
                let art_type = vm.register_host_callback(CB_ART_TYPE);
                for i in 0..32 {
                    ctx.mem.write_u64(s + i * 8, noop)?;
                }
                ctx.mem.write_u64(s, new_art)?;
                ctx.mem.write_u64(s + 0x08, disp_art)?;
                ctx.mem.write_u64(s + 0x20, first_art)?;
                ctx.mem.write_u64(s + 0x38, art_type)?;
                s
            }
            "AI Matching Art Suite" => {
                let s = ctx.mem.heap_alloc(64, true)?;
                let get_sel = vm.register_host_callback(CB_MATCH_SELECTED);
                ctx.mem.write_u64(s, get_sel)?;
                ctx.mem.write_u64(s + 8, get_sel)?;
                s
            }
            "AI MdMemory Suite" => {
                let s = ctx.mem.heap_alloc(64, true)?;
                let mn = vm.register_host_callback(CB_MD_NEW);
                let md = vm.register_host_callback(CB_MD_DISPOSE);
                let ms = vm.register_host_callback(CB_MD_SIZE);
                let mr = vm.register_host_callback(CB_MD_RESIZE);
                let ml = vm.register_host_callback(CB_MD_LOCK);
                let mu = vm.register_host_callback(CB_MD_UNLOCK);
                ctx.mem.write_u64(s, mn)?;
                ctx.mem.write_u64(s + 0x08, md)?;
                ctx.mem.write_u64(s + 0x10, ms)?;
                ctx.mem.write_u64(s + 0x18, mr)?;
                ctx.mem.write_u64(s + 0x20, ml)?;
                ctx.mem.write_u64(s + 0x28, mu)?;
                s
            }
            "SP Blocks Suite" => {
                let s = ctx.mem.heap_alloc(32, true)?;
                let a = vm.register_host_callback(CB_SP_ALLOC);
                let f = vm.register_host_callback(CB_SP_FREE);
                let r = vm.register_host_callback(CB_SP_REALLOC);
                ctx.mem.write_u64(s, a)?;
                ctx.mem.write_u64(s + 8, f)?;
                ctx.mem.write_u64(s + 16, r)?;
                s
            }
            "AI Path Suite" => {
                let s = ctx.mem.heap_alloc(200, true)?;
                let g_cnt = vm.register_host_callback(CB_PATH_GET_COUNT);
                let s_cnt = vm.register_host_callback(CB_PATH_SET_COUNT);
                let g_seg = vm.register_host_callback(CB_PATH_GET_SEGS);
                let s_seg = vm.register_host_callback(CB_PATH_SET_SEGS);
                let i_seg = vm.register_host_callback(CB_PATH_INSERT_SEGS);
                let r_seg = vm.register_host_callback(CB_PATH_REMOVE_SEGS);
                let g_cls = vm.register_host_callback(CB_PATH_GET_CLOSED);
                let s_cls = vm.register_host_callback(CB_PATH_SET_CLOSED);
                ctx.mem.write_u64(s, g_cnt)?;
                ctx.mem.write_u64(s + 0x08, s_cnt)?;
                ctx.mem.write_u64(s + 0x10, g_seg)?;
                ctx.mem.write_u64(s + 0x18, s_seg)?;
                ctx.mem.write_u64(s + 0x20, i_seg)?;
                ctx.mem.write_u64(s + 0x28, r_seg)?;
                ctx.mem.write_u64(s + 0x30, g_cls)?;
                ctx.mem.write_u64(s + 0x38, s_cls)?;
                for i in 8..24 {
                    ctx.mem.write_u64(s + i * 8, noop)?;
                }
                s
            }
            _ => {
                let s = ctx.mem.heap_alloc(256, true)?;
                for i in 0..32 {
                    ctx.mem.write_u64(s + i * 8, noop)?;
                }
                s
            }
        };
        self.suite_cache.insert(name.to_string(), ptr);
        Ok(ptr)
    }
}

impl HostCallbackHandler for AipRuntimeState {
    fn invoke_host_thunk(&mut self, thunk_id: u32, ctx: &mut ThunkCallCtx<'_>, vm: &mut GuestVm) -> Result<ThunkOutcome> {
        match thunk_id {
            CB_NOOP_OK | CB_SP_RELEASE | CB_MD_UNLOCK => Ok(ThunkOutcome::Int(0)),
            CB_SP_ACQUIRE => {
                let name = ctx.mem.read_cstring(ctx.arg(0), 256)?;
                let out_ptr = ctx.arg(2);
                let suite = self.build_suite(&name, ctx, vm)?;
                if out_ptr != 0 {
                    ctx.mem.write_u64(out_ptr, suite)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_SP_ISEQUAL => {
                let a = ctx.mem.read_cstring(ctx.arg(0), 256)?;
                let b = ctx.mem.read_cstring(ctx.arg(1), 256)?;
                Ok(ThunkOutcome::Int(u64::from(a == b)))
            }
            CB_SP_ALLOC => {
                let ptr = ctx.mem.heap_alloc(ctx.arg(0) as usize, true)?;
                if ctx.arg(1) != 0 {
                    ctx.mem.write_u64(ctx.arg(1), ptr)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_SP_FREE => {
                ctx.mem.heap_free(ctx.arg(0));
                Ok(ThunkOutcome::Int(0))
            }
            CB_SP_REALLOC => {
                let ptr = ctx.mem.heap_realloc(ctx.arg(0), ctx.arg(1) as usize)?;
                if ctx.arg(2) != 0 {
                    ctx.mem.write_u64(ctx.arg(2), ptr)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_FILTER_ADD => {
                let id = 0xF100_0001 + self.registered_filters.len() as u64;
                let title = if ctx.arg(1) != 0 {
                    let s_ptr = ctx.mem.read_u64(ctx.arg(1) + 8).unwrap_or(0);
                    if s_ptr != 0 { ctx.mem.read_cstring(s_ptr, 128).unwrap_or_default() } else { String::new() }
                } else {
                    String::new()
                };
                if ctx.arg(3) != 0 {
                    ctx.mem.write_u64(ctx.arg(3), id)?;
                }
                self.registered_filters.push((id, title));
                Ok(ThunkOutcome::Int(0))
            }
            CB_LIVEEFFECT_ADD => {
                let id = 0xEF00_0001 + self.registered_effects.len() as u64;
                let out_slot = if ctx.arg(2) != 0 { ctx.arg(2) } else { ctx.arg(3) };
                if out_slot != 0 {
                    ctx.mem.write_u64(out_slot, id)?;
                }
                self.registered_effects.push((id, String::new()));
                Ok(ThunkOutcome::Int(0))
            }
            CB_MENU_ADD => {
                let id = 0xAE10_0001 + self.registered_menus.len() as u64;
                let out_slot = if ctx.arg(3) != 0 { ctx.arg(3) } else { ctx.arg(2) };
                if out_slot != 0 {
                    ctx.mem.write_u64(out_slot, id)?;
                }
                self.registered_menus.push(id);
                Ok(ThunkOutcome::Int(0))
            }
            CB_ART_FIRST => {
                let first = self.arts.iter().position(|a| a.alive).map_or(0, Self::handle_for);
                if ctx.arg(1) != 0 {
                    ctx.mem.write_u64(ctx.arg(1), first)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_ART_TYPE => {
                if ctx.arg(1) != 0 {
                    ctx.mem.write_i16(ctx.arg(1), 1)?; // kPathArt = 1
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_ART_NEW => {
                let idx = self.arts.len();
                self.arts.push(GuestArtObject { alive: true, subpath: AipSubpath { closed: false, segments: Vec::new() } });
                let out = if ctx.arg(3) != 0 { ctx.arg(3) } else { ctx.arg(2) };
                if out != 0 {
                    ctx.mem.write_u64(out, Self::handle_for(idx))?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_ART_DISPOSE => {
                if let Some(idx) = Self::art_idx(ctx.arg(0))
                    && let Some(a) = self.arts.get_mut(idx)
                {
                    a.alive = false;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_MATCH_SELECTED => {
                let out_matches_ptr = ctx.arg(0);
                let out_count_ptr = ctx.arg(1);
                let alive_handles: Vec<u64> = self.arts.iter().enumerate().filter(|(_, a)| a.alive).map(|(i, _)| Self::handle_for(i)).collect();
                let hdl = ctx.mem.new_handle((alive_handles.len().max(1)) * 8, true)?;
                let arr_ptr = ctx.mem.lock_handle(hdl)?;
                for (i, &h) in alive_handles.iter().enumerate() {
                    ctx.mem.write_u64(arr_ptr + (i as u64) * 8, h)?;
                }
                if out_matches_ptr != 0 {
                    ctx.mem.write_u64(out_matches_ptr, hdl)?;
                }
                if out_count_ptr != 0 {
                    ctx.mem.write_i32(out_count_ptr, alive_handles.len() as i32)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_MD_NEW => {
                let h = ctx.mem.new_handle(ctx.arg(0) as usize, true)?;
                if ctx.arg(1) != 0 {
                    ctx.mem.write_u64(ctx.arg(1), h)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_MD_DISPOSE => {
                ctx.mem.dispose_handle(ctx.arg(0));
                Ok(ThunkOutcome::Int(0))
            }
            CB_MD_SIZE => {
                if ctx.arg(1) != 0 {
                    ctx.mem.write_u64(ctx.arg(1), ctx.mem.handle_size(ctx.arg(0)) as u64)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_MD_RESIZE => {
                ctx.mem.resize_handle(ctx.arg(0), ctx.arg(1) as usize)?;
                Ok(ThunkOutcome::Int(0))
            }
            CB_MD_LOCK => {
                let p = ctx.mem.lock_handle(ctx.arg(0))?;
                if ctx.arg(1) != 0 {
                    ctx.mem.write_u64(ctx.arg(1), p)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_PATH_GET_COUNT..=CB_PATH_SET_CLOSED => self.invoke_path_thunk(thunk_id, ctx),
            other => Err(PluginHostError::Abi(format!("unknown .aip PICA callback {other}"))),
        }
    }
}

impl AipRuntimeState {
    fn invoke_path_thunk(&mut self, thunk_id: u32, ctx: &mut ThunkCallCtx<'_>) -> Result<ThunkOutcome> {
        match thunk_id {
            CB_PATH_GET_COUNT => {
                let cnt = Self::art_idx(ctx.arg(0)).and_then(|i| self.arts.get(i)).map_or(0, |a| a.subpath.segments.len() as i32);
                if ctx.arg(1) != 0 {
                    // Writing i32 in little-endian satisfies both `int16*` (low 2 bytes) and `int32*` callers!
                    ctx.mem.write_i32(ctx.arg(1), cnt)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_PATH_SET_COUNT => {
                let new_cnt = (ctx.arg(1) as i32).clamp(0, 100_000) as usize;
                if let Some(idx) = Self::art_idx(ctx.arg(0))
                    && let Some(art) = self.arts.get_mut(idx)
                {
                    art.subpath.segments.resize(new_cnt, AipBezierSegment { p: [0.0; 2], h_in: [0.0; 2], h_out: [0.0; 2], corner: true });
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_PATH_GET_SEGS => {
                let start = (ctx.arg(1) as i32).max(0) as usize;
                let count = (ctx.arg(2) as i32).clamp(0, 100_000) as usize;
                let out_buf = ctx.arg(3);
                if let Some(idx) = Self::art_idx(ctx.arg(0))
                    && let Some(art) = self.arts.get(idx)
                    && out_buf != 0
                {
                    for i in 0..count {
                        let Some(seg) = art.subpath.segments.get(start + i) else { break };
                        write_ai_segment(ctx.mem, out_buf + (i as u64) * PATH_SEG_BYTES, seg)?;
                    }
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_PATH_SET_SEGS => {
                let start = (ctx.arg(1) as i32).max(0) as usize;
                let count = (ctx.arg(2) as i32).clamp(0, 100_000) as usize;
                let in_buf = ctx.arg(3);
                if let Some(idx) = Self::art_idx(ctx.arg(0))
                    && let Some(art) = self.arts.get_mut(idx)
                    && in_buf != 0
                {
                    if start + count > art.subpath.segments.len() {
                        art.subpath.segments.resize(start + count, AipBezierSegment { p: [0.0; 2], h_in: [0.0; 2], h_out: [0.0; 2], corner: true });
                    }
                    for i in 0..count {
                        art.subpath.segments[start + i] = read_ai_segment(ctx.mem, in_buf + (i as u64) * PATH_SEG_BYTES)?;
                    }
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_PATH_INSERT_SEGS => {
                let at = (ctx.arg(1) as i32).max(0) as usize;
                let count = (ctx.arg(2) as i32).clamp(0, 100_000) as usize;
                let in_buf = ctx.arg(3);
                if let Some(idx) = Self::art_idx(ctx.arg(0))
                    && let Some(art) = self.arts.get_mut(idx)
                    && in_buf != 0
                {
                    let mut inserted = Vec::with_capacity(count);
                    for i in 0..count {
                        inserted.push(read_ai_segment(ctx.mem, in_buf + (i as u64) * PATH_SEG_BYTES)?);
                    }
                    let pos = at.min(art.subpath.segments.len());
                    art.subpath.segments.splice(pos..pos, inserted);
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_PATH_REMOVE_SEGS => {
                let at = (ctx.arg(1) as i32).max(0) as usize;
                let count = (ctx.arg(2) as i32).max(0) as usize;
                if let Some(idx) = Self::art_idx(ctx.arg(0))
                    && let Some(art) = self.arts.get_mut(idx)
                    && at < art.subpath.segments.len()
                {
                    let end = (at + count).min(art.subpath.segments.len());
                    art.subpath.segments.drain(at..end);
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_PATH_GET_CLOSED => {
                let closed = Self::art_idx(ctx.arg(0)).and_then(|i| self.arts.get(i)).is_some_and(|a| a.subpath.closed);
                if ctx.arg(1) != 0 {
                    ctx.mem.write_i32(ctx.arg(1), i32::from(closed))?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_PATH_SET_CLOSED => {
                if let Some(idx) = Self::art_idx(ctx.arg(0))
                    && let Some(art) = self.arts.get_mut(idx)
                {
                    art.subpath.closed = (ctx.arg(1) & 0xFF) != 0;
                }
                Ok(ThunkOutcome::Int(0))
            }
            other => Err(PluginHostError::Abi(format!("unknown .aip PICA callback {other}"))),
        }
    }
}

fn write_ai_segment(mem: &mut PagedMemory, addr: u64, seg: &AipBezierSegment) -> Result<()> {
    mem.write_f64(addr, seg.p[0])?;
    mem.write_f64(addr + 0x08, seg.p[1])?;
    mem.write_f64(addr + 0x10, seg.h_in[0])?;
    mem.write_f64(addr + 0x18, seg.h_in[1])?;
    mem.write_f64(addr + 0x20, seg.h_out[0])?;
    mem.write_f64(addr + 0x28, seg.h_out[1])?;
    mem.write_i32(addr + 0x30, i32::from(seg.corner))?;
    Ok(())
}

fn read_ai_segment(mem: &PagedMemory, addr: u64) -> Result<AipBezierSegment> {
    let clean = |v: f64| if v.is_finite() { v.clamp(-4.0e6, 4.0e6) } else { 0.0 };
    Ok(AipBezierSegment {
        p: [clean(mem.read_f64(addr)?), clean(mem.read_f64(addr + 0x08)?)],
        h_in: [clean(mem.read_f64(addr + 0x10)?), clean(mem.read_f64(addr + 0x18)?)],
        h_out: [clean(mem.read_f64(addr + 0x20)?), clean(mem.read_f64(addr + 0x28)?)],
        corner: mem.read_i32(addr + 0x30)? != 0,
    })
}

/// Cross-platform Adobe Illustrator `.aip` SweetPea / PICA C++ binary host.
#[derive(Clone)]
pub struct AipHost {
    binary: Vec<u8>,
    descriptor: PluginDescriptor,
    limits: Limits,
}

impl AipHost {
    pub fn from_bytes(bytes: Vec<u8>, _file_hint: Option<&str>, limits: Limits) -> Result<Self> {
        if bytes.len() > limits.max_binary_bytes {
            return Err(PluginHostError::Limit(format!("binary size {} exceeds limit", bytes.len())));
        }
        let mut mem = PagedMemory::new(limits.max_memory_bytes);
        let (info, rsrc_blobs) = load_any_image(&bytes, &mut mem)?;
        let pipl = pipl::parse_pipl_from_blobs(&rsrc_blobs, &bytes);
        let (ep_name, ep_addr) = resolve_plugin_entrypoint(&info, &pipl, PluginFormat::IllustratorAip)?;
        let descriptor = PluginDescriptor {
            plugin_format: PluginFormat::IllustratorAip,
            binary_format: info.format,
            arch: info.arch,
            guest_os: info.os,
            entrypoint_symbol: ep_name,
            entrypoint_addr: ep_addr,
            pipl,
            exported_symbols: info.exports.keys().cloned().collect(),
        };
        Ok(Self { binary: bytes, descriptor, limits })
    }

    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// Executes the `.aip` C++ vector filter or live effect over `subpaths` and returns the
    /// transformed Bezier subpaths!
    pub fn execute_paths(&self, subpaths: &[AipSubpath], param_scalar: f64) -> Result<Vec<AipSubpath>> {
        if subpaths.is_empty() {
            return Ok(Vec::new());
        }
        let mut mem = PagedMemory::new(self.limits.max_memory_bytes);
        let (info, _) = load_any_image(&self.binary, &mut mem)?;
        let (_, ep_addr) = resolve_plugin_entrypoint(&info, &self.descriptor.pipl, PluginFormat::IllustratorAip)?;

        let mut vm = GuestVm::new(info.arch, info.os, mem, self.limits);
        vm.bind_imports(&info.imports)?;
        vm.run_dll_attach(info.image_base, info.dll_entry)?;

        let sp_basic = build_sp_basic_suite(&mut vm)?;
        let msg_buf = vm.mem.heap_alloc(128, true)?;
        let param_blk = vm.mem.heap_alloc(64, true)?;
        vm.mem.write_f64(param_blk, param_scalar)?;

        let mut state = AipRuntimeState::new(subpaths);

        // 1. Startup (`caller = "SP Interface"`, `selector = "Startup"`)
        write_sp_message_header(&mut vm.mem, msg_buf, 0, sp_basic)?;
        let c_sp_if = vm.mem.alloc_cstring("SP Interface")?;
        let s_startup = vm.mem.alloc_cstring("Startup")?;
        let _ = vm.call_guest(ep_addr, &[c_sp_if, s_startup, msg_buf], &mut state)?;
        let globals = vm.mem.read_u64(msg_buf + 0x10)?;

        // 2. Determine registered dispatch caller/selector (`AI Filter`, `AI Live Effect`, or `AI Menu`)
        let (caller_str, sel_str, handle_id) = if let Some(&(fid, _)) = state.registered_filters.first() {
            ("AI Filter", "Go", fid)
        } else if let Some(&(eid, _)) = state.registered_effects.first() {
            ("AI Live Effect", "Go Live Effect", eid)
        } else if let Some(&mid) = state.registered_menus.first() {
            ("AI Menu", "Go", mid)
        } else {
            ("AI Filter", "Go", 0xF100_0001)
        };

        // `AIFilterMessage` / `AILiveEffectGoMessage`:
        // `+0x00..+0x20`: `SPMessageData`
        // `+0x20`: `filter` / `effect` handle (`u64`)
        // `+0x28`: `parameters` (`void*`)
        // `+0x30`: `art` (`AIArtHandle`, first active path for Live Effect `Go`)
        write_sp_message_header(&mut vm.mem, msg_buf, globals, sp_basic)?;
        vm.mem.write_u64(msg_buf + 0x20, handle_id)?;
        vm.mem.write_u64(msg_buf + 0x28, param_blk)?;
        vm.mem.write_u64(msg_buf + 0x30, AipRuntimeState::handle_for(0))?;

        let c_go_caller = vm.mem.alloc_cstring(caller_str)?;
        let s_go = vm.mem.alloc_cstring(sel_str)?;
        vm.reset_fuel(self.limits.instruction_budget);
        let err = vm.call_guest(ep_addr, &[c_go_caller, s_go, msg_buf], &mut state)? as i32;
        if err != 0 && err != -1 {
            // Note: classic Adobe error codes like `'!sel'` are non-zero if selector differs;
            // only surface error if no paths were touched and an error was returned.
            let unchanged = state.arts.iter().filter(|a| a.alive).map(|a| &a.subpath).eq(subpaths.iter());
            if unchanged {
                return Err(PluginHostError::PluginError(err));
            }
        }

        // 3. Shutdown (`caller = "SP Interface"`, `selector = "Shutdown"`)
        let s_shutdown = vm.mem.alloc_cstring("Shutdown")?;
        write_sp_message_header(&mut vm.mem, msg_buf, globals, sp_basic)?;
        vm.mem.write_u64(msg_buf + 0x20, 0)?;
        vm.mem.write_u64(msg_buf + 0x28, 0)?;
        vm.mem.write_u64(msg_buf + 0x30, 0)?;
        let _ = vm.call_guest(ep_addr, &[c_sp_if, s_shutdown, msg_buf], &mut state);

        Ok(state.arts.into_iter().filter(|a| a.alive).map(|a| a.subpath).collect())
    }
}

fn build_sp_basic_suite(vm: &mut GuestVm) -> Result<u64> {
    let s = vm.mem.heap_alloc(64, true)?;
    let acq = vm.register_host_callback(CB_SP_ACQUIRE);
    let rel = vm.register_host_callback(CB_SP_RELEASE);
    let eq = vm.register_host_callback(CB_SP_ISEQUAL);
    let al = vm.register_host_callback(CB_SP_ALLOC);
    let fr = vm.register_host_callback(CB_SP_FREE);
    let ra = vm.register_host_callback(CB_SP_REALLOC);
    let un = vm.register_host_callback(CB_NOOP_OK);
    vm.mem.write_u64(s, acq)?;
    vm.mem.write_u64(s + 0x08, rel)?;
    vm.mem.write_u64(s + 0x10, eq)?;
    vm.mem.write_u64(s + 0x18, al)?;
    vm.mem.write_u64(s + 0x20, fr)?;
    vm.mem.write_u64(s + 0x28, ra)?;
    vm.mem.write_u64(s + 0x30, un)?;
    Ok(s)
}

fn write_sp_message_header(mem: &mut PagedMemory, msg: u64, globals: u64, sp_basic: u64) -> Result<()> {
    mem.write_i32(msg, SP_CHECK_MAGIC)?;
    mem.write_u64(msg + 0x08, 0xA100_0001)?;
    mem.write_u64(msg + 0x10, globals)?;
    mem.write_u64(msg + 0x18, sp_basic)?;
    Ok(())
}
