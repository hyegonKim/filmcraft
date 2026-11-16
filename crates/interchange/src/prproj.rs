//! Experimental reader for Premiere project documents (`.prproj`, a `<PremiereData>` XML document,
//! usually gzip-compressed), with its own RFC 1952 gzip / RFC 1951 DEFLATE decoder (the trailer's
//! CRC-32 and size are checked) and UTF-8 / UTF-16 decoding.
//!
//! Objects refer to each other with `ObjectID` / `ObjectUID` and `ObjectRef` / `ObjectURef`; times
//! are in ticks of 1/254 016 000 000 s, the same unit as [`filmcraft_time::TICKS_PER_SECOND`]. The
//! element names and nesting read here are inferred and have not been checked against documents
//! authored in Premiere, so only sequences, tracks, clip timing, markers and captions are read and
//! the import report says what was not.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use roxmltree::{Node, NodeId};

use filmcraft_media::MediaKind;
use filmcraft_project::{Caption, CaptionFormat, CaptionTrack, ClipId, ItemId, Label, Marker, MarkerId, MarkerKind, Sequence, TrackId, TrackKind};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};

use crate::common::{Builder, MediaSpec, empty_sequence, file_name, file_stem, kind_from_ext, rate_from_f64, resolve_path, settings_for};
use crate::xml::{child, child_text, elements, parse, text};
use crate::{Error, ImportOptions, Imported, Report, Result};

/// Whether `bytes` begins with the RFC 1952 gzip magic header (`0x1F 0x8B`).
pub fn is_gzip(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0x1F && bytes[1] == 0x8B
}

/// Whether `xml` is an Adobe Premiere Pro `<PremiereData>` document.
pub fn is_premiere_data(xml: &str) -> bool {
    let head = &xml[..xml.floor_char_boundary(xml.len().min(4096))];
    head.contains("<PremiereData") || head.contains("<premiereData")
}

/// Decompress `bytes` if gzip-compressed (`0x1F 0x8B`), and decode UTF-8 or UTF-16 into a `String`.
pub fn maybe_decompress(bytes: &[u8]) -> Result<String> {
    let raw = if is_gzip(bytes) { gunzip(bytes)? } else { bytes.to_vec() };
    decode_xml_bytes(&raw)
}

/// The format named in errors about the Premiere document itself.
const PREMIERE: &str = "Premiere project";

fn parse_err(msg: impl Into<String>) -> Error {
    Error::Parse { format: PREMIERE, message: msg.into() }
}

fn gzip_err(msg: impl Into<String>) -> Error {
    Error::Parse { format: "gzip", message: msg.into() }
}

fn text_err(msg: impl Into<String>) -> Error {
    Error::Parse { format: "XML text", message: msg.into() }
}

fn decode_xml_bytes(raw: &[u8]) -> Result<String> {
    if raw.len() >= 2 && raw[0] == 0xFF && raw[1] == 0xFE {
        // UTF-16LE with BOM
        let mut u16s = Vec::with_capacity((raw.len() - 2) / 2);
        let mut i = 2;
        while i + 1 < raw.len() {
            u16s.push(u16::from_le_bytes([raw[i], raw[i + 1]]));
            i += 2;
        }
        return String::from_utf16(&u16s).map_err(|e| text_err(format!("invalid UTF-16LE: {e}")));
    }
    if raw.len() >= 2 && raw[0] == 0xFE && raw[1] == 0xFF {
        // UTF-16BE with BOM
        let mut u16s = Vec::with_capacity((raw.len() - 2) / 2);
        let mut i = 2;
        while i + 1 < raw.len() {
            u16s.push(u16::from_be_bytes([raw[i], raw[i + 1]]));
            i += 2;
        }
        return String::from_utf16(&u16s).map_err(|e| text_err(format!("invalid UTF-16BE: {e}")));
    }
    let slice = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) { &raw[3..] } else { raw };
    std::str::from_utf8(slice).map(str::to_string).map_err(|e| text_err(format!("not UTF-8: {e}")))
}

// ---------------------------------------------------------------------------------------------
// RFC 1952 (gzip) + RFC 1951 (DEFLATE) decompressor & stored-block compressor
// ---------------------------------------------------------------------------------------------

const MAX_DECOMPRESSED_BYTES: usize = 128 * 1024 * 1024;

/// Compress `data` into a valid RFC 1952 gzip stream using stored DEFLATE blocks.
pub fn gzip_compress_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 32);
    // Gzip 10-byte header: ID1=0x1f, ID2=0x8b, CM=8 (deflate), FLG=0, MTIME=0, XFL=0, OS=255
    out.extend_from_slice(&[0x1F, 0x8B, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xFF]);
    if data.is_empty() {
        out.push(0x01); // BFINAL=1, BTYPE=00
        out.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF]);
    } else {
        let mut offset = 0;
        while offset < data.len() {
            let chunk_len = (data.len() - offset).min(65535);
            let is_last = offset + chunk_len == data.len();
            out.push(u8::from(is_last)); // BFINAL + BTYPE=00 (stored)
            let len16 = chunk_len as u16;
            let nlen16 = !len16;
            out.extend_from_slice(&len16.to_le_bytes());
            out.extend_from_slice(&nlen16.to_le_bytes());
            out.extend_from_slice(&data[offset..offset + chunk_len]);
            offset += chunk_len;
        }
    }
    let crc = crc32(data);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// CRC-32 (ISO 3309 / ITU-T V.42, reflected polynomial 0xEDB88320) lookup table, as RFC 1952 uses.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = CRC_TABLE[usize::from((crc as u8) ^ b)] ^ (crc >> 8);
    }
    !crc
}

fn gunzip(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() < 18 || bytes[0] != 0x1F || bytes[1] != 0x8B {
        return Err(gzip_err("invalid gzip header"));
    }
    if bytes[2] != 8 {
        return Err(gzip_err(format!("unsupported gzip compression method {}", bytes[2])));
    }
    let flg = bytes[3];
    let mut pos = 10usize;
    // FEXTRA
    if flg & 0x04 != 0 {
        if pos + 2 > bytes.len() {
            return Err(gzip_err("truncated gzip FEXTRA"));
        }
        let xlen = usize::from(u16::from_le_bytes([bytes[pos], bytes[pos + 1]]));
        pos += 2 + xlen;
    }
    // FNAME
    if flg & 0x08 != 0 {
        while pos < bytes.len() && bytes[pos] != 0 {
            pos += 1;
        }
        pos += 1;
    }
    // FCOMMENT
    if flg & 0x10 != 0 {
        while pos < bytes.len() && bytes[pos] != 0 {
            pos += 1;
        }
        pos += 1;
    }
    // FHCRC
    if flg & 0x02 != 0 {
        pos += 2;
    }
    let Some(body) = bytes.get(pos..).filter(|b| !b.is_empty()) else {
        return Err(gzip_err("truncated gzip header"));
    };
    let mut reader = BitReader::new(body);
    let out = inflate(&mut reader)?;
    // RFC 1952 trailer: CRC-32 and size (mod 2^32) of the uncompressed data, little-endian.
    let end = reader.consumed();
    let trailer = end.checked_add(8).and_then(|stop| body.get(end..stop)).ok_or_else(|| gzip_err("truncated gzip trailer (CRC-32 and size)"))?;
    let crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
    if crc != crc32(&out) {
        return Err(gzip_err("CRC-32 mismatch: the compressed document is damaged"));
    }
    // ISIZE is the length modulo 2^32: truncating the length is the comparison RFC 1952 defines.
    if size != out.len() as u32 {
        return Err(gzip_err("size mismatch: the compressed document is damaged"));
    }
    Ok(out)
}

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_buf: u64,
    bit_cnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, byte_pos: 0, bit_buf: 0, bit_cnt: 0 }
    }

    fn fill(&mut self, need: u32) -> Result<()> {
        while self.bit_cnt < need {
            let Some(&b) = self.data.get(self.byte_pos) else {
                return Err(gzip_err("unexpected end of the deflate stream"));
            };
            self.byte_pos += 1;
            self.bit_buf |= u64::from(b) << self.bit_cnt;
            self.bit_cnt += 8;
        }
        Ok(())
    }

    fn read_bits(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        self.fill(n)?;
        let mask = (1u64 << n) - 1;
        let val = (self.bit_buf & mask) as u32;
        self.bit_buf >>= n;
        self.bit_cnt -= n;
        Ok(val)
    }

    /// Bytes of the input used so far, once the stream is byte-aligned (whole unread bytes still in
    /// the bit buffer are given back).
    fn consumed(&mut self) -> usize {
        self.align_byte();
        self.byte_pos.saturating_sub((self.bit_cnt / 8) as usize)
    }

    fn align_byte(&mut self) {
        let drop_bits = self.bit_cnt & 7;
        self.bit_buf >>= drop_bits;
        self.bit_cnt -= drop_bits;
    }

    fn read_u16_aligned(&mut self) -> Result<u16> {
        self.align_byte();
        let lo = self.read_bits(8)? as u8;
        let hi = self.read_bits(8)? as u8;
        Ok(u16::from_le_bytes([lo, hi]))
    }

    fn read_bytes_aligned(&mut self, len: usize, out: &mut Vec<u8>) -> Result<()> {
        self.align_byte();
        for _ in 0..len {
            let b = self.read_bits(8)? as u8;
            out.push(b);
        }
        Ok(())
    }
}

struct HuffTree {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl HuffTree {
    fn from_lengths(lengths: &[u8]) -> Result<Self> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            let li = usize::from(l);
            if li >= 16 {
                return Err(gzip_err("invalid Huffman code length"));
            }
            if li > 0 {
                counts[li] = counts[li].saturating_add(1);
            }
        }
        let mut offsets = [0usize; 16];
        let mut total = 0usize;
        for bits in 1..=15 {
            offsets[bits] = total;
            total += usize::from(counts[bits]);
        }
        let mut symbols = vec![0u16; total];
        for (sym, &l) in lengths.iter().enumerate() {
            let li = usize::from(l);
            if li > 0 {
                if let Some(slot) = symbols.get_mut(offsets[li]) {
                    *slot = sym as u16;
                }
                offsets[li] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, r: &mut BitReader<'_>) -> Result<u16> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0usize;
        for len in 1..=15 {
            let bit = r.read_bits(1)? as i32;
            code |= bit;
            let count = i32::from(self.counts[len]);
            if code - count < first {
                let idx = index + (code - first) as usize;
                return self.symbols.get(idx).copied().ok_or_else(|| gzip_err("invalid Huffman symbol"));
            }
            index += count as usize;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(gzip_err("unmatched Huffman code"))
    }
}

fn fixed_trees() -> Result<(HuffTree, HuffTree)> {
    let mut lit = vec![0u8; 288];
    for l in &mut lit[0..=143] {
        *l = 8;
    }
    for l in &mut lit[144..=255] {
        *l = 9;
    }
    for l in &mut lit[256..=279] {
        *l = 7;
    }
    for l in &mut lit[280..=287] {
        *l = 8;
    }
    let dist = vec![5u8; 32];
    Ok((HuffTree::from_lengths(&lit)?, HuffTree::from_lengths(&dist)?))
}

fn dynamic_trees(r: &mut BitReader<'_>) -> Result<(HuffTree, HuffTree)> {
    let hlit = (r.read_bits(5)? as usize) + 257;
    let hdist = (r.read_bits(5)? as usize) + 1;
    let hclen = (r.read_bits(4)? as usize) + 4;
    const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
    let mut cl_lengths = [0u8; 19];
    for &idx in CL_ORDER.iter().take(hclen) {
        cl_lengths[idx] = r.read_bits(3)? as u8;
    }
    let cl_tree = HuffTree::from_lengths(&cl_lengths)?;
    let total = hlit + hdist;
    let mut lengths = Vec::with_capacity(total);
    while lengths.len() < total {
        let sym = cl_tree.decode(r)?;
        match sym {
            0..=15 => lengths.push(sym as u8),
            16 => {
                let prev = *lengths.last().ok_or_else(|| gzip_err("repeat code 16 with no previous length"))?;
                let repeat = (r.read_bits(2)? as usize) + 3;
                for _ in 0..repeat {
                    if lengths.len() < total {
                        lengths.push(prev);
                    }
                }
            }
            17 => {
                let repeat = (r.read_bits(3)? as usize) + 3;
                for _ in 0..repeat {
                    if lengths.len() < total {
                        lengths.push(0);
                    }
                }
            }
            18 => {
                let repeat = (r.read_bits(7)? as usize) + 11;
                for _ in 0..repeat {
                    if lengths.len() < total {
                        lengths.push(0);
                    }
                }
            }
            _ => return Err(gzip_err("invalid code length symbol")),
        }
    }
    let lit_tree = HuffTree::from_lengths(&lengths[..hlit])?;
    let dist_tree = HuffTree::from_lengths(&lengths[hlit..])?;
    Ok((lit_tree, dist_tree))
}

fn inflate(r: &mut BitReader<'_>) -> Result<Vec<u8>> {
    const LEN_BASE: [usize; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
    const LEN_EXTRA: [u32; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
    const DIST_BASE: [usize; 30] =
        [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
    const DIST_EXTRA: [u32; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

    let mut out = Vec::new();
    loop {
        let bfinal = r.read_bits(1)?;
        let btype = r.read_bits(2)?;
        match btype {
            0 => {
                let len = r.read_u16_aligned()?;
                let nlen = r.read_u16_aligned()?;
                if len != !nlen {
                    return Err(gzip_err("corrupt stored deflate block length"));
                }
                if out.len().saturating_add(usize::from(len)) > MAX_DECOMPRESSED_BYTES {
                    return Err(gzip_err("the decompressed document is larger than 128 MiB"));
                }
                r.read_bytes_aligned(usize::from(len), &mut out)?;
            }
            1 | 2 => {
                let (lit_tree, dist_tree) = if btype == 1 { fixed_trees()? } else { dynamic_trees(r)? };
                loop {
                    let sym = lit_tree.decode(r)?;
                    if sym < 256 {
                        if out.len() >= MAX_DECOMPRESSED_BYTES {
                            return Err(gzip_err("the decompressed document is larger than 128 MiB"));
                        }
                        out.push(sym as u8);
                    } else if sym == 256 {
                        break;
                    } else {
                        let len_idx = usize::from(sym - 257);
                        let Some(&base_len) = LEN_BASE.get(len_idx) else {
                            return Err(gzip_err("invalid deflate length symbol"));
                        };
                        let extra_bits = LEN_EXTRA.get(len_idx).copied().unwrap_or(0);
                        let length = base_len + (r.read_bits(extra_bits)? as usize);

                        let dist_sym = usize::from(dist_tree.decode(r)?);
                        let Some(&base_dist) = DIST_BASE.get(dist_sym) else {
                            return Err(gzip_err("invalid deflate distance symbol"));
                        };
                        let dist_extra = DIST_EXTRA.get(dist_sym).copied().unwrap_or(0);
                        let distance = base_dist + (r.read_bits(dist_extra)? as usize);
                        if distance == 0 || distance > out.len() {
                            return Err(gzip_err("invalid deflate back-reference distance"));
                        }
                        if out.len().saturating_add(length) > MAX_DECOMPRESSED_BYTES {
                            return Err(gzip_err("the decompressed document is larger than 128 MiB"));
                        }
                        for _ in 0..length {
                            let b = out[out.len() - distance];
                            out.push(b);
                        }
                    }
                }
            }
            _ => return Err(gzip_err("invalid deflate block type 3")),
        }
        if bfinal != 0 {
            break;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Premiere Pro `<PremiereData>` XML parser
// ---------------------------------------------------------------------------------------------

/// Deepest object-graph level a property search descends to.
const SEARCH_DEPTH: usize = 6;
/// Most objects a single property search visits: references can make the graph cyclic or very
/// wide, and a search must stay cheap whatever the document contains.
const SEARCH_BUDGET: usize = 2048;

struct PrprojImporter<'a, 'i, 'r> {
    b: Builder,
    opts: &'a ImportOptions,
    report: &'r mut Report,
    by_id: HashMap<&'i str, Node<'a, 'i>>,
    by_uid: HashMap<&'i str, Node<'a, 'i>>,
    media_by_node: HashMap<usize, ItemId>,
    /// A property search stopped at [`SEARCH_BUDGET`] (reported once).
    search_cut: Cell<bool>,
}

impl<'a, 'i, 'r> PrprojImporter<'a, 'i, 'r> {
    fn resolve(&self, n: Node<'a, 'i>) -> Node<'a, 'i> {
        if let Some(r) = n.attribute("ObjectRef")
            && let Some(&target) = self.by_id.get(r)
        {
            return target;
        }
        if let Some(u) = n.attribute("ObjectURef")
            && let Some(&target) = self.by_uid.get(u)
        {
            return target;
        }
        n
    }

    fn resolve_child(&self, n: Node<'a, 'i>, tag: &str) -> Option<Node<'a, 'i>> {
        child(n, tag).map(|c| self.resolve(c))
    }

    /// The first non-empty text of an element named in `tags` at or below `n`, following object
    /// references. Each object is visited once per search and a search visits at most
    /// [`SEARCH_BUDGET`] objects, so cyclic or very wide reference graphs stay cheap.
    fn find_text(&self, n: Node<'a, 'i>, tags: &[&str]) -> Option<&'a str> {
        let mut seen = HashSet::new();
        let mut budget = SEARCH_BUDGET;
        let found = self.find_text_in(n, tags, 0, &mut seen, &mut budget);
        if budget == 0 {
            self.search_cut.set(true);
        }
        found
    }

    fn find_text_in(&self, n: Node<'a, 'i>, tags: &[&str], depth: usize, seen: &mut HashSet<NodeId>, budget: &mut usize) -> Option<&'a str> {
        if depth > SEARCH_DEPTH || *budget == 0 {
            return None;
        }
        let n = self.resolve(n);
        if !seen.insert(n.id()) {
            return None;
        }
        *budget -= 1;
        for &tag in tags {
            if let Some(t) = child_text(n, tag)
                && !t.is_empty()
            {
                return Some(t);
            }
        }
        for c in elements(n) {
            let rc = self.resolve(c);
            if tags.iter().any(|&t| rc.tag_name().name().eq_ignore_ascii_case(t)) {
                let val = text(rc);
                if !val.is_empty() {
                    return Some(val);
                }
            }
            if let Some(found) = self.find_text_in(rc, tags, depth + 1, seen, budget) {
                return Some(found);
            }
        }
        None
    }

    fn register_all_media(&mut self, root: Node<'a, 'i>) {
        let mut media_nodes = Vec::new();
        for n in root.descendants().filter(|n| n.is_element()) {
            let tag = n.tag_name().name();
            if matches!(tag, "Media" | "MediaSource" | "MasterClip" | "ClipItem")
                && (n.attribute("ObjectID").is_some()
                    || n.attribute("ObjectUID").is_some()
                    || child(n, "ActualMediaFilePath").is_some()
                    || child(n, "FilePath").is_some()
                    || child(n, "RelativePath").is_some())
            {
                media_nodes.push(n);
            }
        }
        for n in media_nodes {
            let _ = self.ensure_media_for_node(n, FrameRate::FPS_24);
        }
    }

    fn ensure_media_for_node(&mut self, n: Node<'a, 'i>, default_rate: FrameRate) -> Option<ItemId> {
        let resolved = self.resolve(n);
        let key_id = resolved.id().get() as usize;
        if let Some(&existing) = self.media_by_node.get(&key_id) {
            return Some(existing);
        }
        let raw_path = self.find_text(resolved, &["ActualMediaFilePath", "FilePath", "RelativePath", "PathUrl", "pathurl"])?;
        let resolved_path = resolve_path(raw_path, self.opts.base_dir.as_deref());
        let name = self.find_text(resolved, &["Title", "Name", "ClipName"]).filter(|s| !s.is_empty()).unwrap_or_else(|| {
            let f = file_name(&resolved_path);
            if f.is_empty() { "Clip" } else { f }
        });
        let dur = self.find_text(resolved, &["Duration", "OutPoint"]).and_then(|s| parse_premiere_tick(s, default_rate));
        let kind = kind_from_ext(&resolved_path);
        let (w, h) = self.find_text(resolved, &["FrameRect"]).and_then(parse_frame_rect).unwrap_or((1920, 1080));
        let spec = MediaSpec {
            duration: dur,
            video: if kind == MediaKind::AudioOnly { None } else { Some((w, h, default_rate)) },
            audio: if kind == MediaKind::Still { None } else { Some((48_000, 2)) },
            start_tc: None,
            kind: Some(kind),
        };
        let item_id = self.b.file_media(&resolved_path, name, &resolved_path, &spec, None);
        self.media_by_node.insert(key_id, item_id);
        Some(item_id)
    }

    fn parse_sequence(&mut self, seq_node: Node<'a, 'i>) {
        let seq_node = self.resolve(seq_node);
        let name = child_text(seq_node, "Name").or_else(|| seq_node.attribute("Name")).filter(|s| !s.is_empty()).unwrap_or("Sequence 01");

        let rate = self.find_text(seq_node, &["FrameRate", "Timebase"]).and_then(parse_premiere_rate).unwrap_or(FrameRate::FPS_24);

        let (width, height) = self
            .find_text(seq_node, &["FrameRect"])
            .and_then(parse_frame_rect)
            .or_else(|| {
                let w = self.find_text(seq_node, &["Width"])?.parse::<u32>().ok()?;
                let h = self.find_text(seq_node, &["Height"])?.parse::<u32>().ok()?;
                Some((w, h))
            })
            .unwrap_or((1920, 1080));

        let settings = settings_for(rate, width, height, false);
        let seq_id = self.b.reserve_sequence(name, settings.clone(), None);
        self.b.top.push(seq_id);
        let mut seq = empty_sequence(settings);

        let mut video_track_nodes = Vec::new();
        let mut audio_track_nodes = Vec::new();
        let mut caption_track_nodes = Vec::new();

        self.collect_sequence_tracks(seq_node, &mut video_track_nodes, &mut audio_track_nodes, &mut caption_track_nodes);
        if video_track_nodes.is_empty() && audio_track_nodes.is_empty() && caption_track_nodes.is_empty() {
            self.report.warn(format!("sequence \"{name}\": no tracks were recognised; it may be imported empty"));
        }

        for (idx, vt_node) in video_track_nodes.into_iter().enumerate() {
            self.populate_track(&mut seq, TrackKind::Video, idx, vt_node, rate);
        }
        for (idx, at_node) in audio_track_nodes.into_iter().enumerate() {
            self.populate_track(&mut seq, TrackKind::Audio, idx, at_node, rate);
        }
        for (idx, ct_node) in caption_track_nodes.into_iter().enumerate() {
            self.populate_caption_track(&mut seq, idx, ct_node, rate);
        }

        if seq.video_tracks.is_empty() && seq.audio_tracks.is_empty() {
            for item_node in elements(seq_node) {
                let resolved = self.resolve(item_node);
                match resolved.tag_name().name() {
                    "VideoClipTrackItem" => self.place_track_item(&mut seq, TrackKind::Video, 0, resolved, rate),
                    "AudioClipTrackItem" => self.place_track_item(&mut seq, TrackKind::Audio, 0, resolved, rate),
                    _ => {}
                }
            }
        }

        if seq.video_tracks.is_empty() {
            let _ = self.b.ensure_tracks(&mut seq, TrackKind::Video, 1);
        }
        if seq.audio_tracks.is_empty() {
            let _ = self.b.ensure_tracks(&mut seq, TrackKind::Audio, 1);
        }

        for m_node in seq_node.descendants().filter(|n| n.is_element() && n.tag_name().name() == "Marker") {
            let rm = self.resolve(m_node);
            if let Some(start) = child_text(rm, "Start").or_else(|| child_text(rm, "InPoint")).and_then(|s| parse_premiere_tick(s, rate)) {
                let m_name = child_text(rm, "Name").unwrap_or("").to_string();
                let comment = child_text(rm, "Comment").unwrap_or("").to_string();
                let dur = child_text(rm, "Duration").and_then(|s| parse_premiere_tick(s, rate)).unwrap_or(Tick::ZERO);
                seq.markers.push(Marker {
                    id: MarkerId(self.b.alloc()),
                    start,
                    duration: dur,
                    name: m_name,
                    comment,
                    kind: MarkerKind::Comment,
                    color: Label::Cerulean,
                    author: Default::default(),
                    custom_color: Default::default(),
                    url: Default::default(),
                });
            }
        }

        self.b.put_sequence(seq_id, seq);
    }

    fn collect_sequence_tracks(&self, seq_node: Node<'a, 'i>, video: &mut Vec<Node<'a, 'i>>, audio: &mut Vec<Node<'a, 'i>>, captions: &mut Vec<Node<'a, 'i>>) {
        for c in elements(seq_node) {
            let rc = self.resolve(c);
            match rc.tag_name().name() {
                "VideoTrackGroup" => self.collect_tracks_in_group(rc, TrackGroupKind::Video, video, audio, captions),
                "AudioTrackGroup" => self.collect_tracks_in_group(rc, TrackGroupKind::Audio, video, audio, captions),
                "CaptionTrackGroup" | "CaptioningMetadata" => {
                    self.collect_tracks_in_group(rc, TrackGroupKind::Caption, video, audio, captions);
                }
                "TrackGroup" => {
                    for g in elements(rc) {
                        let rg = self.resolve(g);
                        match rg.tag_name().name() {
                            "VideoTrackGroup" => self.collect_tracks_in_group(rg, TrackGroupKind::Video, video, audio, captions),
                            "AudioTrackGroup" => self.collect_tracks_in_group(rg, TrackGroupKind::Audio, video, audio, captions),
                            "CaptionTrackGroup" | "CaptioningMetadata" => {
                                self.collect_tracks_in_group(rg, TrackGroupKind::Caption, video, audio, captions);
                            }
                            "VideoClipTrack" => video.push(rg),
                            "AudioClipTrack" => audio.push(rg),
                            "CaptionTrack" | "CaptionStream" => captions.push(rg),
                            "Track" => match classify_track(rg) {
                                TrackGroupKind::Video => video.push(rg),
                                TrackGroupKind::Audio => audio.push(rg),
                                TrackGroupKind::Caption => captions.push(rg),
                            },
                            _ => {}
                        }
                    }
                }
                "VideoClipTrack" | "VideoTrack" => video.push(rc),
                "AudioClipTrack" | "AudioTrack" => audio.push(rc),
                "CaptionTrack" | "CaptionStream" => captions.push(rc),
                "Tracks" => {
                    for t in elements(rc) {
                        let rt = self.resolve(t);
                        match rt.tag_name().name() {
                            "VideoClipTrack" | "VideoTrack" => video.push(rt),
                            "AudioClipTrack" | "AudioTrack" => audio.push(rt),
                            "CaptionTrack" | "CaptionStream" => captions.push(rt),
                            _ => match classify_track(rt) {
                                TrackGroupKind::Video => video.push(rt),
                                TrackGroupKind::Audio => audio.push(rt),
                                TrackGroupKind::Caption => captions.push(rt),
                            },
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn collect_tracks_in_group(
        &self,
        group: Node<'a, 'i>,
        default_kind: TrackGroupKind,
        video: &mut Vec<Node<'a, 'i>>,
        audio: &mut Vec<Node<'a, 'i>>,
        captions: &mut Vec<Node<'a, 'i>>,
    ) {
        let mut found_subtrack = false;
        for c in elements(group) {
            let rc = self.resolve(c);
            match rc.tag_name().name() {
                "Tracks" | "TrackGroup" => {
                    for t in elements(rc) {
                        let rt = self.resolve(t);
                        found_subtrack = true;
                        match default_kind {
                            TrackGroupKind::Video => video.push(rt),
                            TrackGroupKind::Audio => audio.push(rt),
                            TrackGroupKind::Caption => captions.push(rt),
                        }
                    }
                }
                "VideoClipTrack" | "VideoTrack" => {
                    found_subtrack = true;
                    video.push(rc);
                }
                "AudioClipTrack" | "AudioTrack" => {
                    found_subtrack = true;
                    audio.push(rc);
                }
                "CaptionTrack" | "CaptionStream" => {
                    found_subtrack = true;
                    captions.push(rc);
                }
                "Track" => {
                    found_subtrack = true;
                    match default_kind {
                        TrackGroupKind::Video => video.push(rc),
                        TrackGroupKind::Audio => audio.push(rc),
                        TrackGroupKind::Caption => captions.push(rc),
                    }
                }
                _ => {}
            }
        }
        if !found_subtrack {
            match default_kind {
                TrackGroupKind::Video => video.push(group),
                TrackGroupKind::Audio => audio.push(group),
                TrackGroupKind::Caption => captions.push(group),
            }
        }
    }

    fn populate_track(&mut self, seq: &mut Sequence, kind: TrackKind, track_idx: usize, track_node: Node<'a, 'i>, rate: FrameRate) {
        if !self.b.ensure_tracks(seq, kind, track_idx + 1) {
            return;
        }
        if let Some(t_name) = child_text(track_node, "Name").or_else(|| track_node.attribute("Name"))
            && !t_name.is_empty()
            && let Some(track) = seq.tracks_mut(kind).get_mut(track_idx)
        {
            track.name = t_name.to_string();
        }
        let mut item_nodes = Vec::new();
        for c in elements(track_node) {
            let rc = self.resolve(c);
            match rc.tag_name().name() {
                "TrackItems" | "ClipItems" | "Items" => {
                    for it in elements(rc) {
                        item_nodes.push(self.resolve(it));
                    }
                }
                "VideoClipTrackItem" | "AudioClipTrackItem" | "ClipTrackItem" | "TrackItem" | "ClipItem" => {
                    item_nodes.push(rc);
                }
                _ => {}
            }
        }
        for item_node in item_nodes {
            self.place_track_item(seq, kind, track_idx, item_node, rate);
        }
    }

    fn place_track_item(&mut self, seq: &mut Sequence, kind: TrackKind, track_idx: usize, item_node: Node<'a, 'i>, rate: FrameRate) {
        let item_node = self.resolve(item_node);
        let start = self.find_text(item_node, &["Start", "TimelineIn"]).and_then(|s| parse_premiere_tick(s, rate)).unwrap_or(Tick::ZERO);
        let end = self.find_text(item_node, &["End", "TimelineOut"]).and_then(|s| parse_premiere_tick(s, rate));
        let duration = match end {
            Some(e) if e > start => e - start,
            _ => self.find_text(item_node, &["Duration"]).and_then(|s| parse_premiere_tick(s, rate)).unwrap_or_else(|| Tick::from_seconds_f64(5.0)),
        };
        let source_in = self.find_text(item_node, &["InPoint", "SourceIn", "In"]).and_then(|s| parse_premiere_tick(s, rate)).unwrap_or(Tick::ZERO);

        let subclip = self
            .resolve_child(item_node, "SubClip")
            .or_else(|| self.resolve_child(item_node, "Clip"))
            .or_else(|| self.resolve_child(item_node, "MasterClip"))
            .unwrap_or(item_node);

        let media_id = self.ensure_media_for_node(subclip, rate).or_else(|| self.ensure_media_for_node(item_node, rate)).unwrap_or_else(|| {
            let fallback_name = self.find_text(item_node, &["Name", "Title"]).unwrap_or("Offline Clip");
            self.report.warn(format!("clip \"{fallback_name}\" has no media file path; imported offline"));
            let spec = MediaSpec {
                duration: Some(source_in + duration),
                video: if kind == TrackKind::Video { Some((seq.settings.width, seq.settings.height, rate)) } else { None },
                audio: if kind == TrackKind::Audio { Some((48_000, 2)) } else { None },
                start_tc: None,
                kind: Some(if kind == TrackKind::Audio { MediaKind::AudioOnly } else { MediaKind::Movie }),
            };
            // As the other importers do, the name is kept only as a relink hint for an item
            // marked offline, under its own key so it never merges with a real file's item.
            let hint = resolve_path(fallback_name, self.opts.base_dir.as_deref());
            let id = self.b.file_media(&format!("offline:{fallback_name}"), fallback_name, &hint, &spec, None);
            if let Some(m) = self.b.p.item_mut(id).and_then(|i| i.as_media_mut()) {
                m.offline = true;
            }
            id
        });

        let clip_name = self
            .find_text(item_node, &["Name", "Title", "ClipName"])
            .or_else(|| self.find_text(subclip, &["Name", "Title", "ClipName"]))
            .map(str::to_string)
            .unwrap_or_else(|| self.b.p.item(media_id).map(|i| i.name.clone()).unwrap_or_else(|| "Clip".into()));

        let ti = self.b.clip(media_id, kind, &clip_name, start, duration, source_in);
        let _ = self.b.place(seq, kind, track_idx, ti);
    }

    fn populate_caption_track(&mut self, seq: &mut Sequence, idx: usize, ct_node: Node<'a, 'i>, rate: FrameRate) {
        let ct_node = self.resolve(ct_node);
        let name = child_text(ct_node, "Name")
            .or_else(|| ct_node.attribute("Name"))
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Subtitle {}", idx + 1));
        let format = match child_text(ct_node, "Format").or_else(|| ct_node.attribute("Format")).unwrap_or("Subtitle") {
            s if s.eq_ignore_ascii_case("cea608") || s.eq_ignore_ascii_case("608") => CaptionFormat::Cea608,
            s if s.eq_ignore_ascii_case("cea708") || s.eq_ignore_ascii_case("708") => CaptionFormat::Cea708,
            s if s.eq_ignore_ascii_case("teletext") => CaptionFormat::Teletext,
            _ => CaptionFormat::Subtitle,
        };
        let mut captions = Vec::new();
        for c_node in
            ct_node.descendants().filter(|n| n.is_element() && matches!(n.tag_name().name(), "Caption" | "CaptionItem" | "Subtitle" | "CaptionTrackItem"))
        {
            let rc = self.resolve(c_node);
            let start = self.find_text(rc, &["Start", "InPoint", "TimelineIn"]).and_then(|s| parse_premiere_tick(s, rate)).unwrap_or(Tick::ZERO);
            let end = self.find_text(rc, &["End", "OutPoint", "TimelineOut"]).and_then(|s| parse_premiere_tick(s, rate));
            let duration = match end {
                Some(e) if e > start => e - start,
                _ => self.find_text(rc, &["Duration"]).and_then(|s| parse_premiere_tick(s, rate)).unwrap_or_else(|| Tick::from_seconds_f64(2.0)),
            };
            let caption_text = self.find_text(rc, &["Text", "CaptionText", "Value"]).unwrap_or_else(|| text(rc)).to_string();
            if !caption_text.is_empty() {
                captions.push(Caption {
                    id: ClipId(self.b.alloc()),
                    start,
                    duration,
                    text: caption_text,
                    speaker: None,
                    cue_id: None,
                    settings: String::new(),
                });
            }
        }
        captions.sort_by_key(|c| c.start);
        let mut ct = CaptionTrack::new(TrackId(self.b.alloc()), name, format);
        ct.captions = captions;
        seq.caption_tracks.push(ct);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrackGroupKind {
    Video,
    Audio,
    Caption,
}

fn classify_track(n: Node<'_, '_>) -> TrackGroupKind {
    let name = n.tag_name().name();
    if name.contains("Caption") {
        return TrackGroupKind::Caption;
    }
    if name.contains("Audio") {
        return TrackGroupKind::Audio;
    }
    if name.contains("Video") {
        return TrackGroupKind::Video;
    }
    for d in n.descendants().filter(|x| x.is_element()) {
        match d.tag_name().name() {
            "AudioClipTrackItem" => return TrackGroupKind::Audio,
            "Caption" | "CaptionItem" | "CaptionTrackItem" => return TrackGroupKind::Caption,
            "VideoClipTrackItem" => return TrackGroupKind::Video,
            _ => {}
        }
    }
    TrackGroupKind::Video
}

/// Convert a Premiere Pro time value string into [`Tick`].
/// Premiere Pro's internal tick unit is `254_016_000_000` ticks/sec, identical to [`TICKS_PER_SECOND`].
pub fn parse_premiere_tick(raw: &str, rate: FrameRate) -> Option<Tick> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(v) = s.parse::<i64>() {
        if v < 0 {
            return Some(Tick::ZERO);
        }
        if v == 0 {
            return Some(Tick::ZERO);
        }
        if v >= 1_000_000 {
            return Some(Tick(v));
        }
        return Some(rate.tick_of(v));
    }
    if let Ok(sec) = s.parse::<f64>()
        && sec.is_finite()
        && sec >= 0.0
    {
        return Some(Tick::from_seconds_f64(sec));
    }
    None
}

/// Convert a Premiere Pro `<FrameRate>` value (either ticks-per-frame like `10584000000` for 24fps
/// or fps like `29.97` / `24`) into a [`FrameRate`].
pub fn parse_premiere_rate(raw: &str) -> Option<FrameRate> {
    let s = raw.trim();
    if let Ok(v) = s.parse::<i64>()
        && v > 0
    {
        if v >= 1_000_000 {
            let fps = (TICKS_PER_SECOND as f64) / (v as f64);
            return Some(rate_from_f64(fps));
        }
        return Some(rate_from_f64(v as f64));
    }
    if let Ok(f) = s.parse::<f64>()
        && f > 0.0
    {
        return Some(rate_from_f64(f));
    }
    None
}

fn parse_frame_rect(raw: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.len() == 4 {
        let x0 = parts[0].parse::<i64>().ok()?;
        let y0 = parts[1].parse::<i64>().ok()?;
        let x1 = parts[2].parse::<i64>().ok()?;
        let y1 = parts[3].parse::<i64>().ok()?;
        let w = u32::try_from((x1 - x0).unsigned_abs()).ok()?;
        let h = u32::try_from((y1 - y0).unsigned_abs()).ok()?;
        if w > 0 && h > 0 {
            return Some((w, h));
        }
    }
    None
}

/// Import an Adobe Premiere Pro `<PremiereData>` XML document.
pub(crate) fn import(text: &str, opts: &ImportOptions, report: &mut Report) -> Result<Imported> {
    let doc = parse(text).map_err(parse_err)?;
    let root = doc.root_element();

    let project_name = root
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "Project")
        .and_then(|p| child_text(p, "Name").or_else(|| p.attribute("Name")))
        .filter(|s| !s.is_empty())
        .or_else(|| opts.name.as_deref().filter(|s| !s.is_empty()))
        .or_else(|| opts.base_dir.as_deref().map(file_stem).filter(|s| !s.is_empty()))
        .unwrap_or("Premiere Project");

    let mut by_id = HashMap::new();
    let mut by_uid = HashMap::new();
    for n in root.descendants().filter(|n| n.is_element()) {
        if let Some(id) = n.attribute("ObjectID") {
            by_id.insert(id, n);
        }
        if let Some(uid) = n.attribute("ObjectUID") {
            by_uid.insert(uid, n);
        }
    }
    let dangling = |n: &Node<'_, '_>| {
        n.attribute("ObjectRef").is_some_and(|r| !by_id.contains_key(r)) || n.attribute("ObjectURef").is_some_and(|u| !by_uid.contains_key(u))
    };
    let unresolved = root.descendants().filter(|n| n.is_element() && dangling(n)).count();
    report.info("Premiere project import is experimental: only sequences, tracks, clip timing, markers and captions are read");
    report.warn("not imported from the Premiere project: effects, transitions, speed changes, audio levels, bins and labels");
    if unresolved > 0 {
        report.warn(format!("{unresolved} object reference(s) point to objects that are not in the document"));
    }

    let mut imp = PrprojImporter { b: Builder::new(project_name), opts, report, by_id, by_uid, media_by_node: HashMap::new(), search_cut: Cell::new(false) };

    imp.register_all_media(root);

    let seq_nodes: Vec<Node<'_, '_>> = root
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "Sequence" && n.attribute("ObjectRef").is_none() && n.attribute("ObjectURef").is_none())
        .collect();

    for seq_node in seq_nodes {
        imp.parse_sequence(seq_node);
    }

    if imp.search_cut.get() {
        imp.report.warn("some properties were not found: the document's object references are too deep or too wide to follow");
    }
    if !imp.media_by_node.is_empty() {
        imp.report.info("media details the document does not give are assumed (1920×1080, 24 fps, 48 kHz stereo) until the files are linked");
    }
    if imp.b.top.is_empty() {
        imp.report.warn("the Premiere project has no sequence; an empty one was made");
        let settings = settings_for(FrameRate::FPS_24, 1920, 1080, false);
        let seq_id = imp.b.reserve_sequence("Sequence 01", settings.clone(), None);
        imp.b.top.push(seq_id);
        let mut seq = empty_sequence(settings);
        let _ = imp.b.ensure_tracks(&mut seq, TrackKind::Video, 1);
        let _ = imp.b.ensure_tracks(&mut seq, TrackKind::Audio, 1);
        imp.b.put_sequence(seq_id, seq);
    }

    Ok(imp.b.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Format, detect, import as import_bytes};

    const SAMPLE_PRPROJ_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<PremiereData Version="3">
  <Project ObjectID="1" Name="Premiere 2026 Cut">
    <Sequence ObjectRef="2"/>
  </Project>
  <Sequence ObjectID="2">
    <Name>Main Edit</Name>
    <FrameRate>10584000000</FrameRate>
    <FrameRect>0,0,1920,1080</FrameRect>
    <TrackGroup>
      <VideoTrackGroup ObjectRef="3"/>
      <AudioTrackGroup ObjectRef="4"/>
      <CaptionTrackGroup ObjectRef="5"/>
    </TrackGroup>
  </Sequence>
  <VideoTrackGroup ObjectID="3">
    <VideoClipTrack ObjectID="30">
      <Name>V1</Name>
      <TrackItems>
        <VideoClipTrackItem ObjectRef="100"/>
      </TrackItems>
    </VideoClipTrack>
  </VideoTrackGroup>
  <AudioTrackGroup ObjectID="4">
    <AudioClipTrack ObjectID="40">
      <Name>A1</Name>
      <TrackItems>
        <AudioClipTrackItem ObjectRef="101"/>
      </TrackItems>
    </AudioClipTrack>
  </AudioTrackGroup>
  <CaptionTrackGroup ObjectID="5">
    <CaptionTrack ObjectID="50" Name="Korean Subtitles" Format="Subtitle">
      <Caption ObjectID="500">
        <Start>0</Start>
        <End>508032000000</End>
        <Text>안녕하세요 프리미어 프로 프로젝트입니다</Text>
      </Caption>
    </CaptionTrack>
  </CaptionTrackGroup>
  <VideoClipTrackItem ObjectID="100">
    <Start>0</Start>
    <End>1270080000000</End>
    <InPoint>254016000000</InPoint>
    <SubClip ObjectRef="200"/>
  </VideoClipTrackItem>
  <AudioClipTrackItem ObjectID="101">
    <Start>0</Start>
    <End>1270080000000</End>
    <InPoint>254016000000</InPoint>
    <SubClip ObjectRef="200"/>
  </AudioClipTrackItem>
  <SubClip ObjectID="200">
    <Name>interview_a_cam.mp4</Name>
    <Media ObjectRef="300"/>
  </SubClip>
  <Media ObjectID="300">
    <Title>interview_a_cam.mp4</Title>
    <ActualMediaFilePath>C:/Media/interview_a_cam.mp4</ActualMediaFilePath>
    <Duration>2540160000000</Duration>
  </Media>
</PremiereData>
"#;

    /// Text compressed by zlib (the reference implementation, used as a test oracle only) with fixed
    /// Huffman codes; `GZIP_DYNAMIC_HUFFMAN` is `dynamic_text()` at level 9 with dynamic codes.
    const FIXED_TEXT: &str = r#"<PremiereData Version="3"><Project ObjectID="1"><Name>fixed huffman</Name></Project></PremiereData>"#;

    const GZIP_FIXED_HUFFMAN: &[u8] = &[
        0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x0A, 0xB3, 0x09, 0x28, 0x4A, 0xCD, 0xCD, 0x4C, 0x2D, 0x4A, 0x75, 0x49, 0x2C, 0x49, 0x54, 0x08,
        0x4B, 0x2D, 0x2A, 0xCE, 0xCC, 0xCF, 0xB3, 0x55, 0x32, 0x56, 0xB2, 0xB3, 0x09, 0x28, 0xCA, 0xCF, 0x4A, 0x4D, 0x2E, 0x51, 0xF0, 0x4F, 0x02, 0x51, 0x9E,
        0x2E, 0xB6, 0x4A, 0x86, 0x40, 0x51, 0xBF, 0xC4, 0xDC, 0x54, 0xBB, 0xB4, 0xCC, 0x8A, 0xD4, 0x14, 0x85, 0x8C, 0xD2, 0xB4, 0xB4, 0xDC, 0xC4, 0x3C, 0x1B,
        0x7D, 0xB0, 0x98, 0x8D, 0x3E, 0x54, 0x03, 0x98, 0x85, 0x30, 0xD4, 0x0E, 0x00, 0xC1, 0x73, 0x2B, 0xBF, 0x63, 0x00, 0x00, 0x00,
    ];
    const GZIP_DYNAMIC_HUFFMAN: &[u8] = &[
        0x1F, 0x8B, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x0A, 0x8D, 0x9A, 0xCD, 0x8A, 0x25, 0x45, 0x18, 0x44, 0x5F, 0x45, 0xE6, 0x05, 0xA6, 0xA2, 0xF2,
        0xF7, 0x83, 0x6B, 0xAF, 0xDC, 0x2A, 0x82, 0xE0, 0xBE, 0x17, 0x77, 0x31, 0x48, 0x2B, 0xB4, 0xFD, 0xFE, 0x4C, 0x32, 0x22, 0x68, 0x04, 0x78, 0xDC, 0xDD,
        0x0B, 0x15, 0xAB, 0x13, 0x95, 0xF5, 0xE5, 0xC9, 0x7C, 0xFC, 0xFC, 0xFE, 0x7C, 0xFB, 0xF2, 0x7C, 0x7F, 0xFE, 0xF0, 0xFA, 0xF1, 0xFA, 0xDD, 0xAF, 0xCF,
        0xF7, 0x3F, 0xBF, 0xFC, 0xF1, 0xFB, 0xF7, 0x9F, 0xDA, 0xA7, 0x97, 0xC7, 0x8F, 0xAF, 0xEF, 0xBF, 0x3D, 0xDF, 0x5F, 0x1E, 0xBF, 0x7C, 0xBC, 0xBE, 0x7F,
        0xBC, 0x5C, 0x8F, 0xCF, 0x7F, 0xFD, 0x78, 0xFC, 0xF4, 0xFA, 0xF6, 0x7C, 0x79, 0x3B, 0xFF, 0xBF, 0xFD, 0x78, 0x7C, 0xFE, 0xFB, 0xB9, 0x7F, 0x3F, 0xBF,
        0xEC, 0x79, 0xC1, 0xF3, 0xEA, 0x16, 0xB8, 0x21, 0x70, 0xCB, 0x02, 0x8D, 0x02, 0xDB, 0x02, 0x1D, 0x02, 0x6D, 0x58, 0x60, 0x40, 0xA0, 0xDF, 0x16, 0x98,
        0x14, 0x28, 0x0B, 0x2C, 0x08, 0x8C, 0x69, 0x81, 0x0D, 0x81, 0xD9, 0x2C, 0x50, 0xC4, 0xCD, 0x41, 0x0B, 0x49, 0x07, 0x6A, 0x62, 0xBD, 0x9D, 0xB5, 0x08,
        0x76, 0x39, 0x6C, 0x11, 0xED, 0x72, 0xDA, 0x22, 0xDC, 0xBA, 0x9C, 0xB7, 0x08, 0xB8, 0xE4, 0xC4, 0x35, 0x31, 0xE2, 0xCC, 0x45, 0xD0, 0x75, 0x3B, 0x75,
        0x11, 0x76, 0x35, 0xE7, 0xAE, 0xC2, 0x17, 0xD0, 0xC9, 0xDF, 0x17, 0x46, 0x1C, 0xFD, 0x8D, 0xAF, 0xF9, 0x88, 0xF7, 0x9C, 0xD8, 0x6B, 0x3A, 0xFC, 0xBB,
        0x61, 0xC4, 0xE9, 0xDF, 0x48, 0x7F, 0x39, 0xFD, 0x1B, 0xE9, 0x6F, 0xA7, 0x7F, 0x23, 0xFD, 0xED, 0xF4, 0x6F, 0xA4, 0x5F, 0x4E, 0xFF, 0x26, 0xFA, 0xF7,
        0xE5, 0xF4, 0xEF, 0xC2, 0xD5, 0xD4, 0xE9, 0xB7, 0x0B, 0x23, 0x4E, 0xBF, 0x11, 0xFD, 0xFB, 0x76, 0xFA, 0x0D, 0x97, 0xF9, 0x16, 0xEB, 0x3C, 0x2E, 0xF4,
        0xCD, 0xE9, 0x37, 0xA2, 0x7F, 0x77, 0xA7, 0xDF, 0x88, 0xFE, 0x3D, 0x9C, 0x7E, 0x9B, 0x18, 0x71, 0xFA, 0x8D, 0xE8, 0xDF, 0xD3, 0xE9, 0x37, 0xA4, 0xBF,
        0x9C, 0x7E, 0x43, 0xFA, 0xDB, 0xE9, 0x77, 0xA4, 0xBF, 0x9D, 0x7E, 0x47, 0xFA, 0xE5, 0xF4, 0x3B, 0xD1, 0x6F, 0x97, 0xD3, 0xEF, 0x0D, 0x23, 0xF1, 0x9D,
        0xC7, 0x0F, 0xBD, 0x9C, 0x7E, 0x27, 0xFA, 0xED, 0x76, 0xFA, 0x7D, 0x62, 0xC4, 0xE9, 0x77, 0xA2, 0xDF, 0x9A, 0xD3, 0xEF, 0x44, 0xBF, 0x75, 0xA7, 0xDF,
        0x0B, 0xE7, 0x1C, 0xA7, 0x3F, 0x2E, 0x8C, 0x38, 0xFD, 0x41, 0xF4, 0xDB, 0x74, 0xFA, 0x03, 0xE9, 0x2F, 0xA7, 0x3F, 0x90, 0xFE, 0x72, 0xFA, 0x03, 0xE9,
        0xEF, 0x98, 0xF3, 0x90, 0x7E, 0x39, 0xFD, 0x81, 0xF4, 0xCB, 0xE9, 0x0F, 0xA2, 0xDF, 0x2F, 0xA7, 0x3F, 0x88, 0x7E, 0x97, 0xD3, 0x1F, 0x85, 0x43, 0xAB,
        0xD3, 0x9F, 0x17, 0x46, 0x9C, 0xFE, 0x24, 0xFA, 0xBD, 0x39, 0xFD, 0x49, 0xF4, 0x7B, 0x77, 0xFA, 0xB3, 0x61, 0xC4, 0xE9, 0x4F, 0xA2, 0xDF, 0x87, 0xD3,
        0x9F, 0x38, 0xE6, 0xCF, 0x98, 0xF3, 0x71, 0xD0, 0x9F, 0x4E, 0x7F, 0x22, 0xFD, 0xE5, 0xF4, 0x27, 0xD2, 0xDF, 0x4E, 0x7F, 0x22, 0xFD, 0x72, 0xFA, 0x0B,
        0xE9, 0x97, 0xD3, 0x5F, 0x44, 0x7F, 0x5C, 0x4E, 0x7F, 0x11, 0xFD, 0x21, 0xA7, 0xBF, 0x1A, 0x46, 0x9C, 0xFE, 0x22, 0xFA, 0xE3, 0x76, 0xFA, 0x8B, 0xE8,
        0x8F, 0xE6, 0xF4, 0xD7, 0xC4, 0x48, 0xEC, 0xF3, 0x70, 0xA3, 0xD7, 0x9D, 0xFE, 0x22, 0xFA, 0x63, 0x38, 0xFD, 0x55, 0xB8, 0x9D, 0x74, 0xFA, 0xFB, 0xC2,
        0x88, 0xD3, 0xDF, 0x48, 0x7F, 0x39, 0xFD, 0x8D, 0xF4, 0xB7, 0xD3, 0xDF, 0x48, 0x7F, 0x3B, 0xFD, 0x8D, 0xF4, 0xCB, 0xE9, 0x6F, 0xA2, 0x3F, 0x2F, 0xA7,
        0xBF, 0x27, 0x46, 0x9C, 0xFE, 0x26, 0xFA, 0x53, 0xB1, 0xCF, 0xC7, 0x8D, 0xFE, 0xED, 0xF4, 0x77, 0xA1, 0x1B, 0x70, 0xFA, 0x75, 0x61, 0xC4, 0xE9, 0x17,
        0xD1, 0x9F, 0xDD, 0xE9, 0x17, 0xD1, 0x9F, 0xC3, 0xE9, 0x57, 0xC3, 0x88, 0xD3, 0x2F, 0xA2, 0x3F, 0xA7, 0xD3, 0x2F, 0xA4, 0xBF, 0x9C, 0x7E, 0x21, 0xFD,
        0xE5, 0xF4, 0x0B, 0xE9, 0x6F, 0xA7, 0x5F, 0x48, 0xBF, 0xC2, 0xF3, 0xB0, 0xE8, 0x49, 0xD3, 0x83, 0xAA, 0xE7, 0x0A, 0xD7, 0x73, 0x11, 0xFF, 0x15, 0x66,
        0x4F, 0x17, 0x15, 0x60, 0x85, 0xDC, 0xD3, 0xD5, 0x30, 0x13, 0xC6, 0xE7, 0xA2, 0x0A, 0xAC, 0x50, 0x7C, 0xDF, 0x24, 0xD0, 0x7F, 0x67, 0xC2, 0xF2, 0xE9,
        0x9A, 0x98, 0x09, 0xE9, 0x73, 0x51, 0x0B, 0x56, 0xB8, 0x3E, 0x5D, 0x54, 0x83, 0x15, 0xBA, 0x4F, 0x17, 0xF6, 0x20, 0x8D, 0xDF, 0xFF, 0x50, 0x7E, 0xE9,
        0xFC, 0xB0, 0x07, 0x69, 0xFD, 0x50, 0xFB, 0xAD, 0xF4, 0x7E, 0x28, 0xFE, 0x56, 0x9A, 0x3F, 0x54, 0x7F, 0x3B, 0xD5, 0x1F, 0xBA, 0xBF, 0x9D, 0xEE, 0x0F,
        0xE5, 0xDF, 0x4E, 0xF9, 0x87, 0xF6, 0x6F, 0xA7, 0xFD, 0x43, 0xFD, 0xB7, 0x53, 0xFF, 0xA1, 0xFF, 0xDB, 0xE1, 0xFF, 0x84, 0x02, 0x70, 0x87, 0x00, 0x14,
        0x1A, 0xC0, 0x3D, 0xD2, 0xFE, 0x52, 0x0F, 0x76, 0x28, 0x40, 0xA1, 0x03, 0xDC, 0xE1, 0x00, 0x85, 0x12, 0x70, 0x87, 0x04, 0x14, 0x5A, 0xC0, 0x1D, 0x16,
        0x50, 0xA8, 0x01, 0x77, 0x68, 0x40, 0xA1, 0x07, 0xDC, 0xE1, 0x01, 0x85, 0x22, 0xB0, 0x42, 0x04, 0x0A, 0x4D, 0x60, 0x85, 0x09, 0x14, 0xAA, 0xC0, 0x0A,
        0x15, 0x28, 0x74, 0x81, 0x15, 0x2E, 0x50, 0x28, 0x03, 0xAB, 0xE5, 0x39, 0x00, 0x1E, 0x04, 0x84, 0x0D, 0x14, 0xEA, 0xC0, 0x0A, 0x1D, 0x28, 0xF4, 0x81,
        0x15, 0x3E, 0x50, 0x28, 0x04, 0x2B, 0x84, 0xA0, 0xD0, 0x08, 0x56, 0x18, 0x41, 0xA1, 0x12, 0xAC, 0x50, 0x82, 0x42, 0x27, 0x58, 0xE1, 0x04, 0x85, 0x52,
        0xB0, 0x42, 0x0A, 0x0A, 0xAD, 0x60, 0x85, 0x15, 0x14, 0x6A, 0xC1, 0x33, 0xA9, 0x44, 0x11, 0x50, 0x0C, 0x9E, 0x50, 0x9E, 0x09, 0xF1, 0xA1, 0x50, 0xB8,
        0x41, 0xA1, 0x1C, 0x3C, 0x03, 0x4E, 0x74, 0x01, 0xF5, 0xE0, 0x09, 0x45, 0x19, 0x50, 0x10, 0x9E, 0xB1, 0x28, 0xDA, 0x80, 0x8A, 0xF0, 0xCC, 0x45, 0x51,
        0x07, 0x94, 0x84, 0x67, 0x30, 0x8A, 0x3E, 0xA0, 0x26, 0x3C, 0xA1, 0x28, 0x04, 0x8A, 0xC2, 0x33, 0x4E, 0x45, 0x23, 0x06, 0x37, 0x22, 0x5C, 0xA1, 0x06,
        0x37, 0x22, 0x6C, 0xA1, 0x06, 0x37, 0x62, 0xE7, 0x39, 0x21, 0x37, 0x22, 0x8C, 0xA1, 0x06, 0x37, 0x22, 0x9C, 0xA1, 0x50, 0x1A, 0x4A, 0x61, 0x0D, 0x85,
        0xDA, 0x50, 0x0A, 0x6F, 0x28, 0x14, 0x87, 0x52, 0x98, 0x43, 0xA1, 0x3A, 0x94, 0xC2, 0x1D, 0x0A, 0xE5, 0xA1, 0x14, 0xF6, 0x50, 0xA8, 0x0F, 0xA5, 0xF0,
        0x87, 0x42, 0x81, 0x28, 0x85, 0x41, 0x14, 0x2A, 0x44, 0x29, 0x1C, 0xA2, 0x26, 0x1F, 0x1D, 0xCF, 0x3C, 0x3B, 0xE6, 0xC3, 0xE3, 0xF0, 0x88, 0x9A, 0xDC,
        0x88, 0x30, 0x89, 0x9A, 0xDC, 0x88, 0x70, 0x89, 0x9A, 0xDC, 0x88, 0xB0, 0x89, 0x5A, 0xDC, 0x88, 0xF0, 0x89, 0x42, 0xA1, 0x78, 0xAA, 0x17, 0x8D, 0x40,
        0xA5, 0x78, 0x66, 0xD3, 0x68, 0x04, 0x4A, 0xC5, 0x13, 0x8A, 0x46, 0xA0, 0x56, 0x3C, 0x13, 0x6D, 0x34, 0x02, 0xC5, 0xE2, 0x19, 0x69, 0xA3, 0x11, 0xA8,
        0x16, 0x4F, 0x28, 0xEF, 0x13, 0xF0, 0x85, 0x82, 0xB0, 0x8B, 0x42, 0xBD, 0x78, 0x26, 0xE1, 0x68, 0x04, 0x0A, 0xC6, 0x33, 0x0A, 0x47, 0x23, 0x50, 0x31,
        0x9E, 0x50, 0x34, 0x62, 0x73, 0x23, 0xC2, 0x32, 0x6A, 0x73, 0x23, 0xC2, 0x33, 0x6A, 0x73, 0x23, 0xC2, 0x34, 0x6A, 0x73, 0x23, 0xC2, 0x35, 0x0A, 0x65,
        0xE3, 0x19, 0xBC, 0xA3, 0x11, 0xA8, 0x1B, 0x4F, 0x28, 0x1A, 0x81, 0xC2, 0xF1, 0x8C, 0xEB, 0x79, 0xC7, 0x84, 0x2F, 0x99, 0x84, 0x73, 0x14, 0x4A, 0xC7,
        0x33, 0xB0, 0x47, 0x23, 0x50, 0x3B, 0x9E, 0x50, 0x34, 0x02, 0xC5, 0xE3, 0xF9, 0x68, 0x44, 0x23, 0x50, 0x3D, 0x9E, 0x39, 0x3F, 0x1A, 0x81, 0xF2, 0xF1,
        0x84, 0xA2, 0x11, 0xA8, 0x1F, 0xCF, 0xEE, 0x20, 0x1A, 0x51, 0xDC, 0x88, 0x30, 0x90, 0x2A, 0x6E, 0x44, 0x38, 0x48, 0x15, 0x37, 0x22, 0x2C, 0xA4, 0x8A,
        0x1B, 0x51, 0x79, 0xEF, 0x28, 0x1B, 0xF1, 0xF9, 0x9F, 0x97, 0x0E, 0x5F, 0xBE, 0x02, 0xD8, 0xA9, 0x66, 0xED, 0x83, 0x28, 0x00, 0x00,
    ];

    fn dynamic_text() -> String {
        let markers: String = (0..200).map(|i| format!("<Marker><Start>{}</Start><Name>m{i}</Name></Marker>", i * 7)).collect();
        format!(r#"<PremiereData Version="3">{markers}</PremiereData>"#)
    }

    fn has(report: &Report, part: &str) -> bool {
        report.entries.iter().any(|e| e.message.contains(part))
    }

    #[test]
    fn gzip_compressed_prproj_roundtrip_and_import() {
        let gz = gzip_compress_stored(SAMPLE_PRPROJ_XML.as_bytes());
        assert!(is_gzip(&gz));
        assert_eq!(detect(&gz, Some("prproj")), Some(Format::Fcp7Xml));

        let decompressed = maybe_decompress(&gz).unwrap();
        assert!(is_premiere_data(&decompressed));

        let (imported, report) = import_bytes(&gz, Format::Fcp7Xml, None).unwrap();
        assert_eq!(imported.sequences.len(), 1);
        let seq = imported.project.sequence(imported.sequences[0]).unwrap();
        assert_eq!(seq.settings.frame_rate, FrameRate::FPS_24);
        assert_eq!(seq.settings.width, 1920);
        assert_eq!(seq.settings.height, 1080);

        // Start = 0, End = 1_270_080_000_000 (5 s), InPoint = 254_016_000_000 (1 s)
        assert_eq!(seq.video_tracks[0].items.len(), 1);
        let vclip = &seq.video_tracks[0].items[0];
        assert_eq!(vclip.name, "interview_a_cam.mp4");
        assert_eq!(vclip.start, Tick::ZERO);
        assert_eq!(vclip.duration, Tick::from_seconds_f64(5.0));
        assert_eq!(vclip.source_in, Tick::from_seconds_f64(1.0));

        assert_eq!(seq.audio_tracks[0].items.len(), 1);
        assert_eq!(seq.caption_tracks.len(), 1);
        assert_eq!(seq.caption_tracks[0].name, "Korean Subtitles");
        assert_eq!(seq.caption_tracks[0].captions.len(), 1);
        assert_eq!(seq.caption_tracks[0].captions[0].duration, Tick::from_seconds_f64(2.0));
        assert_eq!(seq.caption_tracks[0].captions[0].text, "안녕하세요 프리미어 프로 프로젝트입니다");
        // Captions are not a transcript: no word timings are made up from them.
        assert!(imported.project.transcripts.is_empty());
        // What is not read is said in the report.
        assert!(has(&report, "experimental"), "{report:?}");
        assert!(has(&report, "not imported"), "{report:?}");
    }

    #[test]
    fn only_a_prproj_name_makes_a_gzip_document_a_premiere_project() {
        let gz = gzip_compress_stored(SAMPLE_PRPROJ_XML.as_bytes());
        assert_eq!(detect(&gz, Some("prproj")), Some(Format::Fcp7Xml));
        assert_eq!(detect(&gz, Some(".PRPROJ")), Some(Format::Fcp7Xml));
        assert_eq!(detect(&gz, None), None);
        assert_eq!(detect(&gz, Some("xml")), None);
        assert_eq!(detect(&gz, Some("gz")), None);
    }

    #[test]
    fn fixed_and_dynamic_huffman_streams_inflate_and_check_their_trailer() {
        // the first block's BTYPE (bits 1-2 after the 10-byte header): 1 = fixed, 2 = dynamic codes
        assert_eq!((GZIP_FIXED_HUFFMAN[10] >> 1) & 3, 1);
        assert_eq!((GZIP_DYNAMIC_HUFFMAN[10] >> 1) & 3, 2);
        assert_eq!(maybe_decompress(GZIP_FIXED_HUFFMAN).unwrap(), FIXED_TEXT);
        assert_eq!(maybe_decompress(GZIP_DYNAMIC_HUFFMAN).unwrap(), dynamic_text());
        // our stored-block writer round-trips too, empty input included
        for data in [&b""[..], b"x", FIXED_TEXT.as_bytes()] {
            assert_eq!(gunzip(&gzip_compress_stored(data)).unwrap(), data);
        }
    }

    #[test]
    fn a_damaged_gzip_trailer_is_an_error() {
        for fixture in [GZIP_FIXED_HUFFMAN, GZIP_DYNAMIC_HUFFMAN] {
            let n = fixture.len();
            let mut crc = fixture.to_vec();
            crc[n - 8] ^= 0x01;
            assert!(maybe_decompress(&crc).unwrap_err().to_string().contains("CRC-32"));
            let mut size = fixture.to_vec();
            size[n - 4] ^= 0x01;
            assert!(maybe_decompress(&size).unwrap_err().to_string().contains("size mismatch"));
            assert!(maybe_decompress(&fixture[..n - 3]).unwrap_err().to_string().contains("trailer"));
        }
        assert!(maybe_decompress(&[0x1F, 0x8B, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap_err().to_string().contains("gzip"));
    }

    /// Mutation fuzzing (AGENTS.md §0.8): every truncation and single-bit flips in every byte of
    /// real compressed streams are an error or a result, never a panic.
    #[test]
    fn truncated_and_bit_flipped_gzip_never_panics() {
        for fixture in [GZIP_FIXED_HUFFMAN, GZIP_DYNAMIC_HUFFMAN] {
            for len in 0..fixture.len() {
                let cut = &fixture[..len];
                assert!(std::panic::catch_unwind(|| maybe_decompress(cut)).is_ok(), "truncated to {len}");
                // from the two magic bytes on, a cut stream is a gzip stream missing its end
                if len >= 2 {
                    assert!(maybe_decompress(cut).is_err(), "a truncated stream must not decode (length {len})");
                }
            }
            for i in 0..fixture.len() {
                for bit in [0u8, 3, 7] {
                    let mut m = fixture.to_vec();
                    m[i] ^= 1 << bit;
                    assert!(std::panic::catch_unwind(|| maybe_decompress(&m)).is_ok(), "byte {i} bit {bit}");
                }
            }
        }
    }

    /// The whole importer, on damaged documents: errors or partial imports, never a panic.
    #[test]
    fn mutated_premiere_documents_never_panic() {
        let gz = gzip_compress_stored(SAMPLE_PRPROJ_XML.as_bytes());
        for i in (0..gz.len()).step_by(5) {
            let mut m = gz.clone();
            m[i] ^= 0x20;
            assert!(std::panic::catch_unwind(|| import_bytes(&m, Format::Fcp7Xml, None)).is_ok(), "byte {i}");
        }
        let xml = SAMPLE_PRPROJ_XML;
        for len in (0..xml.len()).step_by(7) {
            let cut = &xml.as_bytes()[..xml.floor_char_boundary(len)];
            assert!(std::panic::catch_unwind(|| import_bytes(cut, Format::Fcp7Xml, None)).is_ok(), "truncated to {len}");
        }
        for i in (0..xml.len()).step_by(11) {
            let mut m = xml.as_bytes().to_vec();
            m[i] ^= 0x01;
            assert!(std::panic::catch_unwind(|| import_bytes(&m, Format::Fcp7Xml, None)).is_ok(), "XML byte {i}");
        }
    }

    /// A reference cycle that fans out (each object refers back to the sequence many times) made
    /// the property search visit 64^6 objects; it now visits each object once, within a budget.
    #[test]
    fn cyclic_and_wide_object_references_stay_cheap() {
        let refs: String = (0..64).map(|_| r#"<Clip ObjectRef="1"/>"#).collect();
        let xml = format!(r#"<PremiereData Version="3"><Sequence ObjectID="1"><Name>Loop</Name>{refs}</Sequence><Thing ObjectRef="404"/></PremiereData>"#);
        let t0 = std::time::Instant::now();
        let (imported, report) = import_bytes(xml.as_bytes(), Format::Fcp7Xml, None).unwrap();
        assert!(t0.elapsed() < std::time::Duration::from_secs(10), "took {:?}", t0.elapsed());
        assert_eq!(imported.sequences.len(), 1);
        assert!(has(&report, "no tracks were recognised"), "{report:?}");
        assert!(has(&report, "1 object reference(s)"), "{report:?}");
    }

    #[test]
    fn a_clip_without_media_is_offline_and_reported() {
        let xml = r#"<PremiereData Version="3">
  <Sequence ObjectID="1">
    <Name>S</Name>
    <VideoClipTrack ObjectID="2">
      <TrackItems>
        <VideoClipTrackItem ObjectID="3"><Start>0</Start><End>254016000000</End><Name>lost.mov</Name></VideoClipTrackItem>
      </TrackItems>
    </VideoClipTrack>
  </Sequence>
</PremiereData>"#;
        let (imported, report) = import_bytes(xml.as_bytes(), Format::Fcp7Xml, Some("/projects")).unwrap();
        let seq = imported.project.sequence(imported.sequences[0]).unwrap();
        let clip = &seq.video_tracks[0].items[0];
        let media = imported.project.item(clip.item).and_then(|i| i.as_media()).unwrap();
        assert!(media.offline, "a clip with no media path is offline, not a file named after the clip");
        assert!(has(&report, "has no media file path"), "{report:?}");
    }

    #[test]
    fn errors_name_the_layer_that_failed() {
        let e = import_bytes(b"<PremiereData><Sequence>", Format::Fcp7Xml, None).unwrap_err().to_string();
        assert!(e.contains("Premiere project"), "{e}");
        let e = maybe_decompress(&[0xFF, 0xFE, 0x00, 0xD8]).unwrap_err().to_string();
        assert!(e.contains("XML text"), "{e}");
    }
}
