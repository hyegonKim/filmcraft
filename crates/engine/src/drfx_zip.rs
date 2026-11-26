//! Bounded pure-Rust `.drfx` ZIP archive and RFC 1951 DEFLATE reader (capped at 16 MiB).
//!
//! Every input is treated as hostile: truncated headers, out-of-range offsets, invalid Huffman
//! tables, back-references before start of output, and archives exceeding [`MAX_DRFX_BYTES`] return
//! `Err`, never panics or unbounded allocations.

pub const MAX_DRFX_BYTES: usize = 16 * 1024 * 1024;
const MAX_DRFX_ENTRIES: usize = 256;
const FAST_BITS: u32 = 10;
const MAX_BITS: usize = 15;

#[derive(Clone, Debug)]
pub struct ZipEntry {
    pub name: String,
    pub data: Vec<u8>,
}

fn u16_le(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_le(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Extract non-directory entries from a `.drfx` ZIP archive with a hard 16 MiB total budget.
pub fn read_zip_entries(archive: &[u8]) -> Result<Vec<ZipEntry>, String> {
    if archive.len() > MAX_DRFX_BYTES {
        return Err(format!("DRFX archive exceeds the {MAX_DRFX_BYTES}-byte limit"));
    }
    if archive.len() < 4 {
        return Err("DRFX archive is too small to be a ZIP container".into());
    }
    if let Some((cd_off, cd_count)) = find_eocd(archive) {
        return read_from_central_dir(archive, cd_off, cd_count);
    }
    if u32_le(archive, 0) == Some(0x0403_4b50) {
        return read_from_local_headers(archive);
    }
    Err("not a valid DRFX ZIP archive".into())
}

fn find_eocd(b: &[u8]) -> Option<(usize, usize)> {
    if b.len() < 22 {
        return None;
    }
    let min_off = b.len().saturating_sub(22 + 65_535);
    let mut pos = b.len() - 22;
    loop {
        if u32_le(b, pos) == Some(0x0605_4b50) {
            let count = u16_le(b, pos + 10)? as usize;
            let cd_off = u32_le(b, pos + 16)? as usize;
            if cd_off <= pos {
                return Some((cd_off, count));
            }
        }
        if pos <= min_off {
            break;
        }
        pos -= 1;
    }
    None
}

fn read_from_central_dir(b: &[u8], mut off: usize, count: usize) -> Result<Vec<ZipEntry>, String> {
    let entries_cap = count.min(MAX_DRFX_ENTRIES);
    let mut out = Vec::new();
    let mut total_uncompressed = 0usize;

    for _ in 0..entries_cap {
        if u32_le(b, off) != Some(0x0201_4b50) {
            break;
        }
        let method = u16_le(b, off + 10).ok_or("truncated ZIP central header")?;
        let comp_sz = u32_le(b, off + 20).ok_or("truncated ZIP central header")? as usize;
        let uncomp_sz = u32_le(b, off + 24).ok_or("truncated ZIP central header")? as usize;
        let name_len = u16_le(b, off + 28).ok_or("truncated ZIP central header")? as usize;
        let extra_len = u16_le(b, off + 30).ok_or("truncated ZIP central header")? as usize;
        let comment_len = u16_le(b, off + 32).ok_or("truncated ZIP central header")? as usize;
        let local_off = u32_le(b, off + 42).ok_or("truncated ZIP central header")? as usize;

        let name_start = off.checked_add(46).ok_or("ZIP header offset overflow")?;
        let name_end = name_start.checked_add(name_len).ok_or("ZIP header offset overflow")?;
        let name_bytes = b.get(name_start..name_end).ok_or("truncated ZIP entry name")?;
        let name = String::from_utf8_lossy(name_bytes).replace('\\', "/");

        off = name_end
            .checked_add(extra_len)
            .and_then(|x| x.checked_add(comment_len))
            .ok_or("ZIP central header overflow")?;

        if name.ends_with('/') || name.is_empty() {
            continue;
        }
        let data = extract_local_payload(b, local_off, method, comp_sz, uncomp_sz, &mut total_uncompressed)?;
        out.push(ZipEntry { name, data });
    }
    Ok(out)
}

fn read_from_local_headers(b: &[u8]) -> Result<Vec<ZipEntry>, String> {
    let mut off = 0usize;
    let mut out = Vec::new();
    let mut total_uncompressed = 0usize;

    for _ in 0..MAX_DRFX_ENTRIES {
        let Some(sig) = u32_le(b, off) else { break };
        if sig != 0x0403_4b50 {
            break;
        }
        let flags = u16_le(b, off + 6).ok_or("truncated ZIP local header")?;
        if flags & 0x0008 != 0 {
            return Err("streaming ZIP data descriptors require a central directory".into());
        }
        let method = u16_le(b, off + 8).ok_or("truncated ZIP local header")?;
        let comp_sz = u32_le(b, off + 18).ok_or("truncated ZIP local header")? as usize;
        let uncomp_sz = u32_le(b, off + 22).ok_or("truncated ZIP local header")? as usize;
        let name_len = u16_le(b, off + 26).ok_or("truncated ZIP local header")? as usize;
        let extra_len = u16_le(b, off + 28).ok_or("truncated ZIP local header")? as usize;

        let name_start = off.checked_add(30).ok_or("ZIP local offset overflow")?;
        let name_end = name_start.checked_add(name_len).ok_or("ZIP local offset overflow")?;
        let payload_start = name_end.checked_add(extra_len).ok_or("ZIP local offset overflow")?;
        let payload_end = payload_start.checked_add(comp_sz).ok_or("ZIP local offset overflow")?;
        let name_bytes = b.get(name_start..name_end).ok_or("truncated ZIP local name")?;
        let name = String::from_utf8_lossy(name_bytes).replace('\\', "/");

        if !name.ends_with('/') && !name.is_empty() {
            let data = extract_local_payload(b, off, method, comp_sz, uncomp_sz, &mut total_uncompressed)?;
            out.push(ZipEntry { name, data });
        }
        off = payload_end;
    }
    Ok(out)
}

fn extract_local_payload(
    b: &[u8],
    local_off: usize,
    method: u16,
    comp_sz: usize,
    uncomp_sz: usize,
    total_uncompressed: &mut usize,
) -> Result<Vec<u8>, String> {
    if u32_le(b, local_off) != Some(0x0403_4b50) {
        return Err("missing ZIP local file header".into());
    }
    let name_len = u16_le(b, local_off + 26).ok_or("truncated ZIP local header")? as usize;
    let extra_len = u16_le(b, local_off + 28).ok_or("truncated ZIP local header")? as usize;
    let start = local_off.checked_add(30).and_then(|x| x.checked_add(name_len)).and_then(|x| x.checked_add(extra_len)).ok_or("ZIP payload offset overflow")?;
    let end = start.checked_add(comp_sz).ok_or("ZIP payload size overflow")?;
    let payload = b.get(start..end).ok_or("truncated ZIP entry payload")?;

    let next_total = total_uncompressed.checked_add(uncomp_sz).ok_or("DRFX uncompressed size overflow")?;
    if next_total > MAX_DRFX_BYTES {
        return Err(format!("DRFX uncompressed data exceeds {MAX_DRFX_BYTES} bytes"));
    }

    let decoded = match method {
        0 => {
            if comp_sz != uncomp_sz {
                return Err("stored ZIP entry length mismatch".into());
            }
            payload.to_vec()
        }
        8 => inflate_raw(payload, uncomp_sz)?,
        other => return Err(format!("unsupported ZIP compression method {other}")),
    };
    *total_uncompressed = total_uncompressed.saturating_add(decoded.len());
    if *total_uncompressed > MAX_DRFX_BYTES {
        return Err(format!("DRFX uncompressed data exceeds {MAX_DRFX_BYTES} bytes"));
    }
    Ok(decoded)
}

// ---------------------------------------------------------------------------
// RFC 1951 Raw DEFLATE Inflater
// ---------------------------------------------------------------------------

struct Bits<'a> {
    src: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
}

impl Bits<'_> {
    fn refill(&mut self) {
        while self.cnt <= 56 {
            let Some(&b) = self.src.get(self.pos) else { return };
            self.buf |= u64::from(b) << self.cnt;
            self.pos += 1;
            self.cnt += 8;
        }
    }

    fn peek(&mut self, n: u32) -> u32 {
        self.refill();
        (self.buf & ((1u64 << n) - 1)) as u32
    }

    fn consume(&mut self, n: u32) -> Result<(), String> {
        if n > self.cnt {
            return Err("truncated DEFLATE stream".into());
        }
        self.buf >>= n;
        self.cnt -= n;
        Ok(())
    }

    fn take(&mut self, n: u32) -> Result<u32, String> {
        let v = self.peek(n);
        self.consume(n)?;
        Ok(v)
    }
}

struct Huffman {
    fast: Vec<u16>,
    count: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self, String> {
        let mut count = [0u16; MAX_BITS + 1];
        for &l in lengths {
            let slot = count.get_mut(usize::from(l)).ok_or("Huffman code length over 15")?;
            *slot += 1;
        }
        count[0] = 0;
        let mut left: i32 = 1;
        for &c in &count[1..] {
            left = (left << 1) - i32::from(c);
            if left < 0 {
                return Err("over-subscribed Huffman code".into());
            }
        }
        let mut offs = [0usize; MAX_BITS + 2];
        let mut next = [0u32; MAX_BITS + 2];
        let mut code = 0u32;
        for len in 1..=MAX_BITS {
            offs[len + 1] = offs[len] + usize::from(count[len]);
            code = (code + u32::from(count[len - 1])) << 1;
            next[len] = code;
        }
        let mut symbols = vec![0u16; offs[MAX_BITS + 1]];
        let mut fast = vec![0u16; 1 << FAST_BITS];
        for (sym, &l) in lengths.iter().enumerate() {
            let len = usize::from(l);
            if len == 0 {
                continue;
            }
            if let Some(s) = symbols.get_mut(offs[len]) {
                *s = sym as u16;
            }
            offs[len] += 1;
            let c = next[len];
            next[len] += 1;
            if len as u32 <= FAST_BITS {
                let rev = c.reverse_bits() >> (32 - len as u32);
                let entry = (sym as u16) << 4 | len as u16;
                let mut i = rev as usize;
                while let Some(e) = fast.get_mut(i) {
                    *e = entry;
                    i += 1 << len;
                }
            }
        }
        Ok(Self { fast, count, symbols })
    }

    fn decode(&self, bits: &mut Bits<'_>) -> Result<u16, String> {
        let peek = bits.peek(MAX_BITS as u32);
        let e = self.fast.get((peek & ((1 << FAST_BITS) - 1)) as usize).copied().unwrap_or(0);
        if e != 0 {
            bits.consume(u32::from(e & 15))?;
            return Ok(e >> 4);
        }
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=MAX_BITS {
            code |= ((peek >> (len - 1)) & 1) as i32;
            let cnt = i32::from(self.count[len]);
            if code - first < cnt {
                bits.consume(len as u32)?;
                return self.symbols.get((index + code - first) as usize).copied().ok_or_else(|| "invalid Huffman symbol".into());
            }
            index += cnt;
            first = (first + cnt) << 1;
            code <<= 1;
        }
        Err("invalid Huffman code".into())
    }
}

fn fixed_tables() -> Result<(Huffman, Huffman), String> {
    let mut lit = [8u8; 288];
    lit[144..256].fill(9);
    lit[256..280].fill(7);
    Ok((Huffman::new(&lit)?, Huffman::new(&[5; 30])?))
}

const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
const LEN_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] =
    [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

pub fn inflate_raw(src: &[u8], expected_limit: usize) -> Result<Vec<u8>, String> {
    let limit = expected_limit.min(MAX_DRFX_BYTES);
    let mut bits = Bits { src, pos: 0, buf: 0, cnt: 0 };
    let mut out = Vec::new();
    out.try_reserve_exact(limit.min(65_536)).map_err(|_| "DEFLATE allocation failed".to_string())?;
    let mut fixed: Option<(Huffman, Huffman)> = None;
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => inflate_stored(&mut bits, &mut out, limit)?,
            1 => {
                let (lit, dist) = match &fixed {
                    Some(t) => t,
                    None => fixed.insert(fixed_tables()?),
                };
                inflate_codes(&mut bits, &mut out, limit, lit, dist)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut bits)?;
                inflate_codes(&mut bits, &mut out, limit, &lit, &dist)?;
            }
            _ => return Err("invalid DEFLATE block type".into()),
        }
        if last {
            return Ok(out);
        }
    }
}

fn inflate_stored(bits: &mut Bits<'_>, out: &mut Vec<u8>, limit: usize) -> Result<(), String> {
    bits.consume(bits.cnt % 8)?;
    let len = bits.take(16)?;
    if len != (!bits.take(16)? & 0xFFFF) {
        return Err("DEFLATE stored block length mismatch".into());
    }
    if out.len().saturating_add(len as usize) > limit {
        return Err("DEFLATE output exceeds limit".into());
    }
    for _ in 0..len {
        out.push(bits.take(8)? as u8);
    }
    Ok(())
}

fn dynamic_tables(bits: &mut Bits<'_>) -> Result<(Huffman, Huffman), String> {
    let nlit = bits.take(5)? as usize + 257;
    let ndist = bits.take(5)? as usize + 1;
    let ncl = bits.take(4)? as usize + 4;
    if nlit > 286 || ndist > 30 {
        return Err("too many DEFLATE codes".into());
    }
    let mut cl = [0u8; 19];
    for &idx in CL_ORDER.iter().take(ncl) {
        cl[idx] = bits.take(3)? as u8;
    }
    let clh = Huffman::new(&cl)?;
    let mut lengths = vec![0u8; nlit + ndist];
    let mut i = 0;
    while i < lengths.len() {
        let sym = clh.decode(bits)?;
        let (val, rep) = match sym {
            0..=15 => (sym as u8, 1),
            16 => {
                let prev = *lengths.get(i.wrapping_sub(1)).ok_or("repeat with no previous length")?;
                (prev, 3 + bits.take(2)? as usize)
            }
            17 => (0, 3 + bits.take(3)? as usize),
            18 => (0, 11 + bits.take(7)? as usize),
            _ => return Err("invalid code length symbol".into()),
        };
        let run = lengths.get_mut(i..i + rep).ok_or("DEFLATE code lengths overflow")?;
        run.fill(val);
        i += rep;
    }
    let (lit, dist) = lengths.split_at(nlit);
    if lit.get(256).copied().unwrap_or(0) == 0 {
        return Err("missing DEFLATE end-of-block code".into());
    }
    Ok((Huffman::new(lit)?, Huffman::new(dist)?))
}

fn inflate_codes(bits: &mut Bits<'_>, out: &mut Vec<u8>, limit: usize, lit: &Huffman, dist: &Huffman) -> Result<(), String> {
    loop {
        let sym = lit.decode(bits)?;
        if sym < 256 {
            if out.len() >= limit {
                return Err("DEFLATE output exceeds limit".into());
            }
            out.push(sym as u8);
            continue;
        }
        if sym == 256 {
            return Ok(());
        }
        let k = usize::from(sym - 257);
        let (&base, &extra) = LEN_BASE.get(k).zip(LEN_EXTRA.get(k)).ok_or("invalid length code")?;
        let len = usize::from(base) + bits.take(u32::from(extra))? as usize;
        let d = usize::from(dist.decode(bits)?);
        let (&dbase, &dextra) = DIST_BASE.get(d).zip(DIST_EXTRA.get(d)).ok_or("invalid distance code")?;
        let distance = usize::from(dbase) + bits.take(u32::from(dextra))? as usize;
        if distance == 0 || distance > out.len() {
            return Err("DEFLATE distance before start of output".into());
        }
        if out.len().saturating_add(len) > limit {
            return Err("DEFLATE output exceeds limit".into());
        }
        let from = out.len() - distance;
        if distance >= len {
            out.extend_from_within(from..from + len);
        } else {
            for j in 0..len {
                let b = out.get(from + j).copied().unwrap_or(0);
                out.push(b);
            }
        }
    }
}
