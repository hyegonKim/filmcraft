//! Adobe Photoshop `.8bf` (Windows PE32+ & macOS `.plugin` Mach-O) C++ Filter Host SDK runtime.
//!
//! Marshals `FilterRecord` (supporting both Windows `#pragma pack(4)` and macOS LP64 layouts),
//! `BigDocumentStruct`, `BufferProcs`, `HandleProcs`, `abortProc`, `progressProc`, and
//! `advanceState` across `Parameters` → `Prepare` → `Start` → `Continue` → `Finish`.

use serde::{Deserialize, Serialize};

use super::crt::{GuestVm, HostCallbackHandler, ThunkCallCtx, ThunkOutcome};
use super::mem::PagedMemory;
use super::{GuestOs, Limits, PluginDescriptor, PluginFormat, PluginHostError, Result, load_any_image, pipl, resolve_plugin_entrypoint};

const SELECTOR_PARAMETERS: u64 = 1;
const SELECTOR_PREPARE: u64 = 2;
const SELECTOR_START: u64 = 3;
const SELECTOR_CONTINUE: u64 = 4;
const SELECTOR_FINISH: u64 = 5;

const CB_ABORT: u32 = 1;
const CB_PROGRESS: u32 = 2;
const CB_HOST_PROC: u32 = 3;
const CB_ADVANCE_STATE: u32 = 4;
const CB_BUF_ALLOC: u32 = 10;
const CB_BUF_LOCK: u32 = 11;
const CB_BUF_UNLOCK: u32 = 12;
const CB_BUF_FREE: u32 = 13;
const CB_BUF_SPACE: u32 = 14;
const CB_HDL_NEW: u32 = 20;
const CB_HDL_DISPOSE: u32 = 21;
const CB_HDL_GET_SIZE: u32 = 22;
const CB_HDL_SET_SIZE: u32 = 23;
const CB_HDL_LOCK: u32 = 24;
const CB_HDL_UNLOCK: u32 = 25;
const CB_HDL_SPACE: u32 = 26;

/// Bit depth of pixels handed to a Photoshop `.8bf` C++ filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelDepth8bf {
    U8,
    U16,
    F32,
}

impl PixelDepth8bf {
    pub fn bits(self) -> i32 {
        match self {
            Self::U8 => 8,
            Self::U16 => 16,
            Self::F32 => 32,
        }
    }

    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::F32 => 4,
        }
    }
}

/// Host image buffer passed into [`Filter8bfHost::execute`] as normalized `[0.0, 1.0]` interleaved `f32`.
#[derive(Clone, Debug)]
pub struct ImageBuffer8bf {
    pub width: u32,
    pub height: u32,
    pub channels: usize,
    pub depth: PixelDepth8bf,
    pub samples: Vec<f32>,
}

impl ImageBuffer8bf {
    pub fn from_rgba_f32(width: u32, height: u32, samples: Vec<f32>, depth: PixelDepth8bf) -> Result<Self> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| PluginHostError::InvalidBinary("image dimensions overflow".into()))?;
        if samples.len() != expected {
            return Err(PluginHostError::Abi(format!("expected {expected} RGBA samples, got {}", samples.len())));
        }
        Ok(Self { width, height, channels: 4, depth, samples })
    }
}

#[derive(Clone, Copy)]
struct FilterRecordLayout {
    abort_proc: u64,
    progress_proc: u64,
    parameters: u64,
    image_size: u64,
    planes: u64,
    filter_rect: u64,
    max_space: u64,
    buffer_space: u64,
    in_rect: u64,
    in_lo_plane: u64,
    in_hi_plane: u64,
    out_rect: u64,
    out_lo_plane: u64,
    out_hi_plane: u64,
    in_data: u64,
    in_row_bytes: u64,
    out_data: u64,
    out_row_bytes: u64,
    have_mask: u64,
    host_sig: u64,
    host_proc: u64,
    image_mode: u64,
    image_h_res: u64,
    image_v_res: u64,
    buffer_procs: u64,
    handle_procs: u64,
    tile_height: u64,
    tile_width: u64,
    advance_state: u64,
    in_col_bytes: u64,
    out_col_bytes: u64,
    in_layer_planes: u64,
    in_trans_mask: u64,
    out_layer_planes: u64,
    out_trans_mask: u64,
    depth: u64,
    big_doc_data: u64,
    total_size: usize,
}

impl FilterRecordLayout {
    /// Verified byte-for-byte against Photoshop 2027 `Solarize.8bf` & `Average.8bf` (`#pragma pack(4)`).
    const WIN64: Self = Self {
        abort_proc: 0x04,
        progress_proc: 0x0C,
        parameters: 0x14,
        image_size: 0x1C,
        planes: 0x20,
        filter_rect: 0x22,
        max_space: 0x38,
        buffer_space: 0x3C,
        in_rect: 0x40,
        in_lo_plane: 0x48,
        in_hi_plane: 0x4A,
        out_rect: 0x4C,
        out_lo_plane: 0x54,
        out_hi_plane: 0x56,
        in_data: 0x58,
        in_row_bytes: 0x60,
        out_data: 0x64,
        out_row_bytes: 0x6C,
        have_mask: 0x71,
        host_sig: 0x90,
        host_proc: 0x94,
        image_mode: 0x9C,
        image_h_res: 0xA0,
        image_v_res: 0xA4,
        buffer_procs: 0xD4,
        handle_procs: 0xF4,
        tile_height: 0x114,
        tile_width: 0x116,
        advance_state: 0x118,
        in_col_bytes: 0x12E,
        out_col_bytes: 0x130,
        in_layer_planes: 0x148,
        in_trans_mask: 0x14A,
        out_layer_planes: 0x152,
        out_trans_mask: 0x154,
        depth: 0x1E0,
        big_doc_data: 0x1F8,
        total_size: 0x280,
    };

    /// Natural LP64 layout used by macOS 64-bit `.plugin` bundles (`arm64` & `x86_64`).
    const MAC64: Self = Self {
        abort_proc: 0x08,
        progress_proc: 0x10,
        parameters: 0x18,
        image_size: 0x20,
        planes: 0x24,
        filter_rect: 0x26,
        max_space: 0x3C,
        buffer_space: 0x40,
        in_rect: 0x44,
        in_lo_plane: 0x4C,
        in_hi_plane: 0x4E,
        out_rect: 0x50,
        out_lo_plane: 0x58,
        out_hi_plane: 0x5A,
        in_data: 0x60,
        in_row_bytes: 0x68,
        out_data: 0x70,
        out_row_bytes: 0x78,
        have_mask: 0x7D,
        host_sig: 0xA0,
        host_proc: 0xA8,
        image_mode: 0xB0,
        image_h_res: 0xB4,
        image_v_res: 0xB8,
        buffer_procs: 0xE8,
        handle_procs: 0x108,
        tile_height: 0x128,
        tile_width: 0x12A,
        advance_state: 0x130,
        in_col_bytes: 0x146,
        out_col_bytes: 0x148,
        in_layer_planes: 0x160,
        in_trans_mask: 0x162,
        out_layer_planes: 0x16A,
        out_trans_mask: 0x16C,
        depth: 0x200,
        big_doc_data: 0x218,
        total_size: 0x2A0,
    };

    fn for_os(os: GuestOs) -> Self {
        match os {
            GuestOs::Windows => Self::WIN64,
            GuestOs::MacOs => Self::MAC64,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct TileRect {
    top: i32,
    left: i32,
    bottom: i32,
    right: i32,
}

impl TileRect {
    fn width(self) -> usize {
        (self.right - self.left).max(0) as usize
    }
    fn height(self) -> usize {
        (self.bottom - self.top).max(0) as usize
    }
    fn is_empty(self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }
}

struct ActiveFilterState<'a> {
    src: &'a ImageBuffer8bf,
    dst_samples: Vec<f32>,
    layout: FilterRecordLayout,
    filter_record_ptr: u64,
    big_doc_ptr: u64,
    guest_in_buf: u64,
    guest_out_buf: u64,
    guest_buf_cap: usize,
    active_out_rect: Option<TileRect>,
    active_out_lo: usize,
    active_out_hi: usize,
    active_out_row_bytes: usize,
    advance_calls: usize,
}

impl ActiveFilterState<'_> {
    fn read_requested_rects(&self, mem: &PagedMemory) -> Result<(TileRect, TileRect, usize, usize, usize, usize)> {
        let use_32 = mem.read_i32(self.big_doc_ptr)? != 0;
        let (in_r, out_r) = if use_32 {
            let ir = TileRect {
                top: mem.read_i32(self.big_doc_ptr + 0x1C)?,
                left: mem.read_i32(self.big_doc_ptr + 0x20)?,
                bottom: mem.read_i32(self.big_doc_ptr + 0x24)?,
                right: mem.read_i32(self.big_doc_ptr + 0x28)?,
            };
            let or = TileRect {
                top: mem.read_i32(self.big_doc_ptr + 0x2C)?,
                left: mem.read_i32(self.big_doc_ptr + 0x30)?,
                bottom: mem.read_i32(self.big_doc_ptr + 0x34)?,
                right: mem.read_i32(self.big_doc_ptr + 0x38)?,
            };
            (ir, or)
        } else {
            let ir = TileRect {
                top: i32::from(mem.read_i16(self.filter_record_ptr + self.layout.in_rect)?),
                left: i32::from(mem.read_i16(self.filter_record_ptr + self.layout.in_rect + 2)?),
                bottom: i32::from(mem.read_i16(self.filter_record_ptr + self.layout.in_rect + 4)?),
                right: i32::from(mem.read_i16(self.filter_record_ptr + self.layout.in_rect + 6)?),
            };
            let or = TileRect {
                top: i32::from(mem.read_i16(self.filter_record_ptr + self.layout.out_rect)?),
                left: i32::from(mem.read_i16(self.filter_record_ptr + self.layout.out_rect + 2)?),
                bottom: i32::from(mem.read_i16(self.filter_record_ptr + self.layout.out_rect + 4)?),
                right: i32::from(mem.read_i16(self.filter_record_ptr + self.layout.out_rect + 6)?),
            };
            (ir, or)
        };
        let max_ch = self.src.channels.saturating_sub(1);
        let in_lo = (mem.read_i16(self.filter_record_ptr + self.layout.in_lo_plane)?.max(0) as usize).min(max_ch);
        let in_hi = (mem.read_i16(self.filter_record_ptr + self.layout.in_hi_plane)?.max(0) as usize).min(max_ch);
        let out_lo = (mem.read_i16(self.filter_record_ptr + self.layout.out_lo_plane)?.max(0) as usize).min(max_ch);
        let out_hi = (mem.read_i16(self.filter_record_ptr + self.layout.out_hi_plane)?.max(0) as usize).min(max_ch);
        Ok((in_r, out_r, in_lo, in_hi, out_lo, out_hi))
    }

    fn flush_active_out(&mut self, mem: &PagedMemory) -> Result<()> {
        let Some(rect) = self.active_out_rect.take() else { return Ok(()) };
        if rect.is_empty() || self.active_out_hi < self.active_out_lo {
            return Ok(());
        }
        let planes = self.active_out_hi - self.active_out_lo + 1;
        let bps = self.src.depth.bytes_per_sample();
        let w = rect.width();
        let h = rect.height();
        let total_bytes = self.active_out_row_bytes.saturating_mul(h);
        let raw = mem.read_vec(self.guest_out_buf, total_bytes)?;
        let img_w = self.src.width as i32;
        let img_h = self.src.height as i32;

        for row in 0..h {
            let py = rect.top + row as i32;
            if py < 0 || py >= img_h {
                continue;
            }
            let row_base = row * self.active_out_row_bytes;
            for col in 0..w {
                let px = rect.left + col as i32;
                if px < 0 || px >= img_w {
                    continue;
                }
                let dst_px = ((py as usize) * (img_w as usize) + (px as usize)) * self.src.channels;
                for p in 0..planes {
                    let ch = self.active_out_lo + p;
                    let off = row_base + (col * planes + p) * bps;
                    let val = decode_ps_sample(&raw, off, self.src.depth);
                    if let Some(slot) = self.dst_samples.get_mut(dst_px + ch) {
                        *slot = val;
                    }
                }
            }
        }
        Ok(())
    }

    fn stage_rect(&self, mem: &mut PagedMemory, guest_ptr: u64, rect: TileRect, lo: usize, hi: usize) -> Result<usize> {
        if rect.is_empty() || hi < lo {
            return Ok(0);
        }
        let planes = hi - lo + 1;
        let bps = self.src.depth.bytes_per_sample();
        let w = rect.width();
        let h = rect.height();
        let row_bytes = w * planes * bps;
        let total = row_bytes.checked_mul(h).ok_or_else(|| PluginHostError::Limit("tile size overflow".into()))?;
        if total > self.guest_buf_cap {
            return Err(PluginHostError::Limit(format!("requested tile {total} B exceeds staging buffer")));
        }
        let mut staging = vec![0u8; total];
        let img_w = self.src.width as i32;
        let img_h = self.src.height as i32;
        for row in 0..h {
            let sy = (rect.top + row as i32).clamp(0, img_h - 1) as usize;
            let row_base = row * row_bytes;
            for col in 0..w {
                let sx = (rect.left + col as i32).clamp(0, img_w - 1) as usize;
                let src_px = (sy * (self.src.width as usize) + sx) * self.src.channels;
                for p in 0..planes {
                    let sample = self.dst_samples.get(src_px + lo + p).copied().unwrap_or(0.0);
                    let off = row_base + (col * planes + p) * bps;
                    encode_ps_sample(&mut staging, off, sample, self.src.depth);
                }
            }
        }
        mem.write_bytes(guest_ptr, &staging)?;
        Ok(row_bytes)
    }

    fn advance_state(&mut self, mem: &mut PagedMemory) -> Result<i16> {
        self.advance_calls += 1;
        if self.advance_calls > 65536 {
            return Err(PluginHostError::Limit("excessive advanceState tile iterations".into()));
        }
        self.flush_active_out(mem)?;
        let (in_r, out_r, in_lo, in_hi, out_lo, out_hi) = self.read_requested_rects(mem)?;
        let bps = self.src.depth.bytes_per_sample() as i16;
        if !in_r.is_empty() && in_hi >= in_lo {
            let in_rb = self.stage_rect(mem, self.guest_in_buf, in_r, in_lo, in_hi)?;
            mem.write_u64(self.filter_record_ptr + self.layout.in_data, self.guest_in_buf)?;
            mem.write_i32(self.filter_record_ptr + self.layout.in_row_bytes, in_rb as i32)?;
            let in_planes = (in_hi - in_lo + 1) as i16;
            mem.write_i16(self.filter_record_ptr + self.layout.in_col_bytes, in_planes * bps)?;
        } else {
            mem.write_u64(self.filter_record_ptr + self.layout.in_data, 0)?;
            mem.write_i32(self.filter_record_ptr + self.layout.in_row_bytes, 0)?;
        }

        if !out_r.is_empty() && out_hi >= out_lo {
            // Photoshop pre-populates `outData` with the current layer pixels (filters like `Solarize.8bf`
            // modify `outData` in place without even reading `inData`!).
            let out_rb = self.stage_rect(mem, self.guest_out_buf, out_r, out_lo, out_hi)?;
            mem.write_u64(self.filter_record_ptr + self.layout.out_data, self.guest_out_buf)?;
            mem.write_i32(self.filter_record_ptr + self.layout.out_row_bytes, out_rb as i32)?;
            let out_planes = (out_hi - out_lo + 1) as i16;
            mem.write_i16(self.filter_record_ptr + self.layout.out_col_bytes, out_planes * bps)?;
            self.active_out_rect = Some(out_r);
            self.active_out_lo = out_lo;
            self.active_out_hi = out_hi;
            self.active_out_row_bytes = out_rb;
        } else {
            mem.write_u64(self.filter_record_ptr + self.layout.out_data, 0)?;
            mem.write_i32(self.filter_record_ptr + self.layout.out_row_bytes, 0)?;
        }
        Ok(0)
    }
}

impl HostCallbackHandler for ActiveFilterState<'_> {
    fn invoke_host_thunk(&mut self, thunk_id: u32, ctx: &mut ThunkCallCtx<'_>, _vm: &mut GuestVm) -> Result<ThunkOutcome> {
        match thunk_id {
            CB_ABORT => Ok(ThunkOutcome::Int(0)),
            CB_PROGRESS | CB_HOST_PROC => Ok(ThunkOutcome::Int(0)),
            CB_ADVANCE_STATE => {
                let code = self.advance_state(ctx.mem)?;
                Ok(ThunkOutcome::Int(code as i64 as u64))
            }
            CB_BUF_ALLOC => {
                let size = ctx.arg(0) as usize;
                let buf_id_out = ctx.arg(1);
                let ptr = ctx.mem.heap_alloc(size, true)?;
                if buf_id_out != 0 {
                    ctx.mem.write_u64(buf_id_out, ptr)?;
                }
                Ok(ThunkOutcome::Int(0))
            }
            CB_BUF_LOCK => Ok(ThunkOutcome::Int(ctx.arg(0))),
            CB_BUF_UNLOCK | CB_BUF_FREE => Ok(ThunkOutcome::Int(0)),
            CB_BUF_SPACE | CB_HDL_SPACE => Ok(ThunkOutcome::Int(64 << 20)),
            CB_HDL_NEW => Ok(ThunkOutcome::Int(ctx.mem.new_handle(ctx.arg(0) as usize, true)?)),
            CB_HDL_DISPOSE => {
                ctx.mem.dispose_handle(ctx.arg(0));
                Ok(ThunkOutcome::Int(0))
            }
            CB_HDL_GET_SIZE => Ok(ThunkOutcome::Int(ctx.mem.handle_size(ctx.arg(0)) as u64)),
            CB_HDL_SET_SIZE => {
                ctx.mem.resize_handle(ctx.arg(0), ctx.arg(1) as usize)?;
                Ok(ThunkOutcome::Int(0))
            }
            CB_HDL_LOCK => Ok(ThunkOutcome::Int(ctx.mem.lock_handle(ctx.arg(0))?)),
            CB_HDL_UNLOCK => {
                ctx.mem.unlock_handle(ctx.arg(0));
                Ok(ThunkOutcome::Int(0))
            }
            other => Err(PluginHostError::Abi(format!("unknown .8bf callback {other}"))),
        }
    }
}

/// Loaded, validated cross-platform Photoshop `.8bf` / `.plugin` binary filter host.
#[derive(Clone)]
pub struct Filter8bfHost {
    binary: Vec<u8>,
    descriptor: PluginDescriptor,
    limits: Limits,
}

impl Filter8bfHost {
    pub fn from_bytes(bytes: Vec<u8>, _file_hint: Option<&str>, limits: Limits) -> Result<Self> {
        if bytes.len() > limits.max_binary_bytes {
            return Err(PluginHostError::Limit(format!("binary size {} exceeds limit", bytes.len())));
        }
        let mut mem = PagedMemory::new(limits.max_memory_bytes);
        let (info, rsrc_blobs) = load_any_image(&bytes, &mut mem)?;
        let pipl = pipl::parse_pipl_from_blobs(&rsrc_blobs, &bytes);
        let (ep_name, ep_addr) = resolve_plugin_entrypoint(&info, &pipl, PluginFormat::Photoshop8bf)?;
        let descriptor = PluginDescriptor {
            plugin_format: PluginFormat::Photoshop8bf,
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

    /// Executes the `.8bf` C++ filter on `input` and returns the filtered RGBA/channel buffer.
    pub fn execute(&self, input: &ImageBuffer8bf) -> Result<ImageBuffer8bf> {
        if input.width == 0 || input.height == 0 {
            return Ok(input.clone());
        }
        let mut mem = PagedMemory::new(self.limits.max_memory_bytes);
        let (info, _) = load_any_image(&self.binary, &mut mem)?;
        let (_, ep_addr) = resolve_plugin_entrypoint(&info, &self.descriptor.pipl, PluginFormat::Photoshop8bf)?;

        let mut vm = GuestVm::new(info.arch, info.os, mem, self.limits);
        vm.bind_imports(&info.imports)?;
        vm.run_dll_attach(info.image_base, info.dll_entry)?;

        let layout = FilterRecordLayout::for_os(info.os);
        let filter_rec = vm.mem.heap_alloc(layout.total_size, true)?;
        let big_doc = vm.mem.heap_alloc(0x60, true)?;
        let data_slot = vm.mem.heap_alloc(8, true)?;
        let result_slot = vm.mem.heap_alloc(4, true)?;
        let buf_procs = build_buffer_procs(&mut vm)?;
        let hdl_procs = build_handle_procs(&mut vm)?;

        let full_bytes = (input.width as usize)
            .saturating_mul(input.height as usize)
            .saturating_mul(input.channels)
            .saturating_mul(input.depth.bytes_per_sample())
            .max(256 * 256 * 16);
        let guest_in_buf = vm.mem.heap_alloc(full_bytes, true)?;
        let guest_out_buf = vm.mem.heap_alloc(full_bytes, true)?;

        populate_filter_record(&mut vm, filter_rec, big_doc, buf_procs, hdl_procs, layout, input)?;

        let mut state = ActiveFilterState {
            src: input,
            dst_samples: input.samples.clone(),
            layout,
            filter_record_ptr: filter_rec,
            big_doc_ptr: big_doc,
            guest_in_buf,
            guest_out_buf,
            guest_buf_cap: full_bytes,
            active_out_rect: None,
            active_out_lo: 0,
            active_out_hi: input.channels.saturating_sub(1),
            active_out_row_bytes: 0,
            advance_calls: 0,
        };

        // Pre-stage full image for single-shot filters that process directly inside `Start` or `Continue`
        // without invoking `advanceState()`.
        let full_rect = TileRect { top: 0, left: 0, bottom: input.height as i32, right: input.width as i32 };
        let hi_ch = input.channels.saturating_sub(1);
        let rb = state.stage_rect(&mut vm.mem, guest_in_buf, full_rect, 0, hi_ch)?;
        let _ = state.stage_rect(&mut vm.mem, guest_out_buf, full_rect, 0, hi_ch)?;
        vm.mem.write_i16(filter_rec + layout.in_rect + 4, (input.height.min(32767)) as i16)?;
        vm.mem.write_i16(filter_rec + layout.in_rect + 6, (input.width.min(32767)) as i16)?;
        vm.mem.write_i16(filter_rec + layout.out_rect + 4, (input.height.min(32767)) as i16)?;
        vm.mem.write_i16(filter_rec + layout.out_rect + 6, (input.width.min(32767)) as i16)?;
        vm.mem.write_i16(filter_rec + layout.in_hi_plane, hi_ch as i16)?;
        vm.mem.write_i16(filter_rec + layout.out_hi_plane, hi_ch as i16)?;
        vm.mem.write_u64(filter_rec + layout.in_data, guest_in_buf)?;
        vm.mem.write_i32(filter_rec + layout.in_row_bytes, rb as i32)?;
        vm.mem.write_u64(filter_rec + layout.out_data, guest_out_buf)?;
        vm.mem.write_i32(filter_rec + layout.out_row_bytes, rb as i32)?;
        state.active_out_rect = Some(full_rect);
        state.active_out_row_bytes = rb;

        // Run Photoshop selector sequence: Parameters(1) -> Prepare(2) -> Start(3) -> Continue(4) loop -> Finish(5)
        for selector in [SELECTOR_PARAMETERS, SELECTOR_PREPARE, SELECTOR_START] {
            vm.mem.write_i16(result_slot, 0)?;
            vm.reset_fuel(self.limits.instruction_budget);
            let _ = vm.call_guest(ep_addr, &[selector, filter_rec, data_slot, result_slot], &mut state)?;
            let code = vm.mem.read_i16(result_slot)?;
            // `Parameters` may return userCanceledErr or unsupported if non-UI, ignore only on Parameters;
            // on Prepare/Start any negative error aborts.
            if selector != SELECTOR_PARAMETERS && code < 0 {
                return Err(PluginHostError::PluginError(i32::from(code)));
            }
        }

        // Continue loop while `inRect` or `outRect` remains non-empty (up to 4096 tiles)
        for _ in 0..4096 {
            let (in_r, out_r, _, _, _, _) = state.read_requested_rects(&vm.mem)?;
            if in_r.is_empty() && out_r.is_empty() {
                break;
            }
            if state.advance_calls == 0 {
                let _ = state.advance_state(&mut vm.mem)?;
            }
            vm.mem.write_i16(result_slot, 0)?;
            vm.reset_fuel(self.limits.instruction_budget);
            let _ = vm.call_guest(ep_addr, &[SELECTOR_CONTINUE, filter_rec, data_slot, result_slot], &mut state)?;
            let code = vm.mem.read_i16(result_slot)?;
            if code != 0 {
                if code < 0 {
                    return Err(PluginHostError::PluginError(i32::from(code)));
                }
                break;
            }
            let (_, after_out, _, _, _, _) = state.read_requested_rects(&vm.mem)?;
            if after_out.is_empty() {
                break;
            }
        }

        state.flush_active_out(&vm.mem)?;

        vm.mem.write_i16(result_slot, 0)?;
        vm.reset_fuel(self.limits.instruction_budget);
        let _ = vm.call_guest(ep_addr, &[SELECTOR_FINISH, filter_rec, data_slot, result_slot], &mut state)?;

        Ok(ImageBuffer8bf { width: input.width, height: input.height, channels: input.channels, depth: input.depth, samples: state.dst_samples })
    }
}

fn build_buffer_procs(vm: &mut GuestVm) -> Result<u64> {
    let p = vm.mem.heap_alloc(64, true)?;
    let alloc = vm.register_host_callback(CB_BUF_ALLOC);
    let lock = vm.register_host_callback(CB_BUF_LOCK);
    let unlock = vm.register_host_callback(CB_BUF_UNLOCK);
    let free = vm.register_host_callback(CB_BUF_FREE);
    let space = vm.register_host_callback(CB_BUF_SPACE);
    vm.mem.write_i16(p, 4)?;
    vm.mem.write_i16(p + 2, 5)?;
    vm.mem.write_u64(p + 8, alloc)?;
    vm.mem.write_u64(p + 16, lock)?;
    vm.mem.write_u64(p + 24, unlock)?;
    vm.mem.write_u64(p + 32, free)?;
    vm.mem.write_u64(p + 40, space)?;
    Ok(p)
}

fn build_handle_procs(vm: &mut GuestVm) -> Result<u64> {
    let p = vm.mem.heap_alloc(80, true)?;
    let h_new = vm.register_host_callback(CB_HDL_NEW);
    let h_disp = vm.register_host_callback(CB_HDL_DISPOSE);
    let h_gsz = vm.register_host_callback(CB_HDL_GET_SIZE);
    let h_ssz = vm.register_host_callback(CB_HDL_SET_SIZE);
    let h_lock = vm.register_host_callback(CB_HDL_LOCK);
    let h_unlock = vm.register_host_callback(CB_HDL_UNLOCK);
    let h_space = vm.register_host_callback(CB_HDL_SPACE);
    vm.mem.write_i16(p, 1)?;
    vm.mem.write_i16(p + 2, 7)?;
    vm.mem.write_u64(p + 8, h_new)?;
    vm.mem.write_u64(p + 16, h_disp)?;
    vm.mem.write_u64(p + 24, h_gsz)?;
    vm.mem.write_u64(p + 32, h_ssz)?;
    vm.mem.write_u64(p + 40, h_lock)?;
    vm.mem.write_u64(p + 48, h_unlock)?;
    vm.mem.write_u64(p + 56, h_space)?;
    Ok(p)
}

#[allow(clippy::too_many_arguments)]
fn populate_filter_record(
    vm: &mut GuestVm,
    rec: u64,
    big_doc: u64,
    buf_procs: u64,
    hdl_procs: u64,
    layout: FilterRecordLayout,
    input: &ImageBuffer8bf,
) -> Result<()> {
    let abort_fn = vm.register_host_callback(CB_ABORT);
    let prog_fn = vm.register_host_callback(CB_PROGRESS);
    let host_fn = vm.register_host_callback(CB_HOST_PROC);
    let adv_fn = vm.register_host_callback(CB_ADVANCE_STATE);

    let w16 = input.width.min(32767) as i16;
    let h16 = input.height.min(32767) as i16;
    let color_planes = input.channels.min(3) as i16;
    let alpha_planes = input.channels.saturating_sub(3) as i16;

    vm.mem.write_u64(rec + layout.abort_proc, abort_fn)?;
    vm.mem.write_u64(rec + layout.progress_proc, prog_fn)?;
    vm.mem.write_u64(rec + layout.parameters, 0)?;
    vm.mem.write_i16(rec + layout.image_size, h16)?;
    vm.mem.write_i16(rec + layout.image_size + 2, w16)?;
    vm.mem.write_i16(rec + layout.planes, input.channels as i16)?;
    vm.mem.write_i16(rec + layout.filter_rect, 0)?;
    vm.mem.write_i16(rec + layout.filter_rect + 2, 0)?;
    vm.mem.write_i16(rec + layout.filter_rect + 4, h16)?;
    vm.mem.write_i16(rec + layout.filter_rect + 6, w16)?;
    vm.mem.write_i32(rec + layout.max_space, 64 << 20)?;
    vm.mem.write_i32(rec + layout.buffer_space, 64 << 20)?;
    vm.mem.write_u8(rec + layout.have_mask, 0)?;
    vm.mem.write_u32(rec + layout.host_sig, 0x3842_494D)?; // '8BIM'
    vm.mem.write_u64(rec + layout.host_proc, host_fn)?;
    vm.mem.write_i16(rec + layout.image_mode, if input.channels == 1 { 1 } else { 3 })?; // RGBColor = 3
    vm.mem.write_i32(rec + layout.image_h_res, 72 << 16)?;
    vm.mem.write_i32(rec + layout.image_v_res, 72 << 16)?;
    vm.mem.write_u64(rec + layout.buffer_procs, buf_procs)?;
    vm.mem.write_u64(rec + layout.handle_procs, hdl_procs)?;
    vm.mem.write_i16(rec + layout.tile_height, 256)?;
    vm.mem.write_i16(rec + layout.tile_width, 256)?;
    vm.mem.write_u64(rec + layout.advance_state, adv_fn)?;
    vm.mem.write_i16(rec + layout.in_layer_planes, color_planes)?;
    vm.mem.write_i16(rec + layout.in_trans_mask, alpha_planes)?;
    vm.mem.write_i16(rec + layout.out_layer_planes, color_planes)?;
    vm.mem.write_i16(rec + layout.out_trans_mask, alpha_planes)?;
    vm.mem.write_i32(rec + layout.depth, input.depth.bits())?;
    vm.mem.write_u64(rec + layout.big_doc_data, big_doc)?;

    // BigDocumentStruct (32-bit coordinates)
    vm.mem.write_i32(big_doc, 0)?;
    vm.mem.write_i32(big_doc + 0x04, input.height as i32)?;
    vm.mem.write_i32(big_doc + 0x08, input.width as i32)?;
    vm.mem.write_i32(big_doc + 0x0C, 0)?;
    vm.mem.write_i32(big_doc + 0x10, 0)?;
    vm.mem.write_i32(big_doc + 0x14, input.height as i32)?;
    vm.mem.write_i32(big_doc + 0x18, input.width as i32)?;
    Ok(())
}

fn encode_ps_sample(buf: &mut [u8], off: usize, v: f32, depth: PixelDepth8bf) {
    let clamped = if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
    match depth {
        PixelDepth8bf::U8 => {
            if let Some(slot) = buf.get_mut(off) {
                *slot = (clamped * 255.0 + 0.5) as u8;
            }
        }
        PixelDepth8bf::U16 => {
            // Photoshop 16-bit plug-in range is 0..=32768 (`0x8000` = 1.0)!
            let u = (clamped * 32768.0 + 0.5) as u16;
            if let Some(s) = buf.get_mut(off..off + 2) {
                s.copy_from_slice(&u.to_le_bytes());
            }
        }
        PixelDepth8bf::F32 => {
            let fv = if v.is_finite() { v } else { 0.0 };
            if let Some(s) = buf.get_mut(off..off + 4) {
                s.copy_from_slice(&fv.to_le_bytes());
            }
        }
    }
}

fn decode_ps_sample(buf: &[u8], off: usize, depth: PixelDepth8bf) -> f32 {
    match depth {
        PixelDepth8bf::U8 => buf.get(off).map_or(0.0, |&b| f32::from(b) / 255.0),
        PixelDepth8bf::U16 => buf.get(off..off + 2).map_or(0.0, |s| f32::from(u16::from_le_bytes([s[0], s[1]])) / 32768.0),
        PixelDepth8bf::F32 => buf.get(off..off + 4).map_or(0.0, |s| f32::from_le_bytes([s[0], s[1], s[2], s[3]])),
    }
}
