//! Portable PE32+ (`x86_64` & `ARM64`) image loader, base relocator, export/import resolver,
//! and `.rsrc` resource extractor.

use std::collections::BTreeMap;

use super::mem::{DEFAULT_IMAGE_BASE, PagedMemory};
use super::{BinaryFormat, GuestArch, GuestOs, ImportSymbol, LoadedImageInfo, PluginHostError, Result};

const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
const IMAGE_FILE_MACHINE_ARM64: u16 = 0xAA64;
const PE32_PLUS_MAGIC: u16 = 0x020B;
const IMAGE_REL_BASED_ABSOLUTE: u16 = 0;
const IMAGE_REL_BASED_DIR64: u16 = 10;

#[derive(Clone, Debug)]
struct PeSection {
    rva: u32,
    vsize: u32,
    raw_off: u32,
    raw_size: u32,
}

fn read_u16_le(b: &[u8], off: usize) -> Result<u16> {
    let s = b.get(off..off + 2).ok_or_else(|| PluginHostError::InvalidBinary("truncated PE u16".into()))?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32_le(b: &[u8], off: usize) -> Result<u32> {
    let s = b.get(off..off + 4).ok_or_else(|| PluginHostError::InvalidBinary("truncated PE u32".into()))?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_u64_le(b: &[u8], off: usize) -> Result<u64> {
    let s = b.get(off..off + 8).ok_or_else(|| PluginHostError::InvalidBinary("truncated PE u64".into()))?;
    Ok(u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}

fn rva_to_file_offset(sections: &[PeSection], rva: u32) -> Option<usize> {
    for s in sections {
        let span = s.vsize.max(s.raw_size);
        if rva >= s.rva && rva < s.rva.saturating_add(span) {
            let delta = rva - s.rva;
            if delta < s.raw_size {
                return Some((s.raw_off + delta) as usize);
            }
        }
    }
    None
}

fn read_file_cstring(bytes: &[u8], off: usize) -> String {
    let Some(tail) = bytes.get(off..) else { return String::new() };
    let end = tail.iter().position(|&c| c == 0).unwrap_or(tail.len()).min(512);
    String::from_utf8_lossy(&tail[..end]).into_owned()
}

pub fn is_pe_binary(bytes: &[u8]) -> bool {
    if bytes.len() < 0x40 || &bytes[0..2] != b"MZ" {
        return false;
    }
    let Ok(pe_off) = read_u32_le(bytes, 0x3C) else { return false };
    let off = pe_off as usize;
    bytes.get(off..off + 4) == Some(b"PE\0\0")
}

/// Parses and maps a PE32+ DLL (`.8bf`, `.prm`, `.aex`, `.aip`) into `mem`.
pub fn load_pe64<'a>(bytes: &'a [u8], mem: &mut PagedMemory) -> Result<(LoadedImageInfo, Vec<&'a [u8]>)> {
    if !is_pe_binary(bytes) {
        return Err(PluginHostError::InvalidBinary("not a valid PE binary".into()));
    }
    let pe_off = read_u32_le(bytes, 0x3C)? as usize;
    let machine = read_u16_le(bytes, pe_off + 4)?;
    let arch = match machine {
        IMAGE_FILE_MACHINE_AMD64 => GuestArch::X86_64,
        IMAGE_FILE_MACHINE_ARM64 => GuestArch::AArch64,
        other => {
            return Err(PluginHostError::UnsupportedArch(format!("PE machine {other:#06x} (32-bit x86 plugins require 64-bit PE32+ x64/ARM64 build)")));
        }
    };
    let num_sections = read_u16_le(bytes, pe_off + 6)? as usize;
    let opt_size = read_u16_le(bytes, pe_off + 20)? as usize;
    let opt_off = pe_off + 24;
    if read_u16_le(bytes, opt_off)? != PE32_PLUS_MAGIC {
        return Err(PluginHostError::UnsupportedArch("PE32 (32-bit) image is not PE32+ 64-bit".into()));
    }
    let entry_rva = read_u32_le(bytes, opt_off + 16)?;
    let preferred_base = read_u64_le(bytes, opt_off + 24)?;
    let size_of_headers = read_u32_le(bytes, opt_off + 60)? as usize;
    let image_base = if (0x0001_0000_0000..0x0100_0000_0000).contains(&preferred_base) { preferred_base } else { DEFAULT_IMAGE_BASE };

    let dd_off = opt_off + 112;
    let num_rva_sizes = read_u32_le(bytes, opt_off + 108)?.min(16) as usize;
    let dir_entry = |idx: usize| -> Result<(u32, u32)> {
        if idx >= num_rva_sizes || dd_off + (idx + 1) * 8 > opt_off + opt_size {
            return Ok((0, 0));
        }
        Ok((read_u32_le(bytes, dd_off + idx * 8)?, read_u32_le(bytes, dd_off + idx * 8 + 4)?))
    };
    let (exp_rva, exp_size) = dir_entry(0)?;
    let (imp_rva, _) = dir_entry(1)?;
    let (rsrc_rva, rsrc_size) = dir_entry(2)?;
    let (reloc_rva, reloc_size) = dir_entry(5)?;

    let sec_table_off = opt_off + opt_size;
    let mut sections = Vec::with_capacity(num_sections.min(96));
    for i in 0..num_sections.min(96) {
        let soff = sec_table_off + i * 40;
        let vsize = read_u32_le(bytes, soff + 8)?;
        let rva = read_u32_le(bytes, soff + 12)?;
        let raw_size = read_u32_le(bytes, soff + 16)?;
        let raw_off = read_u32_le(bytes, soff + 20)?;
        sections.push(PeSection { rva, vsize, raw_off, raw_size });
    }

    // Map headers + sections into guest memory
    let hdr_len = size_of_headers.min(bytes.len()).min(65536);
    mem.write_bytes(image_base, &bytes[..hdr_len])?;
    for s in &sections {
        let map_addr = image_base + u64::from(s.rva);
        let alloc_len = s.vsize.max(s.raw_size) as usize;
        if alloc_len > 0 {
            mem.zero_fill(map_addr, alloc_len)?;
        }
        let copy_len = (s.raw_size as usize).min(alloc_len);
        if copy_len > 0
            && let Some(slice) = bytes.get(s.raw_off as usize..s.raw_off as usize + copy_len)
        {
            mem.write_bytes(map_addr, slice)?;
        }
    }

    // Apply base relocations if relocated
    let delta = image_base.wrapping_sub(preferred_base);
    if delta != 0 && reloc_rva != 0 && reloc_size >= 8 {
        apply_pe_relocations(mem, image_base, reloc_rva, reloc_size, delta)?;
    }

    let exports = parse_pe_exports(bytes, &sections, image_base, exp_rva, exp_size)?;
    let imports = parse_pe_imports(bytes, &sections, image_base, imp_rva)?;
    let rsrc_blobs = extract_pe_resource_leaves(bytes, &sections, rsrc_rva, rsrc_size);

    let dll_entry = (entry_rva != 0).then_some(image_base + u64::from(entry_rva));
    Ok((LoadedImageInfo { format: BinaryFormat::Pe64, arch, os: GuestOs::Windows, image_base, dll_entry, exports, imports }, rsrc_blobs))
}

fn apply_pe_relocations(mem: &mut PagedMemory, image_base: u64, reloc_rva: u32, reloc_size: u32, delta: u64) -> Result<()> {
    let mut cur = image_base + u64::from(reloc_rva);
    let end = cur + u64::from(reloc_size);
    while cur + 8 <= end {
        let page_rva = mem.read_u32(cur)?;
        let block_size = mem.read_u32(cur + 4)?;
        if block_size < 8 || cur + u64::from(block_size) > end {
            break;
        }
        let count = (block_size - 8) / 2;
        for i in 0..count {
            let entry = mem.read_u16(cur + 8 + u64::from(i) * 2)?;
            let rtype = entry >> 12;
            let offset = u64::from(entry & 0x0FFF);
            match rtype {
                IMAGE_REL_BASED_ABSOLUTE => {}
                IMAGE_REL_BASED_DIR64 => {
                    let target = image_base + u64::from(page_rva) + offset;
                    let val = mem.read_u64(target)?;
                    mem.write_u64(target, val.wrapping_add(delta))?;
                }
                _ => {}
            }
        }
        cur += u64::from(block_size);
    }
    Ok(())
}

fn parse_pe_exports(bytes: &[u8], sections: &[PeSection], image_base: u64, exp_rva: u32, exp_size: u32) -> Result<BTreeMap<String, u64>> {
    let mut exports = BTreeMap::new();
    let Some(eo) = (exp_rva != 0).then(|| rva_to_file_offset(sections, exp_rva)).flatten() else {
        return Ok(exports);
    };
    if eo + 40 > bytes.len() {
        return Ok(exports);
    }
    let num_funcs = read_u32_le(bytes, eo + 20)?.min(8192);
    let num_names = read_u32_le(bytes, eo + 24)?.min(8192);
    let addr_funcs_rva = read_u32_le(bytes, eo + 28)?;
    let addr_names_rva = read_u32_le(bytes, eo + 32)?;
    let addr_ords_rva = read_u32_le(bytes, eo + 36)?;
    let (Some(funcs_off), Some(names_off), Some(ords_off)) =
        (rva_to_file_offset(sections, addr_funcs_rva), rva_to_file_offset(sections, addr_names_rva), rva_to_file_offset(sections, addr_ords_rva))
    else {
        return Ok(exports);
    };
    for i in 0..num_names as usize {
        let name_rva = read_u32_le(bytes, names_off + i * 4)?;
        let ord_idx = read_u16_le(bytes, ords_off + i * 2)? as u32;
        if ord_idx >= num_funcs {
            continue;
        }
        let fn_rva = read_u32_le(bytes, funcs_off + (ord_idx as usize) * 4)?;
        // Skip forwarded exports pointing inside the export directory
        if fn_rva >= exp_rva && fn_rva < exp_rva.saturating_add(exp_size) {
            continue;
        }
        if let Some(noff) = rva_to_file_offset(sections, name_rva) {
            let name = read_file_cstring(bytes, noff);
            if !name.is_empty() {
                exports.insert(name, image_base + u64::from(fn_rva));
            }
        }
    }
    Ok(exports)
}

fn parse_pe_imports(bytes: &[u8], sections: &[PeSection], image_base: u64, imp_rva: u32) -> Result<Vec<ImportSymbol>> {
    let mut imports = Vec::new();
    let Some(mut io) = (imp_rva != 0).then(|| rva_to_file_offset(sections, imp_rva)).flatten() else {
        return Ok(imports);
    };
    while io + 20 <= bytes.len() && imports.len() < 4096 {
        let ilt_rva = read_u32_le(bytes, io)?;
        let name_rva = read_u32_le(bytes, io + 12)?;
        let iat_rva = read_u32_le(bytes, io + 16)?;
        if ilt_rva == 0 && name_rva == 0 && iat_rva == 0 {
            break;
        }
        let dll = rva_to_file_offset(sections, name_rva).map(|off| read_file_cstring(bytes, off)).unwrap_or_default();
        let lookup_rva = if ilt_rva != 0 { ilt_rva } else { iat_rva };
        if let Some(mut toff) = rva_to_file_offset(sections, lookup_rva) {
            let mut slot = 0u64;
            while toff + 8 <= bytes.len() && imports.len() < 4096 {
                let entry = read_u64_le(bytes, toff)?;
                if entry == 0 {
                    break;
                }
                let sym_name = if (entry & (1u64 << 63)) != 0 {
                    format!("#{}", entry & 0xFFFF)
                } else {
                    rva_to_file_offset(sections, (entry & 0x7FFF_FFFF) as u32).map(|hoff| read_file_cstring(bytes, hoff + 2)).unwrap_or_default()
                };
                imports.push(ImportSymbol { library: dll.clone(), name: sym_name, slot_addr: image_base + u64::from(iat_rva) + slot * 8 });
                toff += 8;
                slot += 1;
            }
        }
        io += 20;
    }
    Ok(imports)
}

fn extract_pe_resource_leaves<'a>(bytes: &'a [u8], sections: &[PeSection], rsrc_rva: u32, rsrc_size: u32) -> Vec<&'a [u8]> {
    let mut leaves = Vec::new();
    let Some(base_off) = (rsrc_rva != 0 && rsrc_size >= 16).then(|| rva_to_file_offset(sections, rsrc_rva)).flatten() else {
        return leaves;
    };
    walk_rsrc_dir(bytes, sections, base_off, 0, 0, &mut leaves);
    leaves
}

fn walk_rsrc_dir<'a>(bytes: &'a [u8], sections: &[PeSection], base_off: usize, rel_dir_off: usize, depth: u8, leaves: &mut Vec<&'a [u8]>) {
    if depth > 4 || leaves.len() >= 128 {
        return;
    }
    let dir_off = base_off.saturating_add(rel_dir_off);
    let (Ok(named), Ok(ided)) = (read_u16_le(bytes, dir_off + 12), read_u16_le(bytes, dir_off + 14)) else {
        return;
    };
    let count = (usize::from(named) + usize::from(ided)).min(128);
    for i in 0..count {
        let eoff = dir_off + 16 + i * 8;
        let Ok(offset_or_dir) = read_u32_le(bytes, eoff + 4) else { return };
        if (offset_or_dir & 0x8000_0000) != 0 {
            walk_rsrc_dir(bytes, sections, base_off, (offset_or_dir & 0x7FFF_FFFF) as usize, depth + 1, leaves);
        } else {
            let data_entry_off = base_off.saturating_add(offset_or_dir as usize);
            let (Ok(data_rva), Ok(data_len)) = (read_u32_le(bytes, data_entry_off), read_u32_le(bytes, data_entry_off + 4)) else {
                continue;
            };
            if let Some(foff) = rva_to_file_offset(sections, data_rva)
                && let Some(slice) = bytes.get(foff..foff.saturating_add(data_len as usize))
            {
                leaves.push(slice);
            }
        }
    }
}
