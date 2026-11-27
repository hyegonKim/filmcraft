//! Guarded sparse 4 KiB virtual address space, guest heap allocator, and Macintosh-style
//! double-indirect `Handle` table for cross-platform C++ binary plugin execution.

use std::collections::HashMap;

use super::{PluginHostError, Result};

pub const PAGE_SHIFT: u32 = 12;
pub const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
pub const PAGE_MASK: u64 = (PAGE_SIZE as u64) - 1;

pub const NULL_GUARD_LIMIT: u64 = 0x0000_0000_0001_0000;
pub const DEFAULT_IMAGE_BASE: u64 = 0x0000_0001_8000_0000;
pub const HEAP_BASE: u64 = 0x0000_0200_0000_0000;
pub const HANDLE_MASTER_BASE: u64 = 0x0000_0300_0000_0000;
pub const STACK_BASE: u64 = 0x0000_0700_0000_0000;
pub const STACK_SIZE: u64 = 0x0000_0000_0010_0000; // 1 MiB
pub const STACK_TOP: u64 = STACK_BASE + STACK_SIZE;
pub const TEB_BASE: u64 = 0x0000_07FD_0000_0000;
pub const RETURN_SENTINEL: u64 = 0x0000_07FE_FFFF_FFF0;
pub const THUNK_BASE: u64 = 0x0000_07FF_0000_0000;
pub const THUNK_STRIDE: u64 = 16;
pub const THUNK_LIMIT: u64 = THUNK_BASE + 65536 * THUNK_STRIDE;

const HEAP_HEADER_BYTES: u64 = 16;

#[derive(Clone, Debug)]
struct HandleEntry {
    data_ptr: u64,
    size: usize,
    locked: bool,
}

/// Sparse 64-bit guest virtual address space backed by 4 KiB pages.
pub struct PagedMemory {
    pages: HashMap<u64, Box<[u8; PAGE_SIZE]>>,
    max_pages: usize,
    heap_cursor: u64,
    next_handle_slot: u64,
    handles: HashMap<u64, HandleEntry>,
    alloc_sizes: HashMap<u64, usize>,
}

impl PagedMemory {
    pub fn new(max_memory_bytes: usize) -> Self {
        let max_pages = (max_memory_bytes / PAGE_SIZE).max(64);
        Self {
            pages: HashMap::new(),
            max_pages,
            heap_cursor: HEAP_BASE,
            next_handle_slot: HANDLE_MASTER_BASE,
            handles: HashMap::new(),
            alloc_sizes: HashMap::new(),
        }
    }

    pub fn allocated_bytes(&self) -> usize {
        self.pages.len() * PAGE_SIZE
    }

    pub fn is_thunk_addr(addr: u64) -> Option<u32> {
        if (THUNK_BASE..THUNK_LIMIT).contains(&addr) && (addr - THUNK_BASE).is_multiple_of(THUNK_STRIDE) {
            Some(((addr - THUNK_BASE) / THUNK_STRIDE) as u32)
        } else {
            None
        }
    }

    pub fn thunk_addr(index: u32) -> u64 {
        THUNK_BASE + u64::from(index) * THUNK_STRIDE
    }

    fn check_addr(addr: u64) -> Result<()> {
        if addr < NULL_GUARD_LIMIT {
            return Err(PluginHostError::MemoryFault(format!("null/low pointer dereference at {addr:#x}")));
        }
        Ok(())
    }

    fn page_mut(&mut self, page_idx: u64) -> Result<&mut [u8; PAGE_SIZE]> {
        if !self.pages.contains_key(&page_idx) {
            if self.pages.len() >= self.max_pages {
                return Err(PluginHostError::Limit(format!("guest memory limit exceeded ({} pages)", self.max_pages)));
            }
            self.pages.insert(page_idx, Box::new([0u8; PAGE_SIZE]));
        }
        self.pages.get_mut(&page_idx).map(Box::as_mut).ok_or_else(|| PluginHostError::MemoryFault("page lookup failed".into()))
    }

    pub fn read_bytes(&self, addr: u64, out: &mut [u8]) -> Result<()> {
        if out.is_empty() {
            return Ok(());
        }
        Self::check_addr(addr)?;
        let mut cur = addr;
        let mut pos = 0usize;
        while pos < out.len() {
            let page_idx = cur >> PAGE_SHIFT;
            let off = (cur & PAGE_MASK) as usize;
            let chunk = (PAGE_SIZE - off).min(out.len() - pos);
            if let Some(page) = self.pages.get(&page_idx) {
                out[pos..pos + chunk].copy_from_slice(&page[off..off + chunk]);
            } else {
                out[pos..pos + chunk].fill(0);
            }
            cur = cur.checked_add(chunk as u64).ok_or_else(|| PluginHostError::MemoryFault("address overflow".into()))?;
            pos += chunk;
        }
        Ok(())
    }

    pub fn read_vec(&self, addr: u64, len: usize) -> Result<Vec<u8>> {
        if len > self.max_pages * PAGE_SIZE {
            return Err(PluginHostError::Limit(format!("excessive read length {len}")));
        }
        let mut buf = vec![0u8; len];
        self.read_bytes(addr, &mut buf)?;
        Ok(buf)
    }

    pub fn write_bytes(&mut self, addr: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        Self::check_addr(addr)?;
        let mut cur = addr;
        let mut pos = 0usize;
        while pos < data.len() {
            let page_idx = cur >> PAGE_SHIFT;
            let off = (cur & PAGE_MASK) as usize;
            let chunk = (PAGE_SIZE - off).min(data.len() - pos);
            let page = self.page_mut(page_idx)?;
            page[off..off + chunk].copy_from_slice(&data[pos..pos + chunk]);
            cur = cur.checked_add(chunk as u64).ok_or_else(|| PluginHostError::MemoryFault("address overflow".into()))?;
            pos += chunk;
        }
        Ok(())
    }

    pub fn zero_fill(&mut self, addr: u64, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        Self::check_addr(addr)?;
        let zeros = [0u8; 256];
        let mut cur = addr;
        let mut rem = len;
        while rem > 0 {
            let n = rem.min(zeros.len());
            self.write_bytes(cur, &zeros[..n])?;
            cur = cur.checked_add(n as u64).ok_or_else(|| PluginHostError::MemoryFault("address overflow".into()))?;
            rem -= n;
        }
        Ok(())
    }

    pub fn fill_byte(&mut self, addr: u64, val: u8, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        Self::check_addr(addr)?;
        let block = [val; 256];
        let mut cur = addr;
        let mut rem = len;
        while rem > 0 {
            let n = rem.min(block.len());
            self.write_bytes(cur, &block[..n])?;
            cur = cur.checked_add(n as u64).ok_or_else(|| PluginHostError::MemoryFault("address overflow".into()))?;
            rem -= n;
        }
        Ok(())
    }

    pub fn read_u8(&self, addr: u64) -> Result<u8> {
        let mut b = [0u8; 1];
        self.read_bytes(addr, &mut b)?;
        Ok(b[0])
    }

    pub fn read_u16(&self, addr: u64) -> Result<u16> {
        let mut b = [0u8; 2];
        self.read_bytes(addr, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    pub fn read_i16(&self, addr: u64) -> Result<i16> {
        let mut b = [0u8; 2];
        self.read_bytes(addr, &mut b)?;
        Ok(i16::from_le_bytes(b))
    }

    pub fn read_u32(&self, addr: u64) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read_bytes(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    pub fn read_i32(&self, addr: u64) -> Result<i32> {
        let mut b = [0u8; 4];
        self.read_bytes(addr, &mut b)?;
        Ok(i32::from_le_bytes(b))
    }

    pub fn read_u64(&self, addr: u64) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read_bytes(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    pub fn read_i64(&self, addr: u64) -> Result<i64> {
        let mut b = [0u8; 8];
        self.read_bytes(addr, &mut b)?;
        Ok(i64::from_le_bytes(b))
    }

    pub fn read_f32(&self, addr: u64) -> Result<f32> {
        Ok(f32::from_bits(self.read_u32(addr)?))
    }

    pub fn read_f64(&self, addr: u64) -> Result<f64> {
        Ok(f64::from_bits(self.read_u64(addr)?))
    }

    pub fn read_u128(&self, addr: u64) -> Result<u128> {
        let mut b = [0u8; 16];
        self.read_bytes(addr, &mut b)?;
        Ok(u128::from_le_bytes(b))
    }

    pub fn write_u8(&mut self, addr: u64, v: u8) -> Result<()> {
        self.write_bytes(addr, &[v])
    }

    pub fn write_u16(&mut self, addr: u64, v: u16) -> Result<()> {
        self.write_bytes(addr, &v.to_le_bytes())
    }

    pub fn write_i16(&mut self, addr: u64, v: i16) -> Result<()> {
        self.write_bytes(addr, &v.to_le_bytes())
    }

    pub fn write_u32(&mut self, addr: u64, v: u32) -> Result<()> {
        self.write_bytes(addr, &v.to_le_bytes())
    }

    pub fn write_i32(&mut self, addr: u64, v: i32) -> Result<()> {
        self.write_bytes(addr, &v.to_le_bytes())
    }

    pub fn write_u64(&mut self, addr: u64, v: u64) -> Result<()> {
        self.write_bytes(addr, &v.to_le_bytes())
    }

    pub fn write_i64(&mut self, addr: u64, v: i64) -> Result<()> {
        self.write_bytes(addr, &v.to_le_bytes())
    }

    pub fn write_f32(&mut self, addr: u64, v: f32) -> Result<()> {
        self.write_u32(addr, v.to_bits())
    }

    pub fn write_f64(&mut self, addr: u64, v: f64) -> Result<()> {
        self.write_u64(addr, v.to_bits())
    }

    pub fn write_u128(&mut self, addr: u64, v: u128) -> Result<()> {
        self.write_bytes(addr, &v.to_le_bytes())
    }

    pub fn read_cstring(&self, addr: u64, max_len: usize) -> Result<String> {
        Self::check_addr(addr)?;
        let mut bytes = Vec::new();
        for i in 0..max_len {
            let b = self.read_u8(addr + i as u64)?;
            if b == 0 {
                break;
            }
            bytes.push(b);
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn alloc_cstring(&mut self, s: &str) -> Result<u64> {
        let bytes = s.as_bytes();
        let ptr = self.heap_alloc(bytes.len() + 1, true)?;
        self.write_bytes(ptr, bytes)?;
        self.write_u8(ptr + bytes.len() as u64, 0)?;
        Ok(ptr)
    }

    /// 16-byte aligned guest heap allocation.
    pub fn heap_alloc(&mut self, size: usize, zeroed: bool) -> Result<u64> {
        let actual = size.max(1);
        if actual > self.max_pages * PAGE_SIZE {
            return Err(PluginHostError::Limit(format!("heap allocation {size} exceeds memory budget")));
        }
        let aligned_cur = (self.heap_cursor + 15) & !15;
        let user_ptr = aligned_cur + HEAP_HEADER_BYTES;
        let next = user_ptr.checked_add(((actual as u64) + 15) & !15).ok_or_else(|| PluginHostError::MemoryFault("heap overflow".into()))?;
        self.write_u64(aligned_cur, actual as u64)?;
        if zeroed {
            self.zero_fill(user_ptr, actual)?;
        } else {
            // Touch first and last byte so page budget is accounted immediately.
            self.write_u8(user_ptr, 0)?;
            self.write_u8(user_ptr + (actual - 1) as u64, 0)?;
        }
        self.heap_cursor = next;
        self.alloc_sizes.insert(user_ptr, actual);
        Ok(user_ptr)
    }

    pub fn heap_realloc(&mut self, ptr: u64, new_size: usize) -> Result<u64> {
        if ptr == 0 {
            return self.heap_alloc(new_size, false);
        }
        let old_size = self.ptr_size(ptr);
        let new_ptr = self.heap_alloc(new_size, true)?;
        let copy_len = old_size.min(new_size);
        if copy_len > 0 {
            let buf = self.read_vec(ptr, copy_len)?;
            self.write_bytes(new_ptr, &buf)?;
        }
        self.alloc_sizes.remove(&ptr);
        Ok(new_ptr)
    }

    pub fn heap_free(&mut self, ptr: u64) {
        self.alloc_sizes.remove(&ptr);
    }

    pub fn ptr_size(&self, ptr: u64) -> usize {
        if let Some(&sz) = self.alloc_sizes.get(&ptr) {
            return sz;
        }
        ptr.checked_sub(HEAP_HEADER_BYTES).and_then(|hdr| self.read_u64(hdr).ok()).map_or(0, |v| v as usize)
    }

    /// Allocates a Macintosh/Photoshop relocatable `Handle` (`Ptr*` master pointer).
    pub fn new_handle(&mut self, size: usize, zeroed: bool) -> Result<u64> {
        let data_ptr = self.heap_alloc(size.max(1), zeroed)?;
        let handle_addr = self.next_handle_slot;
        self.next_handle_slot += 16;
        self.write_u64(handle_addr, data_ptr)?;
        self.handles.insert(handle_addr, HandleEntry { data_ptr, size, locked: false });
        Ok(handle_addr)
    }

    pub fn lock_handle(&mut self, handle: u64) -> Result<u64> {
        if let Some(entry) = self.handles.get_mut(&handle) {
            entry.locked = true;
            return Ok(entry.data_ptr);
        }
        self.read_u64(handle)
    }

    pub fn unlock_handle(&mut self, handle: u64) {
        if let Some(entry) = self.handles.get_mut(&handle) {
            entry.locked = false;
        }
    }

    pub fn handle_size(&self, handle: u64) -> usize {
        self.handles.get(&handle).map_or_else(|| self.read_u64(handle).ok().map_or(0, |p| self.ptr_size(p)), |e| e.size)
    }

    pub fn resize_handle(&mut self, handle: u64, new_size: usize) -> Result<()> {
        let old_ptr = self.handles.get(&handle).map(|e| e.data_ptr).unwrap_or(0);
        let new_ptr = self.heap_realloc(old_ptr, new_size.max(1))?;
        self.write_u64(handle, new_ptr)?;
        self.handles.insert(handle, HandleEntry { data_ptr: new_ptr, size: new_size, locked: false });
        Ok(())
    }

    pub fn dispose_handle(&mut self, handle: u64) {
        if let Some(entry) = self.handles.remove(&handle) {
            self.heap_free(entry.data_ptr);
        }
    }
}
