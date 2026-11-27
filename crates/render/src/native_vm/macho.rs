//! Portable macOS / iOS Mach-O 64-bit (`arm64` & `x86_64`) and Universal Fat (`0xCAFEBABE`)
//! binary loader, symbol table & stub binder, and bundle resource reader.

use std::collections::BTreeMap;

use super::mem::{DEFAULT_IMAGE_BASE, PagedMemory};
use super::{BinaryFormat, GuestArch, GuestOs, ImportSymbol, LoadedImageInfo, PluginHostError, Result};

const MH_MAGIC_64: u32 = 0xFEED_FACF;
const FAT_MAGIC_BE: u32 = 0xCAFE_BABE;
const FAT_MAGIC_64_BE: u32 = 0xCAFE_BABF;

const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_TYPE_ARM64: u32 = 0x0100_000C;

const LC_SYMTAB: u32 = 0x02;
const LC_DYSYMTAB: u32 = 0x0B;
const LC_SEGMENT_64: u32 = 0x19;
const LC_DYLD_INFO: u32 = 0x22;
const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;

const S_NON_LAZY_SYMBOL_POINTERS: u32 = 0x06;
const S_LAZY_SYMBOL_POINTERS: u32 = 0x07;
const S_SYMBOL_STUBS: u32 = 0x08;

const N_EXT: u8 = 0x01;
const N_TYPE: u8 = 0x0E;
const N_UNDF: u8 = 0x00;
const N_SECT: u8 = 0x0E;

fn u32_le(b: &[u8], off: usize) -> Result<u32> {
    let s = b.get(off..off + 4).ok_or_else(|| PluginHostError::InvalidBinary("truncated Mach-O u32".into()))?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u32_be(b: &[u8], off: usize) -> Result<u32> {
    let s = b.get(off..off + 4).ok_or_else(|| PluginHostError::InvalidBinary("truncated Fat u32".into()))?;
    Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64_le(b: &[u8], off: usize) -> Result<u64> {
    let s = b.get(off..off + 8).ok_or_else(|| PluginHostError::InvalidBinary("truncated Mach-O u64".into()))?;
    Ok(u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}

fn u64_be(b: &[u8], off: usize) -> Result<u64> {
    let s = b.get(off..off + 8).ok_or_else(|| PluginHostError::InvalidBinary("truncated Fat64 u64".into()))?;
    Ok(u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}

pub fn is_macho_or_fat(bytes: &[u8]) -> bool {
    if bytes.len() < 32 {
        return false;
    }
    let Ok(le) = u32_le(bytes, 0) else { return false };
    let Ok(be) = u32_be(bytes, 0) else { return false };
    le == MH_MAGIC_64 || be == FAT_MAGIC_BE || be == FAT_MAGIC_64_BE
}

fn select_macho_slice(bytes: &[u8]) -> Result<&[u8]> {
    let be = u32_be(bytes, 0)?;
    if be != FAT_MAGIC_BE && be != FAT_MAGIC_64_BE {
        return Ok(bytes);
    }
    let nfat = u32_be(bytes, 4)?.min(16) as usize;
    let mut arm64_slice: Option<&[u8]> = None;
    let mut x64_slice: Option<&[u8]> = None;
    for i in 0..nfat {
        let (cputype, offset, size) = if be == FAT_MAGIC_BE {
            let off = 8 + i * 20;
            (u32_be(bytes, off)?, u32_be(bytes, off + 8)? as usize, u32_be(bytes, off + 12)? as usize)
        } else {
            let off = 8 + i * 32;
            (u32_be(bytes, off)?, u64_be(bytes, off + 8)? as usize, u64_be(bytes, off + 16)? as usize)
        };
        let Some(slice) = bytes.get(offset..offset.saturating_add(size)) else { continue };
        if cputype == CPU_TYPE_ARM64 {
            arm64_slice = Some(slice);
        } else if cputype == CPU_TYPE_X86_64 {
            x64_slice = Some(slice);
        }
    }
    arm64_slice.or(x64_slice).ok_or_else(|| PluginHostError::UnsupportedArch("Universal Mach-O contains neither arm64 nor x86_64 slice".into()))
}

#[derive(Clone, Debug)]
struct IndirectSection {
    addr: u64,
    size: u64,
    sec_type: u32,
    reserved1: u32,
    reserved2: u32,
}

/// Loads a 64-bit Mach-O or Universal Fat binary (`.plugin`, `.bundle`, `.aex`, `.prm`, `.aip`).
pub fn load_macho64<'a>(raw_bytes: &'a [u8], mem: &mut PagedMemory) -> Result<(LoadedImageInfo, Vec<&'a [u8]>)> {
    let bytes = select_macho_slice(raw_bytes)?;
    if bytes.len() < 32 || u32_le(bytes, 0)? != MH_MAGIC_64 {
        return Err(PluginHostError::InvalidBinary("not a 64-bit Mach-O binary".into()));
    }
    let cputype = u32_le(bytes, 4)?;
    let arch = match cputype {
        CPU_TYPE_ARM64 => GuestArch::AArch64,
        CPU_TYPE_X86_64 => GuestArch::X86_64,
        other => return Err(PluginHostError::UnsupportedArch(format!("unsupported Mach-O cputype {other:#x}"))),
    };
    let ncmds = u32_le(bytes, 16)?.min(512) as usize;
    let mut cmd_off = 32usize;

    let mut min_vmaddr = u64::MAX;
    let mut segments = Vec::new();
    let mut indirect_secs = Vec::new();
    let mut symtab: Option<(usize, usize, usize, usize)> = None;
    let mut dysym_indirect: Option<(usize, usize)> = None;
    let mut dyld_binds: Vec<(usize, usize)> = Vec::new();
    let mut rsrc_blobs: Vec<&'a [u8]> = Vec::new();

    for _ in 0..ncmds {
        if cmd_off + 8 > bytes.len() {
            break;
        }
        let cmd = u32_le(bytes, cmd_off)?;
        let cmdsize = u32_le(bytes, cmd_off + 4)? as usize;
        if cmdsize < 8 || cmd_off + cmdsize > bytes.len() {
            break;
        }
        match cmd {
            LC_SEGMENT_64 if cmdsize >= 72 => {
                let segname = &bytes[cmd_off + 8..cmd_off + 24];
                let vmaddr = u64_le(bytes, cmd_off + 24)?;
                let vmsize = u64_le(bytes, cmd_off + 32)?;
                let fileoff = u64_le(bytes, cmd_off + 40)? as usize;
                let filesize = u64_le(bytes, cmd_off + 48)? as usize;
                let nsects = u32_le(bytes, cmd_off + 64)?.min(128) as usize;
                if !segname.starts_with(b"__PAGEZERO") && vmsize > 0 {
                    min_vmaddr = min_vmaddr.min(vmaddr);
                    segments.push((vmaddr, vmsize, fileoff, filesize));
                }
                parse_macho_sections(bytes, cmd_off + 72, nsects, &mut indirect_secs, &mut rsrc_blobs);
            }
            LC_SYMTAB if cmdsize >= 24 => {
                symtab = Some((
                    u32_le(bytes, cmd_off + 8)? as usize,
                    u32_le(bytes, cmd_off + 12)? as usize,
                    u32_le(bytes, cmd_off + 16)? as usize,
                    u32_le(bytes, cmd_off + 20)? as usize,
                ));
            }
            LC_DYSYMTAB if cmdsize >= 80 => {
                dysym_indirect = Some((u32_le(bytes, cmd_off + 56)? as usize, u32_le(bytes, cmd_off + 60)? as usize));
            }
            LC_DYLD_INFO | LC_DYLD_INFO_ONLY if cmdsize >= 48 => {
                for off_pair in [(16, 20), (24, 28), (32, 36)] {
                    let boff = u32_le(bytes, cmd_off + off_pair.0)? as usize;
                    let bsz = u32_le(bytes, cmd_off + off_pair.1)? as usize;
                    if boff > 0 && bsz > 0 {
                        dyld_binds.push((boff, bsz));
                    }
                }
            }
            _ => {}
        }
        cmd_off += cmdsize;
    }

    let slide = if min_vmaddr == u64::MAX {
        DEFAULT_IMAGE_BASE
    } else if min_vmaddr < 0x0001_0000_0000 {
        DEFAULT_IMAGE_BASE - min_vmaddr
    } else {
        0
    };
    let image_base = if min_vmaddr == u64::MAX { DEFAULT_IMAGE_BASE } else { min_vmaddr + slide };

    let mut seg_bases = Vec::new();
    for &(vmaddr, vmsize, fileoff, filesize) in &segments {
        let addr = vmaddr + slide;
        seg_bases.push(addr);
        mem.zero_fill(addr, vmsize as usize)?;
        let copy_len = filesize.min(vmsize as usize);
        if copy_len > 0
            && let Some(slice) = bytes.get(fileoff..fileoff.saturating_add(copy_len))
        {
            mem.write_bytes(addr, slice)?;
        }
    }

    let mut exports = BTreeMap::new();
    let mut sym_names = Vec::new();
    if let Some((symoff, nsyms, stroff, strsize)) = symtab {
        parse_symtab(bytes, symoff, nsyms, stroff, strsize, slide, &mut exports, &mut sym_names)?;
    }

    let mut imports = Vec::new();
    if let Some((indirect_off, indirect_count)) = dysym_indirect {
        collect_indirect_imports(bytes, indirect_off, indirect_count, slide, &indirect_secs, &sym_names, &mut imports)?;
    }
    for (boff, bsz) in dyld_binds {
        if let Some(bslice) = bytes.get(boff..boff.saturating_add(bsz)) {
            parse_dyld_bind_opcodes(bslice, &seg_bases, &mut imports);
        }
    }

    Ok((LoadedImageInfo { format: BinaryFormat::MachO64, arch, os: GuestOs::MacOs, image_base, dll_entry: None, exports, imports }, rsrc_blobs))
}

fn parse_macho_sections<'a>(bytes: &'a [u8], base_off: usize, nsects: usize, indirect_secs: &mut Vec<IndirectSection>, rsrc_blobs: &mut Vec<&'a [u8]>) {
    for i in 0..nsects {
        let soff = base_off + i * 80;
        if soff + 80 > bytes.len() {
            break;
        }
        let sectname = &bytes[soff..soff + 16];
        let (Ok(addr), Ok(size), Ok(foff), Ok(flags), Ok(r1), Ok(r2)) = (
            u64_le(bytes, soff + 32),
            u64_le(bytes, soff + 40),
            u32_le(bytes, soff + 48),
            u32_le(bytes, soff + 64),
            u32_le(bytes, soff + 68),
            u32_le(bytes, soff + 72),
        ) else {
            break;
        };
        let sec_type = flags & 0xFF;
        if matches!(sec_type, S_NON_LAZY_SYMBOL_POINTERS | S_LAZY_SYMBOL_POINTERS | S_SYMBOL_STUBS) {
            indirect_secs.push(IndirectSection { addr, size, sec_type, reserved1: r1, reserved2: r2 });
        }
        if (sectname.starts_with(b"__pipl") || sectname.starts_with(b"__rsrc") || sectname.starts_with(b"__const"))
            && let Some(slice) = bytes.get(foff as usize..(foff as usize).saturating_add(size as usize))
        {
            rsrc_blobs.push(slice);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_symtab(
    bytes: &[u8],
    symoff: usize,
    nsyms: usize,
    stroff: usize,
    strsize: usize,
    slide: u64,
    exports: &mut BTreeMap<String, u64>,
    sym_names: &mut Vec<String>,
) -> Result<()> {
    let strtab = bytes.get(stroff..stroff.saturating_add(strsize)).unwrap_or(&[]);
    for i in 0..nsyms.min(32768) {
        let off = symoff + i * 16;
        if off + 16 > bytes.len() {
            break;
        }
        let strx = u32_le(bytes, off)? as usize;
        let n_type = bytes[off + 4];
        let n_value = u64_le(bytes, off + 8)?;
        let raw_name = strtab
            .get(strx..)
            .map(|t| {
                let end = t.iter().position(|&c| c == 0).unwrap_or(t.len()).min(512);
                String::from_utf8_lossy(&t[..end]).into_owned()
            })
            .unwrap_or_default();
        let clean_name = raw_name.strip_prefix('_').unwrap_or(&raw_name).to_string();
        sym_names.push(clean_name.clone());
        if (n_type & N_EXT) != 0 && (n_type & N_TYPE) == N_SECT && n_value != 0 && !clean_name.is_empty() {
            exports.insert(clean_name, n_value + slide);
            exports.insert(raw_name, n_value + slide);
        } else if (n_type & N_TYPE) == N_UNDF {
            // external undefined symbol stays in sym_names for indirect stub lookup
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn collect_indirect_imports(
    bytes: &[u8],
    indirect_off: usize,
    indirect_count: usize,
    slide: u64,
    secs: &[IndirectSection],
    sym_names: &[String],
    imports: &mut Vec<ImportSymbol>,
) -> Result<()> {
    for sec in secs {
        let stride = if sec.sec_type == S_SYMBOL_STUBS { u64::from(sec.reserved2.max(4)) } else { 8 };
        let count = (sec.size / stride) as usize;
        for idx in 0..count.min(4096) {
            let tbl_idx = sec.reserved1 as usize + idx;
            if tbl_idx >= indirect_count {
                break;
            }
            let ioff = indirect_off + tbl_idx * 4;
            if ioff + 4 > bytes.len() {
                break;
            }
            let sym_idx = u32_le(bytes, ioff)? as usize;
            let Some(name) = sym_names.get(sym_idx).filter(|s| !s.is_empty()) else { continue };
            let target_addr = sec.addr + slide + (idx as u64) * stride;
            // For pointer sections (__got / __la_symbol_ptr), slot_addr is the pointer slot;
            // for direct stub sections without GOT, negative/high bit marks direct stub trap patch.
            let slot_addr = if sec.sec_type == S_SYMBOL_STUBS { target_addr | (1u64 << 62) } else { target_addr };
            imports.push(ImportSymbol { library: "libSystem.B.dylib".into(), name: name.clone(), slot_addr });
        }
    }
    Ok(())
}

fn read_uleb128(data: &[u8], pos: &mut usize) -> u64 {
    let mut val = 0u64;
    let mut shift = 0u32;
    while *pos < data.len() && shift < 64 {
        let b = data[*pos];
        *pos += 1;
        val |= u64::from(b & 0x7F) << shift;
        if (b & 0x80) == 0 {
            break;
        }
        shift += 7;
    }
    val
}

fn read_sleb128(data: &[u8], pos: &mut usize) -> i64 {
    let mut val = 0i64;
    let mut shift = 0u32;
    let mut byte = 0u8;
    while *pos < data.len() && shift < 64 {
        byte = data[*pos];
        *pos += 1;
        val |= i64::from(byte & 0x7F) << shift;
        shift += 7;
        if (byte & 0x80) == 0 {
            break;
        }
    }
    if shift < 64 && (byte & 0x40) != 0 {
        val |= !0i64 << shift;
    }
    val
}

fn parse_dyld_bind_opcodes(opcodes: &[u8], seg_bases: &[u64], imports: &mut Vec<ImportSymbol>) {
    let mut pos = 0usize;
    let mut seg_idx = 0usize;
    let mut seg_off = 0u64;
    let mut sym_name = String::new();
    while pos < opcodes.len() && imports.len() < 4096 {
        let op = opcodes[pos] & 0xF0;
        let imm = opcodes[pos] & 0x0F;
        pos += 1;
        match op {
            0x00 => break, // BIND_OPCODE_DONE
            0x10 | 0x20 => {
                if imm == 15 {
                    let _ = read_uleb128(opcodes, &mut pos);
                }
            }
            0x30 => {
                let _ = read_sleb128(opcodes, &mut pos);
            }
            0x40 => {
                let start = pos;
                while pos < opcodes.len() && opcodes[pos] != 0 {
                    pos += 1;
                }
                let raw = String::from_utf8_lossy(&opcodes[start..pos]);
                sym_name = raw.strip_prefix('_').unwrap_or(&raw).to_string();
                if pos < opcodes.len() {
                    pos += 1;
                }
            }
            0x50 => {} // BIND_OPCODE_SET_TYPE_IMM
            0x60 => {
                let _ = read_sleb128(opcodes, &mut pos);
            }
            0x70 => {
                seg_idx = imm as usize;
                seg_off = read_uleb128(opcodes, &mut pos);
            }
            0x80 => {
                seg_off = seg_off.wrapping_add_signed(read_sleb128(opcodes, &mut pos));
            }
            0x90 => {
                if let Some(&base) = seg_bases.get(seg_idx)
                    && !sym_name.is_empty()
                {
                    imports.push(ImportSymbol { library: "dyld".into(), name: sym_name.clone(), slot_addr: base + seg_off });
                }
                seg_off = seg_off.wrapping_add(8);
            }
            0xA0 => {
                if let Some(&base) = seg_bases.get(seg_idx)
                    && !sym_name.is_empty()
                {
                    imports.push(ImportSymbol { library: "dyld".into(), name: sym_name.clone(), slot_addr: base + seg_off });
                }
                let add = read_uleb128(opcodes, &mut pos);
                seg_off = seg_off.wrapping_add(8).wrapping_add(add);
            }
            0xB0 => {
                if let Some(&base) = seg_bases.get(seg_idx)
                    && !sym_name.is_empty()
                {
                    imports.push(ImportSymbol { library: "dyld".into(), name: sym_name.clone(), slot_addr: base + seg_off });
                }
                seg_off = seg_off.wrapping_add(8 + u64::from(imm) * 8);
            }
            0xC0 => {
                let count = read_uleb128(opcodes, &mut pos).min(1024);
                let skip = read_uleb128(opcodes, &mut pos);
                for _ in 0..count {
                    if let Some(&base) = seg_bases.get(seg_idx)
                        && !sym_name.is_empty()
                    {
                        imports.push(ImportSymbol { library: "dyld".into(), name: sym_name.clone(), slot_addr: base + seg_off });
                    }
                    seg_off = seg_off.wrapping_add(8 + skip);
                }
            }
            _ => break,
        }
    }
}
