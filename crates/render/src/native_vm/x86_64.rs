//! Metered `x86_64` user-mode CPU emulator supporting both **Microsoft x64** (`PE32+`) and
//! **System V AMD64** (`Mach-O x86_64`) calling conventions, GPR ALU/shifts/mul/div, relative
//! switch jump tables, and SSE/SSE2/AVX-128 instructions.

use super::crt::{GuestVm, HostCallbackHandler, ThunkOutcome};
use super::mem::{PagedMemory, RETURN_SENTINEL, STACK_BASE, STACK_TOP, TEB_BASE};
use super::{GuestOs, PluginHostError, Result};

const CF: u64 = 1 << 0;
const ZF: u64 = 1 << 6;
const SF: u64 = 1 << 7;
const DF: u64 = 1 << 10;
const OF: u64 = 1 << 11;

#[derive(Clone, Copy, Default)]
struct Prefixes {
    rex: u8,
    op66: bool,
    rep_f3: bool,
    rep_f2: bool,
    seg_gs: bool,
    vex_vvvv: Option<u8>,
}

impl Prefixes {
    fn rex_w(self) -> bool {
        (self.rex & 0x08) != 0
    }
    fn rex_r(self) -> u8 {
        (self.rex >> 2) & 1
    }
    fn rex_x(self) -> u8 {
        (self.rex >> 1) & 1
    }
    fn rex_b(self) -> u8 {
        self.rex & 1
    }
    fn has_rex(self) -> bool {
        self.rex != 0
    }
    fn op_bits(self) -> u8 {
        if self.rex_w() {
            64
        } else if self.op66 {
            16
        } else {
            32
        }
    }
}

#[derive(Clone, Copy)]
enum RmLoc {
    Reg(u8),
    Mem(u64),
    RipRel(i32),
}

/// Complete `x86_64` user-mode register state.
pub struct X64Cpu {
    pub gpr: [u64; 16], // RAX, RCX, RDX, RBX, RSP, RBP, RSI, RDI, R8..R15
    pub xmm: [u128; 16],
    pub rip: u64,
    pub rflags: u64,
}

impl X64Cpu {
    pub fn new_call(os: GuestOs, fn_addr: u64, sp_base: u64, args: &[u64], mem: &mut PagedMemory) -> Result<Self> {
        let mut cpu = Self { gpr: [0; 16], xmm: [0; 16], rip: fn_addr, rflags: 0x202 };
        let mut rsp = sp_base & !15;
        match os {
            GuestOs::Windows => {
                // Win64: RCX, RDX, R8, R9 + 32-byte shadow space + stack args 4..N above shadow space
                if let Some(&a0) = args.first() {
                    cpu.gpr[1] = a0;
                }
                if let Some(&a1) = args.get(1) {
                    cpu.gpr[2] = a1;
                }
                if let Some(&a2) = args.get(2) {
                    cpu.gpr[8] = a2;
                }
                if let Some(&a3) = args.get(3) {
                    cpu.gpr[9] = a3;
                }
                let stack_slots = args.len().saturating_sub(4).max(4) + 2;
                let frame_bytes = ((stack_slots * 8 + 15) & !15) as u64;
                rsp -= frame_bytes;
                for (i, &val) in args.iter().skip(4).enumerate() {
                    mem.write_u64(rsp + 32 + (i as u64) * 8, val)?;
                }
                rsp -= 8;
                mem.write_u64(rsp, RETURN_SENTINEL)?;
            }
            GuestOs::MacOs => {
                // SysV AMD64: RDI(7), RSI(6), RDX(2), RCX(1), R8(8), R9(9)
                let reg_map = [7usize, 6, 2, 1, 8, 9];
                for (i, &reg_idx) in reg_map.iter().enumerate() {
                    if let Some(&v) = args.get(i) {
                        cpu.gpr[reg_idx] = v;
                    }
                }
                let extra = args.len().saturating_sub(6);
                let frame_bytes = (((extra * 8) + 15) & !15) as u64;
                rsp -= frame_bytes;
                for (i, &val) in args.iter().skip(6).enumerate() {
                    mem.write_u64(rsp + (i as u64) * 8, val)?;
                }
                rsp -= 8;
                mem.write_u64(rsp, RETURN_SENTINEL)?;
            }
        }
        // Initialize Win64 TEB (`gs:[0x30]` self-pointer, `gs:[0x08]` StackBase, `gs:[0x10]` StackLimit, `gs:[0x58]` TLS array)
        if os == GuestOs::Windows && mem.read_u64(TEB_BASE + 0x30).unwrap_or(0) == 0 {
            let tls_arr = mem.heap_alloc(64 * 8, true)?;
            let tls_slot0 = mem.heap_alloc(4096, true)?;
            mem.write_u64(tls_arr, tls_slot0)?;
            mem.write_u64(TEB_BASE + 0x08, STACK_TOP)?;
            mem.write_u64(TEB_BASE + 0x10, STACK_BASE)?;
            mem.write_u64(TEB_BASE + 0x30, TEB_BASE)?;
            mem.write_u32(TEB_BASE + 0x40, 1000)?;
            mem.write_u32(TEB_BASE + 0x48, 1001)?;
            mem.write_u64(TEB_BASE + 0x58, tls_arr)?;
        }
        cpu.gpr[4] = rsp;
        Ok(cpu)
    }

    pub fn run(&mut self, vm: &mut GuestVm, handler: &mut dyn HostCallbackHandler) -> Result<u64> {
        loop {
            if self.rip == RETURN_SENTINEL {
                return Ok(self.gpr[0]);
            }
            if let Some(thunk_idx) = PagedMemory::is_thunk_addr(self.rip) {
                self.handle_thunk_trap(thunk_idx, vm, handler)?;
                continue;
            }
            if vm.fuel_remaining == 0 {
                return Err(PluginHostError::Limit(format!(
                    "x86_64 instruction budget exhausted at {:#x} (rax={:#x}, rcx={:#x}, rdx={:#x})",
                    self.rip, self.gpr[0], self.gpr[1], self.gpr[2]
                )));
            }
            vm.fuel_remaining -= 1;
            self.step(&mut vm.mem)?;
        }
    }

    fn handle_thunk_trap(&mut self, thunk_idx: u32, vm: &mut GuestVm, handler: &mut dyn HostCallbackHandler) -> Result<()> {
        let rsp = self.gpr[4];
        let ret_addr = vm.mem.read_u64(rsp)?;
        let (args, fargs) = match vm.os {
            GuestOs::Windows => {
                let args = [
                    self.gpr[1],
                    self.gpr[2],
                    self.gpr[8],
                    self.gpr[9],
                    vm.mem.read_u64(rsp + 0x28).unwrap_or(0),
                    vm.mem.read_u64(rsp + 0x30).unwrap_or(0),
                    vm.mem.read_u64(rsp + 0x38).unwrap_or(0),
                    vm.mem.read_u64(rsp + 0x40).unwrap_or(0),
                ];
                let fargs = [
                    f64::from_bits(self.xmm[0] as u64),
                    f64::from_bits(self.xmm[1] as u64),
                    f64::from_bits(self.xmm[2] as u64),
                    f64::from_bits(self.xmm[3] as u64),
                ];
                (args, fargs)
            }
            GuestOs::MacOs => {
                let args = [
                    self.gpr[7],
                    self.gpr[6],
                    self.gpr[2],
                    self.gpr[1],
                    self.gpr[8],
                    self.gpr[9],
                    vm.mem.read_u64(rsp + 8).unwrap_or(0),
                    vm.mem.read_u64(rsp + 16).unwrap_or(0),
                ];
                let fargs = [
                    f64::from_bits(self.xmm[0] as u64),
                    f64::from_bits(self.xmm[1] as u64),
                    f64::from_bits(self.xmm[2] as u64),
                    f64::from_bits(self.xmm[3] as u64),
                ];
                (args, fargs)
            }
        };
        let outcome = vm.dispatch_thunk(thunk_idx, args, fargs, handler)?;
        match outcome {
            ThunkOutcome::Int(v) => self.gpr[0] = v,
            ThunkOutcome::Float64(f) => self.xmm[0] = u128::from(f.to_bits()),
            ThunkOutcome::Float32(f) => self.xmm[0] = u128::from(f.to_bits()),
            ThunkOutcome::InitTerm { .. } | ThunkOutcome::PiThreadsDoAll { .. } => {}
        }
        self.gpr[4] = rsp + 8;
        self.rip = ret_addr;
        Ok(())
    }

    fn fetch_u8(&mut self, mem: &PagedMemory) -> Result<u8> {
        let v = mem.read_u8(self.rip)?;
        self.rip += 1;
        Ok(v)
    }

    fn fetch_u16(&mut self, mem: &PagedMemory) -> Result<u16> {
        let v = mem.read_u16(self.rip)?;
        self.rip += 2;
        Ok(v)
    }

    fn fetch_u32(&mut self, mem: &PagedMemory) -> Result<u32> {
        let v = mem.read_u32(self.rip)?;
        self.rip += 4;
        Ok(v)
    }

    fn fetch_u64(&mut self, mem: &PagedMemory) -> Result<u64> {
        let v = mem.read_u64(self.rip)?;
        self.rip += 8;
        Ok(v)
    }

    fn read_reg(&self, reg: u8, bits: u8, has_rex: bool) -> u64 {
        let idx = (reg & 15) as usize;
        match bits {
            8 if !has_rex && (4..8).contains(&idx) => (self.gpr[idx - 4] >> 8) & 0xFF,
            8 => self.gpr[idx] & 0xFF,
            16 => self.gpr[idx] & 0xFFFF,
            32 => self.gpr[idx] & 0xFFFF_FFFF,
            _ => self.gpr[idx],
        }
    }

    fn write_reg(&mut self, reg: u8, bits: u8, has_rex: bool, val: u64) {
        let idx = (reg & 15) as usize;
        match bits {
            8 if !has_rex && (4..8).contains(&idx) => {
                self.gpr[idx - 4] = (self.gpr[idx - 4] & !0xFF00) | ((val & 0xFF) << 8);
            }
            8 => self.gpr[idx] = (self.gpr[idx] & !0xFF) | (val & 0xFF),
            16 => self.gpr[idx] = (self.gpr[idx] & !0xFFFF) | (val & 0xFFFF),
            32 => self.gpr[idx] = val & 0xFFFF_FFFF, // 32-bit write zero-extends to 64 bits
            _ => self.gpr[idx] = val,
        }
    }

    fn resolve_addr(&self, loc: RmLoc) -> Option<u64> {
        match loc {
            RmLoc::Reg(_) => None,
            RmLoc::Mem(addr) => Some(addr),
            RmLoc::RipRel(disp) => Some(self.rip.wrapping_add_signed(i64::from(disp))),
        }
    }

    fn read_rm(&self, mem: &PagedMemory, loc: RmLoc, bits: u8, has_rex: bool) -> Result<u64> {
        match loc {
            RmLoc::Reg(r) => Ok(self.read_reg(r, bits, has_rex)),
            RmLoc::Mem(_) | RmLoc::RipRel(_) => {
                let addr = self.resolve_addr(loc).unwrap_or(0);
                match bits {
                    8 => Ok(u64::from(mem.read_u8(addr)?)),
                    16 => Ok(u64::from(mem.read_u16(addr)?)),
                    32 => Ok(u64::from(mem.read_u32(addr)?)),
                    _ => mem.read_u64(addr),
                }
            }
        }
    }

    fn write_rm(&mut self, mem: &mut PagedMemory, loc: RmLoc, bits: u8, has_rex: bool, val: u64) -> Result<()> {
        match loc {
            RmLoc::Reg(r) => {
                self.write_reg(r, bits, has_rex, val);
                Ok(())
            }
            RmLoc::Mem(_) | RmLoc::RipRel(_) => {
                let addr = self.resolve_addr(loc).unwrap_or(0);
                match bits {
                    8 => mem.write_u8(addr, val as u8),
                    16 => mem.write_u16(addr, val as u16),
                    32 => mem.write_u32(addr, val as u32),
                    _ => mem.write_u64(addr, val),
                }
            }
        }
    }

    fn decode_modrm(&mut self, mem: &PagedMemory, pfx: Prefixes) -> Result<(u8, RmLoc)> {
        let modrm = self.fetch_u8(mem)?;
        let mode = modrm >> 6;
        let reg = ((modrm >> 3) & 7) | (pfx.rex_r() << 3);
        let rm_low = modrm & 7;
        if mode == 3 {
            return Ok((reg, RmLoc::Reg(rm_low | (pfx.rex_b() << 3))));
        }
        if mode == 0 && rm_low == 5 {
            let disp = self.fetch_u32(mem)? as i32;
            return Ok((reg, RmLoc::RipRel(disp)));
        }
        let addr = if rm_low == 4 {
            self.decode_sib(mem, mode, pfx)?
        } else {
            let base = self.gpr[(rm_low | (pfx.rex_b() << 3)) as usize];
            let disp = match mode {
                1 => self.fetch_u8(mem)? as i8 as i64,
                2 => self.fetch_u32(mem)? as i32 as i64,
                _ => 0,
            };
            base.wrapping_add_signed(disp)
        };
        let seg_base = if pfx.seg_gs { TEB_BASE } else { 0 };
        Ok((reg, RmLoc::Mem(seg_base.wrapping_add(addr))))
    }

    fn decode_sib(&mut self, mem: &PagedMemory, mode: u8, pfx: Prefixes) -> Result<u64> {
        let sib = self.fetch_u8(mem)?;
        let scale = 1u64 << (sib >> 6);
        let index_reg = ((sib >> 3) & 7) | (pfx.rex_x() << 3);
        let base_low = sib & 7;
        let base_reg = base_low | (pfx.rex_b() << 3);
        let index_val = if index_reg == 4 { 0 } else { self.gpr[index_reg as usize].wrapping_mul(scale) };
        let (base_val, disp) = if mode == 0 && base_low == 5 {
            (0u64, self.fetch_u32(mem)? as i32 as i64)
        } else {
            let d = match mode {
                1 => self.fetch_u8(mem)? as i8 as i64,
                2 => self.fetch_u32(mem)? as i32 as i64,
                _ => 0,
            };
            (self.gpr[base_reg as usize], d)
        };
        Ok(base_val.wrapping_add(index_val).wrapping_add_signed(disp))
    }

    fn set_flag(&mut self, mask: u64, on: bool) {
        if on {
            self.rflags |= mask;
        } else {
            self.rflags &= !mask;
        }
    }

    fn update_zf_sf(&mut self, res: u64, bits: u8) {
        let masked = mask_bits(res, bits);
        let sign_bit = 1u64 << (bits - 1);
        self.set_flag(ZF, masked == 0);
        self.set_flag(SF, (masked & sign_bit) != 0);
    }

    fn cond_true(&self, cc: u8) -> bool {
        let cf = (self.rflags & CF) != 0;
        let zf = (self.rflags & ZF) != 0;
        let sf = (self.rflags & SF) != 0;
        let of = (self.rflags & OF) != 0;
        let res = match (cc & 0x0F) >> 1 {
            0 => of,
            1 => cf,
            2 => zf,
            3 => cf || zf,
            4 => sf,
            5 => false, // parity seldom used in modern C++; UCOMISS sets CF/ZF
            6 => sf != of,
            _ => zf || (sf != of),
        };
        if (cc & 1) != 0 { !res } else { res }
    }

    fn push64(&mut self, mem: &mut PagedMemory, val: u64) -> Result<()> {
        let rsp = self.gpr[4].wrapping_sub(8);
        mem.write_u64(rsp, val)?;
        self.gpr[4] = rsp;
        Ok(())
    }

    fn pop64(&mut self, mem: &PagedMemory) -> Result<u64> {
        let rsp = self.gpr[4];
        let val = mem.read_u64(rsp)?;
        self.gpr[4] = rsp.wrapping_add(8);
        Ok(val)
    }

    fn step(&mut self, mem: &mut PagedMemory) -> Result<()> {
        let mut pfx = Prefixes::default();
        loop {
            let b = self.fetch_u8(mem)?;
            match b {
                0x66 => pfx.op66 = true,
                0xF3 => pfx.rep_f3 = true,
                0xF2 => pfx.rep_f2 = true,
                0x65 => pfx.seg_gs = true,
                0xF0 | 0x2E | 0x3E | 0x26 | 0x36 | 0x64 | 0x67 => {}
                0x40..=0x4F => pfx.rex = b,
                0xC5 => return self.step_vex2(mem, pfx),
                0xC4 => return self.step_vex3(mem, pfx),
                0x0F => return self.step_0f(mem, pfx),
                op => return self.step_primary(mem, pfx, op),
            }
        }
    }

    fn apply_pp_prefix(pp: u8, pfx: &mut Prefixes) {
        match pp & 3 {
            1 => pfx.op66 = true,
            2 => pfx.rep_f3 = true,
            3 => pfx.rep_f2 = true,
            _ => {}
        }
    }

    fn step_vex2(&mut self, mem: &mut PagedMemory, mut pfx: Prefixes) -> Result<()> {
        let b1 = self.fetch_u8(mem)?;
        let r_inv = (b1 >> 7) & 1;
        pfx.rex = 0x40 | ((r_inv ^ 1) << 2);
        pfx.vex_vvvv = Some((!b1 >> 3) & 0x0F);
        Self::apply_pp_prefix(b1, &mut pfx);
        let op = self.fetch_u8(mem)?;
        self.step_sse_op(mem, pfx, op)
    }

    fn step_vex3(&mut self, mem: &mut PagedMemory, mut pfx: Prefixes) -> Result<()> {
        let b1 = self.fetch_u8(mem)?;
        let b2 = self.fetch_u8(mem)?;
        let r = ((b1 >> 7) & 1) ^ 1;
        let x = ((b1 >> 6) & 1) ^ 1;
        let b = ((b1 >> 5) & 1) ^ 1;
        let w = (b2 >> 7) & 1;
        pfx.rex = 0x40 | (w << 3) | (r << 2) | (x << 1) | b;
        pfx.vex_vvvv = Some((!b2 >> 3) & 0x0F);
        Self::apply_pp_prefix(b2, &mut pfx);
        let op = self.fetch_u8(mem)?;
        if (b1 & 0x1F) == 1 { self.step_sse_op(mem, pfx, op) } else { self.step_0f38_or_3a(mem, pfx, b1 & 0x1F, op) }
    }

    fn step_primary(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        match op {
            0x00..=0x3D if (op & 7) <= 5 => self.exec_std_alu(mem, pfx, op),
            0x50..=0x57 => {
                let r = (op - 0x50) | (pfx.rex_b() << 3);
                let val = self.gpr[r as usize];
                self.push64(mem, val)
            }
            0x58..=0x5F => {
                let r = (op - 0x58) | (pfx.rex_b() << 3);
                self.gpr[r as usize] = self.pop64(mem)?;
                Ok(())
            }
            0x63 => {
                // MOVSXD r64, r/m32
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let v = self.read_rm(mem, loc, 32, pfx.has_rex())? as u32 as i32 as i64 as u64;
                self.write_reg(reg, pfx.op_bits(), pfx.has_rex(), v);
                Ok(())
            }
            0x68 => {
                let imm = self.fetch_u32(mem)? as i32 as i64 as u64;
                self.push64(mem, imm)
            }
            0x6A => {
                let imm = self.fetch_u8(mem)? as i8 as i64 as u64;
                self.push64(mem, imm)
            }
            0x69 | 0x6B => {
                let bits = pfx.op_bits();
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let rhs = if op == 0x6B {
                    self.fetch_u8(mem)? as i8 as i64
                } else if bits == 16 {
                    self.fetch_u16(mem)? as i16 as i64
                } else {
                    self.fetch_u32(mem)? as i32 as i64
                };
                let lhs = sign_extend(self.read_rm(mem, loc, bits, pfx.has_rex())?, bits);
                let prod = lhs.wrapping_mul(rhs) as u64;
                self.write_reg(reg, bits, pfx.has_rex(), prod);
                Ok(())
            }
            0x70..=0x7F => {
                let rel = self.fetch_u8(mem)? as i8 as i64;
                if self.cond_true(op - 0x70) {
                    self.rip = self.rip.wrapping_add_signed(rel);
                }
                Ok(())
            }
            0x80..=0x83 => self.exec_imm_alu(mem, pfx, op),
            0x84 | 0x85 => {
                let bits = if op == 0x84 { 8 } else { pfx.op_bits() };
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let a = self.read_rm(mem, loc, bits, pfx.has_rex())?;
                let b = self.read_reg(reg, bits, pfx.has_rex());
                self.set_flag(CF | OF, false);
                self.update_zf_sf(a & b, bits);
                Ok(())
            }
            0x86 | 0x87 => {
                let bits = if op == 0x86 { 8 } else { pfx.op_bits() };
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let a = self.read_rm(mem, loc, bits, pfx.has_rex())?;
                let b = self.read_reg(reg, bits, pfx.has_rex());
                self.write_rm(mem, loc, bits, pfx.has_rex(), b)?;
                self.write_reg(reg, bits, pfx.has_rex(), a);
                Ok(())
            }
            0x88..=0x8B => self.exec_mov(mem, pfx, op),
            0x8D => {
                let bits = pfx.op_bits();
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                if let Some(addr) = self.resolve_addr(loc) {
                    self.write_reg(reg, bits, pfx.has_rex(), addr);
                }
                Ok(())
            }
            0x90..=0x97 => {
                let r = (op - 0x90) | (pfx.rex_b() << 3);
                if r != 0 {
                    let bits = pfx.op_bits();
                    let a = self.read_reg(0, bits, pfx.has_rex());
                    let b = self.read_reg(r, bits, pfx.has_rex());
                    self.write_reg(0, bits, pfx.has_rex(), b);
                    self.write_reg(r, bits, pfx.has_rex(), a);
                }
                Ok(())
            }
            0x98 => {
                if pfx.rex_w() {
                    self.gpr[0] = (self.gpr[0] as u32 as i32 as i64) as u64;
                } else if pfx.op66 {
                    self.write_reg(0, 16, true, (self.gpr[0] as u8 as i8 as i16) as u16 as u64);
                } else {
                    self.gpr[0] = (self.gpr[0] as u16 as i16 as i32) as u32 as u64;
                }
                Ok(())
            }
            0x99 => {
                if pfx.rex_w() {
                    self.gpr[2] = if (self.gpr[0] as i64) < 0 { u64::MAX } else { 0 };
                } else {
                    self.gpr[2] = if (self.gpr[0] as u32 as i32) < 0 { 0xFFFF_FFFF } else { 0 };
                }
                Ok(())
            }
            0xA4 | 0xA5 | 0xAA | 0xAB => self.exec_string_op(mem, pfx, op),
            0xA8 | 0xA9 => {
                let bits = if op == 0xA8 { 8 } else { pfx.op_bits() };
                let imm = match bits {
                    8 => u64::from(self.fetch_u8(mem)?),
                    16 => u64::from(self.fetch_u16(mem)?),
                    _ => self.fetch_u32(mem)? as i32 as i64 as u64,
                };
                let a = self.read_reg(0, bits, pfx.has_rex());
                self.set_flag(CF | OF, false);
                self.update_zf_sf(a & imm, bits);
                Ok(())
            }
            0xB0..=0xB7 => {
                let r = (op - 0xB0) | (pfx.rex_b() << 3);
                let imm = u64::from(self.fetch_u8(mem)?);
                self.write_reg(r, 8, pfx.has_rex(), imm);
                Ok(())
            }
            0xB8..=0xBF => {
                let r = (op - 0xB8) | (pfx.rex_b() << 3);
                let bits = pfx.op_bits();
                let imm = match bits {
                    16 => u64::from(self.fetch_u16(mem)?),
                    32 => u64::from(self.fetch_u32(mem)?),
                    _ => self.fetch_u64(mem)?,
                };
                self.write_reg(r, bits, pfx.has_rex(), imm);
                Ok(())
            }
            0xC0 | 0xC1 | 0xD0..=0xD3 => self.exec_shift(mem, pfx, op),
            0xC2 => {
                let pop_extra = u64::from(self.fetch_u16(mem)?);
                let ret = self.pop64(mem)?;
                self.gpr[4] = self.gpr[4].wrapping_add(pop_extra);
                self.rip = ret;
                Ok(())
            }
            0xC3 => {
                self.rip = self.pop64(mem)?;
                Ok(())
            }
            0xC6 | 0xC7 => {
                let bits = if op == 0xC6 { 8 } else { pfx.op_bits() };
                let (_sub, loc) = self.decode_modrm(mem, pfx)?;
                let imm = match bits {
                    8 => u64::from(self.fetch_u8(mem)?),
                    16 => u64::from(self.fetch_u16(mem)?),
                    _ => self.fetch_u32(mem)? as i32 as i64 as u64,
                };
                self.write_rm(mem, loc, bits, pfx.has_rex(), imm)
            }
            0xC9 => {
                // LEAVE
                self.gpr[4] = self.gpr[5];
                self.gpr[5] = self.pop64(mem)?;
                Ok(())
            }
            0xCC => Err(PluginHostError::CpuTrap(format!("INT3 breakpoint at {:#x}", self.rip - 1))),
            0xE8 => {
                let rel = self.fetch_u32(mem)? as i32 as i64;
                let ret_ip = self.rip;
                self.push64(mem, ret_ip)?;
                self.rip = ret_ip.wrapping_add_signed(rel);
                Ok(())
            }
            0xE9 => {
                let rel = self.fetch_u32(mem)? as i32 as i64;
                self.rip = self.rip.wrapping_add_signed(rel);
                Ok(())
            }
            0xEB => {
                let rel = self.fetch_u8(mem)? as i8 as i64;
                self.rip = self.rip.wrapping_add_signed(rel);
                Ok(())
            }
            0xF5 => {
                self.set_flag(CF, (self.rflags & CF) == 0);
                Ok(())
            }
            0xF8 => {
                self.set_flag(CF, false);
                Ok(())
            }
            0xF9 => {
                self.set_flag(CF, true);
                Ok(())
            }
            0xFC => {
                self.set_flag(DF, false);
                Ok(())
            }
            0xFD => {
                self.set_flag(DF, true);
                Ok(())
            }
            0xF6 | 0xF7 => self.exec_grp3(mem, pfx, op),
            0xFE | 0xFF => self.exec_grp5(mem, pfx, op),
            other => Err(PluginHostError::CpuTrap(format!("unimplemented x86_64 opcode {other:#04x} at {:#x}", self.rip - 1))),
        }
    }

    fn exec_mov(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        let bits = if (op & 1) == 0 { 8 } else { pfx.op_bits() };
        let (reg, loc) = self.decode_modrm(mem, pfx)?;
        if (op & 2) == 0 {
            let v = self.read_reg(reg, bits, pfx.has_rex());
            self.write_rm(mem, loc, bits, pfx.has_rex(), v)
        } else {
            let v = self.read_rm(mem, loc, bits, pfx.has_rex())?;
            self.write_reg(reg, bits, pfx.has_rex(), v);
            Ok(())
        }
    }

    fn exec_std_alu(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        let alu_kind = op >> 3;
        let sub = op & 7;
        let bits = if (sub & 1) == 0 { 8 } else { pfx.op_bits() };
        if sub >= 4 {
            let imm = match bits {
                8 => u64::from(self.fetch_u8(mem)?),
                16 => u64::from(self.fetch_u16(mem)?),
                _ => self.fetch_u32(mem)? as i32 as i64 as u64,
            };
            let lhs = self.read_reg(0, bits, pfx.has_rex());
            let res = self.alu_compute(alu_kind, lhs, imm, bits);
            if alu_kind != 7 {
                self.write_reg(0, bits, pfx.has_rex(), res);
            }
            return Ok(());
        }
        let (reg, loc) = self.decode_modrm(mem, pfx)?;
        if (sub & 2) == 0 {
            let lhs = self.read_rm(mem, loc, bits, pfx.has_rex())?;
            let rhs = self.read_reg(reg, bits, pfx.has_rex());
            let res = self.alu_compute(alu_kind, lhs, rhs, bits);
            if alu_kind != 7 {
                self.write_rm(mem, loc, bits, pfx.has_rex(), res)?;
            }
        } else {
            let lhs = self.read_reg(reg, bits, pfx.has_rex());
            let rhs = self.read_rm(mem, loc, bits, pfx.has_rex())?;
            let res = self.alu_compute(alu_kind, lhs, rhs, bits);
            if alu_kind != 7 {
                self.write_reg(reg, bits, pfx.has_rex(), res);
            }
        }
        Ok(())
    }

    fn exec_imm_alu(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        let bits = if op == 0x80 || op == 0x82 { 8 } else { pfx.op_bits() };
        let (sub_op, loc) = self.decode_modrm(mem, pfx)?;
        let alu_kind = sub_op & 7;
        let imm = if op == 0x83 {
            self.fetch_u8(mem)? as i8 as i64 as u64
        } else {
            match bits {
                8 => u64::from(self.fetch_u8(mem)?),
                16 => u64::from(self.fetch_u16(mem)?),
                _ => self.fetch_u32(mem)? as i32 as i64 as u64,
            }
        };
        let lhs = self.read_rm(mem, loc, bits, pfx.has_rex())?;
        let res = self.alu_compute(alu_kind, lhs, imm, bits);
        if alu_kind != 7 {
            self.write_rm(mem, loc, bits, pfx.has_rex(), res)?;
        }
        Ok(())
    }

    fn alu_compute(&mut self, kind: u8, lhs_raw: u64, rhs_raw: u64, bits: u8) -> u64 {
        let lhs = mask_bits(lhs_raw, bits);
        let rhs = mask_bits(rhs_raw, bits);
        let sign_mask = 1u64 << (bits - 1);
        let cf_in = u64::from((self.rflags & CF) != 0);
        let res = match kind {
            0 => {
                // ADD
                let (sum, c) = add_with_carry(lhs, rhs, 0, bits);
                let ov = ((!(lhs ^ rhs) & (lhs ^ sum)) & sign_mask) != 0;
                self.set_flag(CF, c);
                self.set_flag(OF, ov);
                sum
            }
            1 => {
                self.set_flag(CF | OF, false);
                lhs | rhs
            }
            2 => {
                // ADC
                let (sum, c) = add_with_carry(lhs, rhs, cf_in, bits);
                let ov = ((!(lhs ^ rhs) & (lhs ^ sum)) & sign_mask) != 0;
                self.set_flag(CF, c);
                self.set_flag(OF, ov);
                sum
            }
            3 => {
                // SBB
                let (diff, b) = sub_with_borrow(lhs, rhs, cf_in, bits);
                let ov = (((lhs ^ rhs) & (lhs ^ diff)) & sign_mask) != 0;
                self.set_flag(CF, b);
                self.set_flag(OF, ov);
                diff
            }
            4 => {
                self.set_flag(CF | OF, false);
                lhs & rhs
            }
            5 | 7 => {
                // SUB / CMP
                let (diff, b) = sub_with_borrow(lhs, rhs, 0, bits);
                let ov = (((lhs ^ rhs) & (lhs ^ diff)) & sign_mask) != 0;
                self.set_flag(CF, b);
                self.set_flag(OF, ov);
                diff
            }
            _ => {
                self.set_flag(CF | OF, false);
                lhs ^ rhs
            }
        };
        self.update_zf_sf(res, bits);
        res
    }

    fn exec_shift(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        let bits = if (op & 1) == 0 { 8 } else { pfx.op_bits() };
        let (sub, loc) = self.decode_modrm(mem, pfx)?;
        let count_raw = match op {
            0xC0 | 0xC1 => self.fetch_u8(mem)?,
            0xD0 | 0xD1 => 1,
            _ => (self.gpr[1] & 0xFF) as u8,
        };
        let mask = if bits == 64 { 63 } else { 31 };
        let cnt = u32::from(count_raw & mask);
        let val = mask_bits(self.read_rm(mem, loc, bits, pfx.has_rex())?, bits);
        if cnt == 0 {
            return Ok(());
        }
        let out = match sub & 7 {
            0 => {
                // ROL
                let r = cnt % u32::from(bits);
                if r == 0 { val } else { mask_bits((val << r) | (val >> (u32::from(bits) - r)), bits) }
            }
            1 => {
                // ROR
                let r = cnt % u32::from(bits);
                if r == 0 { val } else { mask_bits((val >> r) | (val << (u32::from(bits) - r)), bits) }
            }
            4 | 6 => {
                // SHL / SAL
                let cf = ((val >> (u32::from(bits) - cnt)) & 1) != 0;
                let res = mask_bits(val << cnt, bits);
                self.set_flag(CF, cf);
                self.update_zf_sf(res, bits);
                res
            }
            5 => {
                // SHR
                let cf = ((val >> (cnt - 1)) & 1) != 0;
                let res = val >> cnt;
                self.set_flag(CF, cf);
                self.update_zf_sf(res, bits);
                res
            }
            7 => {
                // SAR
                let sval = sign_extend(val, bits);
                let cf = ((sval >> (cnt - 1)) & 1) != 0;
                let res = mask_bits((sval >> cnt) as u64, bits);
                self.set_flag(CF, cf);
                self.update_zf_sf(res, bits);
                res
            }
            _ => val,
        };
        self.write_rm(mem, loc, bits, pfx.has_rex(), out)
    }

    fn exec_grp3(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        let bits = if op == 0xF6 { 8 } else { pfx.op_bits() };
        let (sub, loc) = self.decode_modrm(mem, pfx)?;
        if (sub & 7) <= 1 {
            // TEST r/m, imm (fetch immediate BEFORE resolving RipRel!)
            let imm = match bits {
                8 => u64::from(self.fetch_u8(mem)?),
                16 => u64::from(self.fetch_u16(mem)?),
                _ => self.fetch_u32(mem)? as i32 as i64 as u64,
            };
            let val = self.read_rm(mem, loc, bits, pfx.has_rex())?;
            self.set_flag(CF | OF, false);
            self.update_zf_sf(val & imm, bits);
            return Ok(());
        }
        let val = self.read_rm(mem, loc, bits, pfx.has_rex())?;
        match sub & 7 {
            2 => self.write_rm(mem, loc, bits, pfx.has_rex(), mask_bits(!val, bits)),
            3 => {
                let res = self.alu_compute(5, 0, val, bits);
                self.write_rm(mem, loc, bits, pfx.has_rex(), res)
            }
            4 => {
                // MUL unsigned
                if bits == 64 {
                    let full = u128::from(self.gpr[0]) * u128::from(val);
                    self.gpr[0] = full as u64;
                    self.gpr[2] = (full >> 64) as u64;
                } else if bits == 32 {
                    let full = (self.gpr[0] & 0xFFFF_FFFF) * (val & 0xFFFF_FFFF);
                    self.gpr[0] = full & 0xFFFF_FFFF;
                    self.gpr[2] = (full >> 32) & 0xFFFF_FFFF;
                } else {
                    let full = (self.gpr[0] & 0xFF) * (val & 0xFF);
                    self.write_reg(0, 16, true, full);
                }
                Ok(())
            }
            5 => {
                // IMUL signed 1-op
                if bits == 64 {
                    let full = i128::from(self.gpr[0] as i64) * i128::from(val as i64);
                    self.gpr[0] = full as u64;
                    self.gpr[2] = (full >> 64) as u64;
                } else {
                    let full = sign_extend(self.read_reg(0, bits, pfx.has_rex()), bits) * sign_extend(val, bits);
                    self.write_reg(0, bits, pfx.has_rex(), full as u64);
                    if bits == 32 {
                        self.gpr[2] = ((full as u64) >> 32) & 0xFFFF_FFFF;
                    }
                }
                Ok(())
            }
            6 => self.exec_div_unsigned(val, bits),
            _ => self.exec_div_signed(val, bits),
        }
    }

    fn exec_div_unsigned(&mut self, divisor: u64, bits: u8) -> Result<()> {
        let d = mask_bits(divisor, bits);
        if d == 0 {
            return Err(PluginHostError::CpuTrap("unsigned division by zero".into()));
        }
        if bits == 64 {
            let num = (u128::from(self.gpr[2]) << 64) | u128::from(self.gpr[0]);
            self.gpr[0] = (num / u128::from(d)) as u64;
            self.gpr[2] = (num % u128::from(d)) as u64;
        } else if bits == 32 {
            let num = ((self.gpr[2] & 0xFFFF_FFFF) << 32) | (self.gpr[0] & 0xFFFF_FFFF);
            self.gpr[0] = (num / d) & 0xFFFF_FFFF;
            self.gpr[2] = (num % d) & 0xFFFF_FFFF;
        }
        Ok(())
    }

    fn exec_div_signed(&mut self, divisor: u64, bits: u8) -> Result<()> {
        let d = sign_extend(divisor, bits);
        if d == 0 {
            return Err(PluginHostError::CpuTrap("signed division by zero".into()));
        }
        if bits == 64 {
            let num = (i128::from(self.gpr[2] as i64) << 64) | i128::from(self.gpr[0]);
            self.gpr[0] = (num.wrapping_div(i128::from(d))) as u64;
            self.gpr[2] = (num.wrapping_rem(i128::from(d))) as u64;
        } else if bits == 32 {
            let hi = (self.gpr[2] as u32 as i32 as i64) << 32;
            let lo = self.gpr[0] & 0xFFFF_FFFF;
            let num = hi | (lo as i64);
            self.gpr[0] = (num.wrapping_div(d) as u32) as u64;
            self.gpr[2] = (num.wrapping_rem(d) as u32) as u64;
        }
        Ok(())
    }

    fn exec_grp5(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        let bits = if op == 0xFE { 8 } else { pfx.op_bits() };
        let (sub, loc) = self.decode_modrm(mem, pfx)?;
        match sub & 7 {
            0 => {
                // INC (preserves CF)
                let old_cf = (self.rflags & CF) != 0;
                let v = self.read_rm(mem, loc, bits, pfx.has_rex())?;
                let res = self.alu_compute(0, v, 1, bits);
                self.set_flag(CF, old_cf);
                self.write_rm(mem, loc, bits, pfx.has_rex(), res)
            }
            1 => {
                // DEC (preserves CF)
                let old_cf = (self.rflags & CF) != 0;
                let v = self.read_rm(mem, loc, bits, pfx.has_rex())?;
                let res = self.alu_compute(5, v, 1, bits);
                self.set_flag(CF, old_cf);
                self.write_rm(mem, loc, bits, pfx.has_rex(), res)
            }
            2 if op == 0xFF => {
                let target = self.read_rm(mem, loc, 64, true)?;
                let ret = self.rip;
                self.push64(mem, ret)?;
                self.rip = target;
                Ok(())
            }
            4 if op == 0xFF => {
                self.rip = self.read_rm(mem, loc, 64, true)?;
                Ok(())
            }
            6 if op == 0xFF => {
                let v = self.read_rm(mem, loc, 64, true)?;
                self.push64(mem, v)
            }
            other => Err(PluginHostError::CpuTrap(format!("unimplemented GRP5 /{other} at {:#x}", self.rip))),
        }
    }

    fn exec_string_op(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        let elem = if (op & 1) == 0 {
            1u64
        } else if pfx.rex_w() {
            8
        } else if pfx.op66 {
            2
        } else {
            4
        };
        let step_dir: i64 = if (self.rflags & DF) != 0 { -(elem as i64) } else { elem as i64 };
        let count = if pfx.rep_f3 || pfx.rep_f2 { self.gpr[1].min(16 << 20) } else { 1 };
        for _ in 0..count {
            if op == 0xA4 || op == 0xA5 {
                let v = match elem {
                    1 => u64::from(mem.read_u8(self.gpr[6])?),
                    2 => u64::from(mem.read_u16(self.gpr[6])?),
                    4 => u64::from(mem.read_u32(self.gpr[6])?),
                    _ => mem.read_u64(self.gpr[6])?,
                };
                match elem {
                    1 => mem.write_u8(self.gpr[7], v as u8)?,
                    2 => mem.write_u16(self.gpr[7], v as u16)?,
                    4 => mem.write_u32(self.gpr[7], v as u32)?,
                    _ => mem.write_u64(self.gpr[7], v)?,
                }
                self.gpr[6] = self.gpr[6].wrapping_add_signed(step_dir);
                self.gpr[7] = self.gpr[7].wrapping_add_signed(step_dir);
            } else {
                let v = self.gpr[0];
                match elem {
                    1 => mem.write_u8(self.gpr[7], v as u8)?,
                    2 => mem.write_u16(self.gpr[7], v as u16)?,
                    4 => mem.write_u32(self.gpr[7], v as u32)?,
                    _ => mem.write_u64(self.gpr[7], v)?,
                }
                self.gpr[7] = self.gpr[7].wrapping_add_signed(step_dir);
            }
        }
        if pfx.rep_f3 || pfx.rep_f2 {
            self.gpr[1] = 0;
        }
        Ok(())
    }

    fn step_0f(&mut self, mem: &mut PagedMemory, pfx: Prefixes) -> Result<()> {
        let op = self.fetch_u8(mem)?;
        match op {
            0x01 => {
                let next = self.fetch_u8(mem)?;
                if next == 0xD0 {
                    // XGETBV -> XMM state enabled (bits 1..2), no AVX-512
                    self.gpr[0] = 0x06;
                    self.gpr[2] = 0;
                    return Ok(());
                }
                Err(PluginHostError::CpuTrap(format!("unimplemented 0F 01 {next:02x}")))
            }
            0x1F => {
                // Multi-byte NOP
                let _ = self.decode_modrm(mem, pfx)?;
                Ok(())
            }
            0x31 => {
                // RDTSC
                self.gpr[0] = 0x1234_5678;
                self.gpr[2] = 0;
                Ok(())
            }
            0xA2 => {
                // CPUID (used by MSVC CRT `__isa_available_init`)
                match self.gpr[0] as u32 {
                    0 => {
                        self.gpr[0] = 7;
                        self.gpr[3] = 0x756E_6547; // "Genu"
                        self.gpr[2] = 0x4965_6E69; // "ineI"
                        self.gpr[1] = 0x6C65_746E; // "ntel"
                    }
                    1 => {
                        self.gpr[0] = 0x0003_06C3;
                        self.gpr[3] = 0x0001_0800;
                        // SSE3 (0) | SSSE3 (9) | SSE4.1 (19) | SSE4.2 (20) | POPCNT (23)
                        self.gpr[1] = (1 << 0) | (1 << 9) | (1 << 19) | (1 << 20) | (1 << 23);
                        // FPU (0) | TSC (4) | CMOV (15) | MMX (23) | FXSR (24) | SSE (25) | SSE2 (26)
                        self.gpr[2] = (1 << 0) | (1 << 4) | (1 << 15) | (1 << 23) | (1 << 24) | (1 << 25) | (1 << 26);
                    }
                    _ => {
                        self.gpr[0] = 0;
                        self.gpr[3] = 0;
                        self.gpr[1] = 0;
                        self.gpr[2] = 0;
                    }
                }
                Ok(())
            }
            0x38 => {
                let sub = self.fetch_u8(mem)?;
                self.step_0f38_or_3a(mem, pfx, 2, sub)
            }
            0x3A => {
                let sub = self.fetch_u8(mem)?;
                self.step_0f38_or_3a(mem, pfx, 3, sub)
            }
            0x40..=0x4F => {
                // CMOVcc
                let bits = pfx.op_bits();
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let v = self.read_rm(mem, loc, bits, pfx.has_rex())?;
                if self.cond_true(op - 0x40) {
                    self.write_reg(reg, bits, pfx.has_rex(), v);
                } else if bits == 32 {
                    let cur = self.read_reg(reg, 32, pfx.has_rex());
                    self.write_reg(reg, 32, pfx.has_rex(), cur);
                }
                Ok(())
            }
            0x80..=0x8F => {
                let rel = self.fetch_u32(mem)? as i32 as i64;
                if self.cond_true(op - 0x80) {
                    self.rip = self.rip.wrapping_add_signed(rel);
                }
                Ok(())
            }
            0x90..=0x9F => {
                // SETcc
                let (_reg, loc) = self.decode_modrm(mem, pfx)?;
                let b = u64::from(self.cond_true(op - 0x90));
                self.write_rm(mem, loc, 8, pfx.has_rex(), b)
            }
            0xA3 | 0xAB | 0xB3 | 0xBB | 0xBA => self.exec_bit_test(mem, pfx, op),
            0xA4 | 0xA5 | 0xAC | 0xAD => {
                // SHLD / SHRD
                let bits = pfx.op_bits();
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let cnt = u32::from(if (op & 1) == 0 { self.fetch_u8(mem)? } else { self.gpr[1] as u8 } & 63);
                let dst = mask_bits(self.read_rm(mem, loc, bits, pfx.has_rex())?, bits);
                let src = mask_bits(self.read_reg(reg, bits, pfx.has_rex()), bits);
                if cnt > 0 && cnt < u32::from(bits) {
                    let out = if op < 0xAC {
                        mask_bits((dst << cnt) | (src >> (u32::from(bits) - cnt)), bits)
                    } else {
                        mask_bits((dst >> cnt) | (src << (u32::from(bits) - cnt)), bits)
                    };
                    self.update_zf_sf(out, bits);
                    self.write_rm(mem, loc, bits, pfx.has_rex(), out)?;
                }
                Ok(())
            }
            0xAF => {
                // IMUL r, r/m
                let bits = pfx.op_bits();
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let a = sign_extend(self.read_reg(reg, bits, pfx.has_rex()), bits);
                let b = sign_extend(self.read_rm(mem, loc, bits, pfx.has_rex())?, bits);
                self.write_reg(reg, bits, pfx.has_rex(), a.wrapping_mul(b) as u64);
                Ok(())
            }
            0xB0 | 0xB1 => {
                // CMPXCHG
                let bits = if op == 0xB0 { 8 } else { pfx.op_bits() };
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let dst = self.read_rm(mem, loc, bits, pfx.has_rex())?;
                let acc = self.read_reg(0, bits, pfx.has_rex());
                let _ = self.alu_compute(7, acc, dst, bits);
                if mask_bits(acc, bits) == mask_bits(dst, bits) {
                    let src = self.read_reg(reg, bits, pfx.has_rex());
                    self.write_rm(mem, loc, bits, pfx.has_rex(), src)?;
                } else {
                    self.write_reg(0, bits, pfx.has_rex(), dst);
                }
                Ok(())
            }
            0xB6 | 0xB7 => {
                // MOVZX
                let src_bits = if op == 0xB6 { 8 } else { 16 };
                let dst_bits = pfx.op_bits();
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let v = self.read_rm(mem, loc, src_bits, pfx.has_rex())?;
                self.write_reg(reg, dst_bits, pfx.has_rex(), v);
                Ok(())
            }
            0xBE | 0xBF => {
                // MOVSX
                let src_bits = if op == 0xBE { 8 } else { 16 };
                let dst_bits = pfx.op_bits();
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let v = sign_extend(self.read_rm(mem, loc, src_bits, pfx.has_rex())?, src_bits) as u64;
                self.write_reg(reg, dst_bits, pfx.has_rex(), v);
                Ok(())
            }
            0xBC | 0xBD => {
                // BSF/TZCNT and BSR/LZCNT
                let bits = pfx.op_bits();
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let v = mask_bits(self.read_rm(mem, loc, bits, pfx.has_rex())?, bits);
                if v == 0 {
                    self.set_flag(ZF, true);
                    if pfx.rep_f3 {
                        self.write_reg(reg, bits, pfx.has_rex(), u64::from(bits));
                    }
                } else {
                    self.set_flag(ZF, false);
                    let idx = if op == 0xBC {
                        u64::from(v.trailing_zeros())
                    } else if pfx.rep_f3 {
                        u64::from(v.leading_zeros() - (64 - u32::from(bits)))
                    } else {
                        u64::from(63 - v.leading_zeros())
                    };
                    self.write_reg(reg, bits, pfx.has_rex(), idx);
                }
                Ok(())
            }
            0xC0 | 0xC1 => {
                // XADD
                let bits = if op == 0xC0 { 8 } else { pfx.op_bits() };
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let a = self.read_rm(mem, loc, bits, pfx.has_rex())?;
                let b = self.read_reg(reg, bits, pfx.has_rex());
                let sum = self.alu_compute(0, a, b, bits);
                self.write_reg(reg, bits, pfx.has_rex(), a);
                self.write_rm(mem, loc, bits, pfx.has_rex(), sum)
            }
            0xC8..=0xCF => {
                let r = ((op - 0xC8) | (pfx.rex_b() << 3)) as usize;
                if pfx.rex_w() {
                    self.gpr[r] = self.gpr[r].swap_bytes();
                } else {
                    self.gpr[r] = u64::from((self.gpr[r] as u32).swap_bytes());
                }
                Ok(())
            }
            _ => self.step_sse_op(mem, pfx, op),
        }
    }

    fn exec_bit_test(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        let bits = pfx.op_bits();
        let (reg_or_sub, loc) = self.decode_modrm(mem, pfx)?;
        let (action, bit_idx) = if op == 0xBA {
            let imm = self.fetch_u8(mem)?;
            (reg_or_sub & 7, u64::from(imm))
        } else {
            let act = match op {
                0xA3 => 4,
                0xAB => 5,
                0xB3 => 6,
                _ => 7,
            };
            (act, self.read_reg(reg_or_sub, bits, pfx.has_rex()))
        };
        let resolved_loc = match loc {
            RmLoc::RipRel(d) => RmLoc::Mem(self.rip.wrapping_add_signed(i64::from(d))),
            other => other,
        };
        let (target_loc, shift) = match resolved_loc {
            RmLoc::Reg(_) => (resolved_loc, (bit_idx % u64::from(bits)) as u32),
            RmLoc::Mem(base) => {
                let elem_bytes = i64::from(bits / 8);
                let sbit = sign_extend(bit_idx, bits);
                let elem_off = sbit.div_euclid(i64::from(bits)) * elem_bytes;
                let rem_bit = sbit.rem_euclid(i64::from(bits)) as u32;
                (RmLoc::Mem(base.wrapping_add_signed(elem_off)), rem_bit)
            }
            RmLoc::RipRel(_) => (resolved_loc, 0),
        };
        let val = self.read_rm(mem, target_loc, bits, pfx.has_rex())?;
        let mask = 1u64 << shift;
        self.set_flag(CF, (val & mask) != 0);
        let updated = match action {
            5 => Some(val | mask),
            6 => Some(val & !mask),
            7 => Some(val ^ mask),
            _ => None,
        };
        if let Some(nv) = updated {
            self.write_rm(mem, target_loc, bits, pfx.has_rex(), nv)?;
        }
        Ok(())
    }

    fn step_0f38_or_3a(&mut self, mem: &mut PagedMemory, pfx: Prefixes, map: u8, op: u8) -> Result<()> {
        if map == 2 && op == 0x40 {
            // PMULLD
            let (reg, loc) = self.decode_modrm(mem, pfx)?;
            let lhs = self.xmm[pfx.vex_vvvv.unwrap_or(reg) as usize];
            let rhs = self.read_xmm_rm(mem, loc)?;
            let mut out = [0u8; 16];
            let lb = lhs.to_le_bytes();
            let rb = rhs.to_le_bytes();
            for lane in 0..4 {
                let o = lane * 4;
                let a = i32::from_le_bytes([lb[o], lb[o + 1], lb[o + 2], lb[o + 3]]);
                let b = i32::from_le_bytes([rb[o], rb[o + 1], rb[o + 2], rb[o + 3]]);
                out[o..o + 4].copy_from_slice(&a.wrapping_mul(b).to_le_bytes());
            }
            self.xmm[reg as usize] = u128::from_le_bytes(out);
            return Ok(());
        }
        if map == 3 && (op == 0x0A || op == 0x0B) {
            // ROUNDSS / ROUNDSD
            let (reg, loc) = self.decode_modrm(mem, pfx)?;
            let mode = self.fetch_u8(mem)? & 3;
            let src = self.read_xmm_scalar_rm(mem, loc, op == 0x0B)?;
            if op == 0x0A {
                let f = f32::from_bits(src as u32);
                let r = match mode {
                    1 => f.floor(),
                    2 => f.ceil(),
                    3 => f.trunc(),
                    _ => f.round_ties_even(),
                };
                let base = self.xmm[pfx.vex_vvvv.unwrap_or(reg) as usize];
                self.xmm[reg as usize] = (base & !0xFFFF_FFFF) | u128::from(r.to_bits());
            } else {
                let f = f64::from_bits(src as u64);
                let r = match mode {
                    1 => f.floor(),
                    2 => f.ceil(),
                    3 => f.trunc(),
                    _ => f.round_ties_even(),
                };
                let base = self.xmm[pfx.vex_vvvv.unwrap_or(reg) as usize];
                self.xmm[reg as usize] = (base & !0xFFFF_FFFF_FFFF_FFFF) | u128::from(r.to_bits());
            }
            return Ok(());
        }
        Err(PluginHostError::CpuTrap(format!("unimplemented 0F {map:02x} {op:02x}")))
    }

    fn read_xmm_rm(&self, mem: &PagedMemory, loc: RmLoc) -> Result<u128> {
        match loc {
            RmLoc::Reg(r) => Ok(self.xmm[(r & 15) as usize]),
            RmLoc::Mem(_) | RmLoc::RipRel(_) => mem.read_u128(self.resolve_addr(loc).unwrap_or(0)),
        }
    }

    fn read_xmm_scalar_rm(&self, mem: &PagedMemory, loc: RmLoc, is_f64: bool) -> Result<u128> {
        match loc {
            RmLoc::Reg(r) => Ok(self.xmm[(r & 15) as usize]),
            RmLoc::Mem(_) | RmLoc::RipRel(_) => {
                let addr = self.resolve_addr(loc).unwrap_or(0);
                if is_f64 { Ok(u128::from(mem.read_u64(addr)?)) } else { Ok(u128::from(mem.read_u32(addr)?)) }
            }
        }
    }

    fn step_sse_op(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        match op {
            0x10 | 0x28 | 0x6F => {
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                if pfx.rep_f3 && op == 0x10 {
                    let v = self.read_xmm_scalar_rm(mem, loc, false)?;
                    let dst = match loc {
                        RmLoc::Reg(_) => (self.xmm[pfx.vex_vvvv.unwrap_or(reg) as usize] & !0xFFFF_FFFF) | (v & 0xFFFF_FFFF),
                        RmLoc::Mem(_) | RmLoc::RipRel(_) => v & 0xFFFF_FFFF,
                    };
                    self.xmm[reg as usize] = dst;
                } else if pfx.rep_f2 && op == 0x10 {
                    let v = self.read_xmm_scalar_rm(mem, loc, true)?;
                    let dst = match loc {
                        RmLoc::Reg(_) => (self.xmm[pfx.vex_vvvv.unwrap_or(reg) as usize] & !0xFFFF_FFFF_FFFF_FFFF) | (v & 0xFFFF_FFFF_FFFF_FFFF),
                        RmLoc::Mem(_) | RmLoc::RipRel(_) => v & 0xFFFF_FFFF_FFFF_FFFF,
                    };
                    self.xmm[reg as usize] = dst;
                } else {
                    self.xmm[reg as usize] = self.read_xmm_rm(mem, loc)?;
                }
                Ok(())
            }
            0x11 | 0x29 | 0x7F => {
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let src = self.xmm[reg as usize];
                match loc {
                    RmLoc::Reg(r) => {
                        if pfx.rep_f3 && op == 0x11 {
                            self.xmm[r as usize] = (self.xmm[r as usize] & !0xFFFF_FFFF) | (src & 0xFFFF_FFFF);
                        } else if pfx.rep_f2 && op == 0x11 {
                            self.xmm[r as usize] = (self.xmm[r as usize] & !0xFFFF_FFFF_FFFF_FFFF) | (src & 0xFFFF_FFFF_FFFF_FFFF);
                        } else {
                            self.xmm[r as usize] = src;
                        }
                        Ok(())
                    }
                    RmLoc::Mem(_) | RmLoc::RipRel(_) => {
                        let addr = self.resolve_addr(loc).unwrap_or(0);
                        if pfx.rep_f3 && op == 0x11 {
                            mem.write_u32(addr, src as u32)
                        } else if pfx.rep_f2 && op == 0x11 {
                            mem.write_u64(addr, src as u64)
                        } else {
                            mem.write_u128(addr, src)
                        }
                    }
                }
            }
            0x2A => {
                // CVTSI2SS / CVTSI2SD
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let bits = if pfx.rex_w() { 64 } else { 32 };
                let iv = sign_extend(self.read_rm(mem, loc, bits, true)?, bits);
                let base = self.xmm[pfx.vex_vvvv.unwrap_or(reg) as usize];
                if pfx.rep_f2 {
                    self.xmm[reg as usize] = (base & !0xFFFF_FFFF_FFFF_FFFF) | u128::from((iv as f64).to_bits());
                } else {
                    self.xmm[reg as usize] = (base & !0xFFFF_FFFF) | u128::from((iv as f32).to_bits());
                }
                Ok(())
            }
            0x2C | 0x2D => {
                // CVTTSS2SI / CVTTSD2SI / CVTSS2SI / CVTSD2SI
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let src = self.read_xmm_scalar_rm(mem, loc, pfx.rep_f2)?;
                let iv = if pfx.rep_f2 {
                    let f = f64::from_bits(src as u64);
                    if op == 0x2C { f.trunc() as i64 } else { f.round() as i64 }
                } else {
                    let f = f32::from_bits(src as u32);
                    if op == 0x2C { f.trunc() as i64 } else { f.round() as i64 }
                };
                self.write_reg(reg, if pfx.rex_w() { 64 } else { 32 }, true, iv as u64);
                Ok(())
            }
            0x2E | 0x2F => {
                // UCOMISS / UCOMISD / COMISS / COMISD
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let rhs = self.read_xmm_scalar_rm(mem, loc, pfx.op66)?;
                let (unordered, lt, eq) = if pfx.op66 {
                    let a = f64::from_bits(self.xmm[reg as usize] as u64);
                    let b = f64::from_bits(rhs as u64);
                    (a.is_nan() || b.is_nan(), a < b, a == b)
                } else {
                    let a = f32::from_bits(self.xmm[reg as usize] as u32);
                    let b = f32::from_bits(rhs as u32);
                    (a.is_nan() || b.is_nan(), a < b, a == b)
                };
                self.set_flag(SF | OF, false);
                self.set_flag(ZF, unordered || eq);
                self.set_flag(CF, unordered || lt);
                Ok(())
            }
            0x51..=0x5F => self.exec_sse_math_logic(mem, pfx, op),
            0x60..=0x6D => self.exec_sse_unpack_pack(mem, pfx, op),
            0x71..=0x73 => self.exec_sse_imm_shift(mem, pfx, op),
            0x6E => {
                // MOVD / MOVQ xmm, r/m
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let bits = if pfx.rex_w() { 64 } else { 32 };
                self.xmm[reg as usize] = u128::from(self.read_rm(mem, loc, bits, true)?);
                Ok(())
            }
            0x7E => {
                // MOVD / MOVQ r/m, xmm (or F3 0F 7E MOVQ xmm1, xmm2/m64)
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                if pfx.rep_f3 {
                    let v = self.read_xmm_scalar_rm(mem, loc, true)?;
                    self.xmm[reg as usize] = v & 0xFFFF_FFFF_FFFF_FFFF;
                    Ok(())
                } else {
                    let bits = if pfx.rex_w() { 64 } else { 32 };
                    self.write_rm(mem, loc, bits, true, self.xmm[reg as usize] as u64)
                }
            }
            0x70 => {
                // PSHUFD
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let imm = self.fetch_u8(mem)?;
                let src = self.read_xmm_rm(mem, loc)?.to_le_bytes();
                let mut out = [0u8; 16];
                for i in 0..4 {
                    let sel = ((imm >> (i * 2)) & 3) as usize * 4;
                    out[i * 4..i * 4 + 4].copy_from_slice(&src[sel..sel + 4]);
                }
                self.xmm[reg as usize] = u128::from_le_bytes(out);
                Ok(())
            }
            0xC4 => {
                // PINSRW xmm, r32/m16, imm8
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let imm = (self.fetch_u8(mem)? & 7) as usize;
                let w = (self.read_rm(mem, loc, 16, true)? as u16).to_le_bytes();
                let mut dst = self.xmm[pfx.vex_vvvv.unwrap_or(reg) as usize].to_le_bytes();
                dst[imm * 2..imm * 2 + 2].copy_from_slice(&w);
                self.xmm[reg as usize] = u128::from_le_bytes(dst);
                Ok(())
            }
            0xC5 => {
                // PEXTRW r32, xmm, imm8
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let imm = (self.fetch_u8(mem)? & 7) as usize;
                let src = self.read_xmm_rm(mem, loc)?.to_le_bytes();
                let w = u16::from_le_bytes([src[imm * 2], src[imm * 2 + 1]]);
                self.write_reg(reg, 32, true, u64::from(w));
                Ok(())
            }
            0xC6 => {
                // SHUFPS
                let (reg, loc) = self.decode_modrm(mem, pfx)?;
                let imm = self.fetch_u8(mem)?;
                let a = self.xmm[pfx.vex_vvvv.unwrap_or(reg) as usize].to_le_bytes();
                let b = self.read_xmm_rm(mem, loc)?.to_le_bytes();
                let mut out = [0u8; 16];
                for i in 0..2 {
                    let s = ((imm >> (i * 2)) & 3) as usize * 4;
                    out[i * 4..i * 4 + 4].copy_from_slice(&a[s..s + 4]);
                }
                for i in 2..4 {
                    let s = ((imm >> (i * 2)) & 3) as usize * 4;
                    out[i * 4..i * 4 + 4].copy_from_slice(&b[s..s + 4]);
                }
                self.xmm[reg as usize] = u128::from_le_bytes(out);
                Ok(())
            }
            0xD4 | 0xDB | 0xEB | 0xEF | 0xFB | 0xFE | 0xFA => self.exec_packed_int_op(mem, pfx, op),
            other => Err(PluginHostError::CpuTrap(format!("unimplemented SSE opcode 0F {other:02x} at {:#x}", self.rip))),
        }
    }

    fn exec_sse_imm_shift(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        let (sub, loc) = self.decode_modrm(mem, pfx)?;
        let imm = self.fetch_u8(mem)?;
        let RmLoc::Reg(rm_reg) = loc else {
            return Err(PluginHostError::CpuTrap("SSE imm shift expects XMM register".into()));
        };
        let dst_reg = pfx.vex_vvvv.unwrap_or(rm_reg) as usize;
        let src = self.xmm[rm_reg as usize];
        self.xmm[dst_reg] = match (op, sub & 7) {
            (0x73, 3) => {
                // PSRLDQ (byte shift right)
                let bytes = u32::from(imm).min(16) * 8;
                if bytes >= 128 { 0 } else { src >> bytes }
            }
            (0x73, 7) => {
                // PSLLDQ (byte shift left)
                let bytes = u32::from(imm).min(16) * 8;
                if bytes >= 128 { 0 } else { src << bytes }
            }
            (0x73, 2) => {
                // PSRLQ
                let sh = u32::from(imm);
                let lo = src as u64;
                let hi = (src >> 64) as u64;
                if sh >= 64 { 0 } else { u128::from(lo >> sh) | (u128::from(hi >> sh) << 64) }
            }
            (0x73, 6) => {
                // PSLLQ
                let sh = u32::from(imm);
                let lo = src as u64;
                let hi = (src >> 64) as u64;
                if sh >= 64 { 0 } else { u128::from(lo << sh) | (u128::from(hi << sh) << 64) }
            }
            (0x72, mode) => {
                // PSRLD (2) / PSRAD (4) / PSLLD (6)
                let sb = src.to_le_bytes();
                let mut out = [0u8; 16];
                let sh = u32::from(imm);
                for i in 0..4 {
                    let o = i * 4;
                    let u = u32::from_le_bytes([sb[o], sb[o + 1], sb[o + 2], sb[o + 3]]);
                    let r = match mode {
                        2 => {
                            if sh >= 32 {
                                0
                            } else {
                                u >> sh
                            }
                        }
                        4 => ((u as i32) >> sh.min(31)) as u32,
                        _ => {
                            if sh >= 32 {
                                0
                            } else {
                                u << sh
                            }
                        }
                    };
                    out[o..o + 4].copy_from_slice(&r.to_le_bytes());
                }
                u128::from_le_bytes(out)
            }
            _ => {
                // 0x71: PSRLW (2) / PSRAW (4) / PSLLW (6)
                let sb = src.to_le_bytes();
                let mut out = [0u8; 16];
                let sh = u32::from(imm);
                for i in 0..8 {
                    let o = i * 2;
                    let u = u16::from_le_bytes([sb[o], sb[o + 1]]);
                    let r = match sub & 7 {
                        2 => {
                            if sh >= 16 {
                                0
                            } else {
                                u >> sh
                            }
                        }
                        4 => ((u as i16) >> sh.min(15)) as u16,
                        _ => {
                            if sh >= 16 {
                                0
                            } else {
                                u << sh
                            }
                        }
                    };
                    out[o..o + 2].copy_from_slice(&r.to_le_bytes());
                }
                u128::from_le_bytes(out)
            }
        };
        Ok(())
    }

    fn exec_sse_unpack_pack(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        let (reg, loc) = self.decode_modrm(mem, pfx)?;
        let a = self.xmm[pfx.vex_vvvv.unwrap_or(reg) as usize].to_le_bytes();
        let b = self.read_xmm_rm(mem, loc)?.to_le_bytes();
        let mut out = [0u8; 16];
        match op {
            0x60 => {
                for i in 0..8 {
                    out[i * 2] = a[i];
                    out[i * 2 + 1] = b[i];
                }
            }
            0x61 => {
                for i in 0..4 {
                    out[i * 4..i * 4 + 2].copy_from_slice(&a[i * 2..i * 2 + 2]);
                    out[i * 4 + 2..i * 4 + 4].copy_from_slice(&b[i * 2..i * 2 + 2]);
                }
            }
            0x62 => {
                for i in 0..2 {
                    out[i * 8..i * 8 + 4].copy_from_slice(&a[i * 4..i * 4 + 4]);
                    out[i * 8 + 4..i * 8 + 8].copy_from_slice(&b[i * 4..i * 4 + 4]);
                }
            }
            0x67 => {
                // PACKUSWB
                for i in 0..8 {
                    let va = i16::from_le_bytes([a[i * 2], a[i * 2 + 1]]);
                    let vb = i16::from_le_bytes([b[i * 2], b[i * 2 + 1]]);
                    out[i] = va.clamp(0, 255) as u8;
                    out[8 + i] = vb.clamp(0, 255) as u8;
                }
            }
            0x68 => {
                for i in 0..8 {
                    out[i * 2] = a[8 + i];
                    out[i * 2 + 1] = b[8 + i];
                }
            }
            0x69 => {
                for i in 0..4 {
                    out[i * 4..i * 4 + 2].copy_from_slice(&a[8 + i * 2..10 + i * 2]);
                    out[i * 4 + 2..i * 4 + 4].copy_from_slice(&b[8 + i * 2..10 + i * 2]);
                }
            }
            0x6A => {
                for i in 0..2 {
                    out[i * 8..i * 8 + 4].copy_from_slice(&a[8 + i * 4..12 + i * 4]);
                    out[i * 8 + 4..i * 8 + 8].copy_from_slice(&b[8 + i * 4..12 + i * 4]);
                }
            }
            0x6C => {
                out[0..8].copy_from_slice(&a[0..8]);
                out[8..16].copy_from_slice(&b[0..8]);
            }
            0x6D => {
                out[0..8].copy_from_slice(&a[8..16]);
                out[8..16].copy_from_slice(&b[8..16]);
            }
            _ => {}
        }
        self.xmm[reg as usize] = u128::from_le_bytes(out);
        Ok(())
    }

    fn exec_sse_math_logic(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        let (reg, loc) = self.decode_modrm(mem, pfx)?;
        let lhs = self.xmm[pfx.vex_vvvv.unwrap_or(reg) as usize];
        match op {
            0x54..=0x57 => {
                let rhs = self.read_xmm_rm(mem, loc)?;
                self.xmm[reg as usize] = match op {
                    0x54 => lhs & rhs,
                    0x55 => !lhs & rhs,
                    0x56 => lhs | rhs,
                    _ => lhs ^ rhs,
                };
                Ok(())
            }
            0x5A => {
                // CVTSS2SD / CVTSD2SS / CVTPS2PD / CVTPD2PS
                if pfx.rep_f3 {
                    let rhs = self.read_xmm_scalar_rm(mem, loc, false)?;
                    let d = f64::from(f32::from_bits(rhs as u32));
                    self.xmm[reg as usize] = (lhs & !0xFFFF_FFFF_FFFF_FFFF) | u128::from(d.to_bits());
                } else if pfx.rep_f2 {
                    let rhs = self.read_xmm_scalar_rm(mem, loc, true)?;
                    let s = f64::from_bits(rhs as u64) as f32;
                    self.xmm[reg as usize] = (lhs & !0xFFFF_FFFF) | u128::from(s.to_bits());
                } else if pfx.op66 {
                    let rb = self.read_xmm_rm(mem, loc)?.to_le_bytes();
                    let s0 = (f64::from_bits(u64::from_le_bytes(rb[0..8].try_into().unwrap_or([0; 8]))) as f32).to_bits();
                    let s1 = (f64::from_bits(u64::from_le_bytes(rb[8..16].try_into().unwrap_or([0; 8]))) as f32).to_bits();
                    self.xmm[reg as usize] = u128::from(s0) | (u128::from(s1) << 32);
                } else {
                    let rhs = self.read_xmm_scalar_rm(mem, loc, true)?;
                    let d0 = f64::from(f32::from_bits(rhs as u32)).to_bits();
                    let d1 = f64::from(f32::from_bits((rhs >> 32) as u32)).to_bits();
                    self.xmm[reg as usize] = u128::from(d0) | (u128::from(d1) << 64);
                }
                Ok(())
            }
            0x5B => {
                // CVTDQ2PS / CVTTPS2DQ / CVTPS2DQ
                let rb = self.read_xmm_rm(mem, loc)?.to_le_bytes();
                let mut out = [0u8; 16];
                for i in 0..4 {
                    let o = i * 4;
                    let w = [rb[o], rb[o + 1], rb[o + 2], rb[o + 3]];
                    if pfx.rep_f3 {
                        let iv = (f32::from_bits(u32::from_le_bytes(w)).trunc() as i32).to_le_bytes();
                        out[o..o + 4].copy_from_slice(&iv);
                    } else if pfx.op66 {
                        let iv = (f32::from_bits(u32::from_le_bytes(w)).round_ties_even() as i32).to_le_bytes();
                        out[o..o + 4].copy_from_slice(&iv);
                    } else {
                        let fv = (i32::from_le_bytes(w) as f32).to_bits().to_le_bytes();
                        out[o..o + 4].copy_from_slice(&fv);
                    }
                }
                self.xmm[reg as usize] = u128::from_le_bytes(out);
                Ok(())
            }
            _ => {
                // 0x51 SQRT, 0x58 ADD, 0x59 MUL, 0x5C SUB, 0x5D MIN, 0x5E DIV, 0x5F MAX
                if pfx.rep_f3 {
                    let rhs = self.read_xmm_scalar_rm(mem, loc, false)?;
                    let a = f32::from_bits(lhs as u32);
                    let b = f32::from_bits(rhs as u32);
                    let r = sse_f32_op(op, a, b);
                    self.xmm[reg as usize] = (lhs & !0xFFFF_FFFF) | u128::from(r.to_bits());
                } else if pfx.rep_f2 {
                    let rhs = self.read_xmm_scalar_rm(mem, loc, true)?;
                    let a = f64::from_bits(lhs as u64);
                    let b = f64::from_bits(rhs as u64);
                    let r = sse_f64_op(op, a, b);
                    self.xmm[reg as usize] = (lhs & !0xFFFF_FFFF_FFFF_FFFF) | u128::from(r.to_bits());
                } else if pfx.op66 {
                    let lb = lhs.to_le_bytes();
                    let rb = self.read_xmm_rm(mem, loc)?.to_le_bytes();
                    let mut out = [0u8; 16];
                    for i in 0..2 {
                        let o = i * 8;
                        let a = f64::from_bits(u64::from_le_bytes(lb[o..o + 8].try_into().unwrap_or([0; 8])));
                        let b = f64::from_bits(u64::from_le_bytes(rb[o..o + 8].try_into().unwrap_or([0; 8])));
                        out[o..o + 8].copy_from_slice(&sse_f64_op(op, a, b).to_bits().to_le_bytes());
                    }
                    self.xmm[reg as usize] = u128::from_le_bytes(out);
                } else {
                    let lb = lhs.to_le_bytes();
                    let rb = self.read_xmm_rm(mem, loc)?.to_le_bytes();
                    let mut out = [0u8; 16];
                    for i in 0..4 {
                        let o = i * 4;
                        let a = f32::from_bits(u32::from_le_bytes([lb[o], lb[o + 1], lb[o + 2], lb[o + 3]]));
                        let b = f32::from_bits(u32::from_le_bytes([rb[o], rb[o + 1], rb[o + 2], rb[o + 3]]));
                        out[o..o + 4].copy_from_slice(&sse_f32_op(op, a, b).to_bits().to_le_bytes());
                    }
                    self.xmm[reg as usize] = u128::from_le_bytes(out);
                }
                Ok(())
            }
        }
    }

    fn exec_packed_int_op(&mut self, mem: &mut PagedMemory, pfx: Prefixes, op: u8) -> Result<()> {
        let (reg, loc) = self.decode_modrm(mem, pfx)?;
        let lhs = self.xmm[pfx.vex_vvvv.unwrap_or(reg) as usize];
        let rhs = self.read_xmm_rm(mem, loc)?;
        self.xmm[reg as usize] = match op {
            0xDB => lhs & rhs,
            0xEB => lhs | rhs,
            0xEF => lhs ^ rhs,
            0xD4 | 0xFB => {
                // PADDQ (0xD4) / PSUBQ (0xFB)
                let l0 = lhs as u64;
                let l1 = (lhs >> 64) as u64;
                let r0 = rhs as u64;
                let r1 = (rhs >> 64) as u64;
                let (o0, o1) = if op == 0xD4 { (l0.wrapping_add(r0), l1.wrapping_add(r1)) } else { (l0.wrapping_sub(r0), l1.wrapping_sub(r1)) };
                u128::from(o0) | (u128::from(o1) << 64)
            }
            _ => {
                // PADDD (0xFE) / PSUBD (0xFA)
                let lb = lhs.to_le_bytes();
                let rb = rhs.to_le_bytes();
                let mut out = [0u8; 16];
                for i in 0..4 {
                    let o = i * 4;
                    let a = i32::from_le_bytes([lb[o], lb[o + 1], lb[o + 2], lb[o + 3]]);
                    let b = i32::from_le_bytes([rb[o], rb[o + 1], rb[o + 2], rb[o + 3]]);
                    let r = if op == 0xFE { a.wrapping_add(b) } else { a.wrapping_sub(b) };
                    out[o..o + 4].copy_from_slice(&r.to_le_bytes());
                }
                u128::from_le_bytes(out)
            }
        };
        Ok(())
    }
}

fn mask_bits(val: u64, bits: u8) -> u64 {
    match bits {
        8 => val & 0xFF,
        16 => val & 0xFFFF,
        32 => val & 0xFFFF_FFFF,
        _ => val,
    }
}

fn sign_extend(val: u64, bits: u8) -> i64 {
    match bits {
        8 => val as u8 as i8 as i64,
        16 => val as u16 as i16 as i64,
        32 => val as u32 as i32 as i64,
        _ => val as i64,
    }
}

fn add_with_carry(lhs: u64, rhs: u64, cin: u64, bits: u8) -> (u64, bool) {
    if bits == 64 {
        let (s1, c1) = lhs.overflowing_add(rhs);
        let (s2, c2) = s1.overflowing_add(cin);
        (s2, c1 || c2)
    } else {
        let full = lhs + rhs + cin;
        let mask = mask_bits(u64::MAX, bits);
        (full & mask, full > mask)
    }
}

fn sub_with_borrow(lhs: u64, rhs: u64, bin: u64, bits: u8) -> (u64, bool) {
    if bits == 64 {
        let (d1, b1) = lhs.overflowing_sub(rhs);
        let (d2, b2) = d1.overflowing_sub(bin);
        (d2, b1 || b2)
    } else {
        let full = (lhs as i128) - (rhs as i128) - (bin as i128);
        (mask_bits(full as u64, bits), full < 0)
    }
}

fn sse_f32_op(op: u8, a: f32, b: f32) -> f32 {
    match op {
        0x51 => b.sqrt(),
        0x52 => 1.0 / b.sqrt(),
        0x53 => 1.0 / b,
        0x58 => a + b,
        0x59 => a * b,
        0x5C => a - b,
        0x5D => {
            if a < b {
                a
            } else {
                b
            }
        }
        0x5E => a / b,
        _ => {
            if a > b {
                a
            } else {
                b
            }
        }
    }
}

fn sse_f64_op(op: u8, a: f64, b: f64) -> f64 {
    match op {
        0x51 => b.sqrt(),
        0x58 => a + b,
        0x59 => a * b,
        0x5C => a - b,
        0x5D => {
            if a < b {
                a
            } else {
                b
            }
        }
        0x5E => a / b,
        _ => {
            if a > b {
                a
            } else {
                b
            }
        }
    }
}
