//! Universal Adobe `PiPL` (Plug-in Property List) and `JSON_PIPL` metadata parser.
//!
//! Supports all three formats shipped by Windows and macOS third-party C++ plugins:
//! 1. Little-endian Windows binary `PiPL` (`MIB8` vendor + reversed 4CC keys from `cnvtpipl.exe`),
//! 2. Big-endian macOS / classic `PiPL` (`8BIM` vendor + big-endian 4CC keys),
//! 3. Modern Photoshop / Creative Cloud `JSON_PIPL` embedded in `.rsrc` or Mach-O `__TEXT`.

use serde::{Deserialize, Serialize};

use super::PluginFormat;

/// Extracted metadata and per-platform entrypoint names from a plugin's `PiPL` resource.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiplMetadata {
    pub kind_4cc: String,
    pub format_hint: Option<PluginFormat>,
    pub name: String,
    pub category: String,
    pub match_name: String,
    pub version_major: u16,
    pub version_minor: u16,
    pub entry_win64: Option<String>,
    pub entry_win32: Option<String>,
    pub entry_mac_arm64: Option<String>,
    pub entry_mac_x86_64: Option<String>,
}

impl PiplMetadata {
    pub fn is_populated(&self) -> bool {
        !self.name.is_empty() || !self.kind_4cc.is_empty() || self.entry_win64.is_some() || self.entry_mac_arm64.is_some() || self.entry_mac_x86_64.is_some()
    }
}

fn kind_to_format(kind: &str) -> Option<PluginFormat> {
    match kind {
        "8BFM" | "8BIF" | "8BSM" => Some(PluginFormat::Photoshop8bf),
        "eFKT" | "AEgp" => Some(PluginFormat::AfterEffectsAex),
        "VFlt" | "PrEf" | "IMPT" => Some(PluginFormat::PremierePrm),
        "ARPI" | "AIFl" | "AILv" => Some(PluginFormat::IllustratorAip),
        _ => None,
    }
}

/// Scans `blobs` (extracted `.rsrc` items or whole binary fallback) for `PiPL` and `JSON_PIPL`,
/// merging both binary `PiPL` and `JSON_PIPL` leaves when a modern plugin carries both.
pub fn parse_pipl_from_blobs(blobs: &[&[u8]], full_binary: &[u8]) -> PiplMetadata {
    let mut merged = PiplMetadata::default();
    for blob in blobs {
        if let Some(m) = try_parse_json_pipl(blob) {
            merge_pipl(&mut merged, m);
        }
        if let Some(m) = try_parse_binary_pipl(blob) {
            merge_pipl(&mut merged, m);
        }
    }
    if let Some(m) = try_parse_json_pipl(full_binary) {
        merge_pipl(&mut merged, m);
    }
    if !merged.is_populated()
        && let Some(m) = try_parse_binary_pipl(full_binary)
    {
        merge_pipl(&mut merged, m);
    }
    merged
}

fn merge_pipl(dst: &mut PiplMetadata, src: PiplMetadata) {
    if dst.kind_4cc.is_empty() && !src.kind_4cc.is_empty() {
        dst.kind_4cc = src.kind_4cc;
    }
    if dst.format_hint.is_none() {
        dst.format_hint = src.format_hint;
    }
    if dst.name.is_empty() && !src.name.is_empty() {
        dst.name = src.name;
    }
    if dst.category.is_empty() && !src.category.is_empty() {
        dst.category = src.category;
    }
    if dst.match_name.is_empty() && !src.match_name.is_empty() {
        dst.match_name = src.match_name;
    }
    if dst.version_major == 0 && src.version_major != 0 {
        dst.version_major = src.version_major;
        dst.version_minor = src.version_minor;
    }
    if dst.entry_win64.is_none() {
        dst.entry_win64 = src.entry_win64;
    }
    if dst.entry_win32.is_none() {
        dst.entry_win32 = src.entry_win32;
    }
    if dst.entry_mac_arm64.is_none() {
        dst.entry_mac_arm64 = src.entry_mac_arm64;
    }
    if dst.entry_mac_x86_64.is_none() {
        dst.entry_mac_x86_64 = src.entry_mac_x86_64;
    }
}

fn decode_pascal_or_c_string(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }
    let plen = data[0] as usize;
    if plen > 0 && plen < data.len() && data[1..1 + plen].iter().all(|&b| (0x20..=0x7E).contains(&b) || b >= 0x80) {
        return String::from_utf8_lossy(&data[1..1 + plen]).trim_matches('\0').trim().to_string();
    }
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).trim().to_string()
}

fn decode_c_string(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).trim().to_string()
}

fn apply_property(meta: &mut PiplMetadata, key: &[u8; 4], payload: &[u8], little_endian: bool) {
    let key_str = std::str::from_utf8(key).unwrap_or("");
    match key_str {
        "kind" if payload.len() >= 4 => {
            let mut k = [payload[0], payload[1], payload[2], payload[3]];
            if little_endian {
                k.reverse();
            }
            let s = String::from_utf8_lossy(&k).into_owned();
            meta.format_hint = kind_to_format(&s).or(meta.format_hint);
            meta.kind_4cc = s;
        }
        "name" => {
            let s = decode_pascal_or_c_string(payload);
            if !s.is_empty() {
                meta.name = s;
            }
        }
        "catg" => {
            let s = decode_pascal_or_c_string(payload);
            if !s.is_empty() {
                meta.category = s;
            }
        }
        "eMNA" | "mnam" => {
            let s = decode_pascal_or_c_string(payload);
            if !s.is_empty() {
                meta.match_name = s;
            }
        }
        "8664" | "epm6" => {
            let s = decode_c_string(payload);
            if !s.is_empty() {
                meta.entry_win64 = Some(s);
            }
        }
        "wx86" => {
            let s = decode_c_string(payload);
            if !s.is_empty() {
                meta.entry_win32 = Some(s);
            }
        }
        "m64a" | "macA" | "am64" => {
            let s = decode_c_string(payload);
            if !s.is_empty() {
                meta.entry_mac_arm64 = Some(s);
            }
        }
        "mi64" | "m64i" | "code" => {
            let s = decode_c_string(payload);
            if !s.is_empty() && meta.entry_mac_x86_64.is_none() {
                meta.entry_mac_x86_64 = Some(s);
            }
        }
        "vers" | "eVER" if payload.len() >= 4 => {
            let raw = if little_endian {
                u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]])
            } else {
                u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
            };
            meta.version_major = ((raw >> 16) & 0xFFFF) as u16;
            meta.version_minor = (raw & 0xFFFF) as u16;
        }
        _ => {}
    }
}

fn try_parse_binary_pipl(buf: &[u8]) -> Option<PiplMetadata> {
    let mut meta = PiplMetadata::default();
    let mut i = 0usize;
    let mut hits = 0usize;
    while i + 16 <= buf.len() {
        let sig = &buf[i..i + 4];
        let is_le = sig == b"MIB8";
        let is_be = sig == b"8BIM";
        if !is_le && !is_be {
            i += 1;
            continue;
        }
        let mut key = [buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]];
        if is_le {
            key.reverse();
        }
        if !key.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            i += 1;
            continue;
        }
        let len = if is_le {
            u32::from_le_bytes([buf[i + 12], buf[i + 13], buf[i + 14], buf[i + 15]]) as usize
        } else {
            u32::from_be_bytes([buf[i + 12], buf[i + 13], buf[i + 14], buf[i + 15]]) as usize
        };
        let payload_start = i + 16;
        if len > 65536 || payload_start + len > buf.len() {
            i += 4;
            continue;
        }
        apply_property(&mut meta, &key, &buf[payload_start..payload_start + len], is_le);
        hits += 1;
        let padded = (len + 3) & !3;
        i = (payload_start + padded).max(i + 4);
    }
    (hits > 0 && meta.is_populated()).then_some(meta)
}

fn try_parse_json_pipl(buf: &[u8]) -> Option<PiplMetadata> {
    let trimmed_start = buf.iter().position(|b| !b.is_ascii_whitespace());
    if let Some(first) = trimmed_start
        && buf[first] == b'{'
    {
        let end = buf.iter().rposition(|&b| b == b'}').map(|p| p + 1)?;
        if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&buf[first..end]) {
            let mut meta = PiplMetadata::default();
            extract_json_pipl_fields(&val, &mut meta);
            if meta.is_populated() {
                return Some(meta);
            }
        }
        if let Some(meta) = try_parse_lenient_json_pipl(&buf[first..end]) {
            return Some(meta);
        }
    }
    // Look for `"RequiredHost"` or `"CodeWin64X86"` JSON object inside a larger binary
    let needle = b"\"RequiredHost\"";
    let pos = buf.windows(needle.len()).position(|w| w == needle)?;
    let search_start = pos.saturating_sub(32768);
    let mut start = pos;
    let mut best_open = None;
    while start > search_start {
        start -= 1;
        if buf[start] == b'{' {
            best_open = Some(start);
        } else if buf[start] == 0 {
            break;
        }
    }
    let open = best_open?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut end = None;
    for (idx, &b) in buf[open..].iter().take(65536).enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(open + idx + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let slice = &buf[open..end?];
    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(slice) {
        let mut meta = PiplMetadata::default();
        extract_json_pipl_fields(&val, &mut meta);
        if meta.is_populated() {
            return Some(meta);
        }
    }
    try_parse_lenient_json_pipl(slice)
}

fn try_parse_lenient_json_pipl(buf: &[u8]) -> Option<PiplMetadata> {
    let text = String::from_utf8_lossy(buf);
    if !text.contains("\"RequiredHost\"") && !text.contains("\"CodeWin64") && !text.contains("\"CodeMac") {
        return None;
    }
    let mut meta = PiplMetadata::default();
    if let Some(k) = find_json_string_val(&text, "Kind") {
        let mapped = if k == "Filter" { "8BFM" } else { k.as_str() };
        meta.format_hint = kind_to_format(mapped);
        meta.kind_4cc = mapped.to_string();
    }
    if let Some(n) = find_json_string_val(&text, "Name") {
        meta.name = n.trim_start_matches("$$$/").rsplit('=').next().unwrap_or(&n).trim_end_matches("...").trim().to_string();
    }
    if let Some(c) = find_json_string_val(&text, "Category") {
        meta.category = c.trim_start_matches("$$$/").rsplit('=').next().unwrap_or(&c).trim().to_string();
    }
    if let Some(ev) = find_json_string_val(&text, "EventID") {
        meta.match_name = ev;
    }
    meta.entry_win64 = find_json_string_val(&text, "CodeWin64X86").or_else(|| find_json_string_val(&text, "CodeWin64ARM"));
    meta.entry_mac_arm64 = find_json_string_val(&text, "CodeMacARM64");
    meta.entry_mac_x86_64 = find_json_string_val(&text, "CodeMacIntel64");
    meta.is_populated().then_some(meta)
}

fn find_json_string_val(text: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let idx = text.find(&pat)?;
    let rest = text.get(idx + pat.len()..)?.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn extract_json_pipl_fields(val: &serde_json::Value, meta: &mut PiplMetadata) {
    let Some(obj) = val.as_object() else { return };
    if let Some(arr) = obj.get("Plugins").or_else(|| obj.get("plugins")).and_then(|v| v.as_array()) {
        for item in arr {
            extract_json_pipl_fields(item, meta);
        }
    }
    if let Some(k) = obj.get("Kind").and_then(|v| v.as_str()) {
        let mapped = match k {
            "Filter" => "8BFM",
            _ => k,
        };
        meta.format_hint = kind_to_format(mapped).or(meta.format_hint);
        meta.kind_4cc = mapped.to_string();
    }
    if let Some(n) = obj.get("Name").and_then(|v| v.as_str()) {
        meta.name = n.trim_start_matches("$$$/").rsplit('=').next().unwrap_or(n).trim_end_matches("...").trim().to_string();
    }
    if let Some(c) = obj.get("Category").and_then(|v| v.as_str()) {
        meta.category = c.trim_start_matches("$$$/").rsplit('=').next().unwrap_or(c).trim().to_string();
    }
    if let Some(e) = obj.get("CodeWin64X86").or_else(|| obj.get("CodeWin64ARM")).and_then(|v| v.as_str()) {
        meta.entry_win64 = Some(e.to_string());
    }
    if let Some(e) = obj.get("CodeMacARM64").and_then(|v| v.as_str()) {
        meta.entry_mac_arm64 = Some(e.to_string());
    }
    if let Some(e) = obj.get("CodeMacIntel64").and_then(|v| v.as_str()) {
        meta.entry_mac_x86_64 = Some(e.to_string());
    }
    if let Some(ev) = obj.get("HasTerminology").and_then(|t| t.get("EventID")).and_then(|v| v.as_str())
        && meta.match_name.is_empty()
    {
        meta.match_name = ev.to_string();
    }
}
