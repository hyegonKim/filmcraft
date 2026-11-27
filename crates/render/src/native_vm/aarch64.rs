//! Metered `AArch64` (`arm64`) user-mode CPU emulator implementing **AAPCS64** (`macOS` Apple
//! Silicon, `iOS`, `Android`, `Linux ARM64`, `Windows ARM64`), A64 integer ALU/bitfield/branches/
//! loads-stores (`LDR`/`STR`/`LDP`/`STP`), and scalar FP & NEON SIMD instructions.

use super::crt::{GuestVm, HostCallbackHandler, ThunkOutcome};
use super::mem::{PagedMemory, RETURN_SENTINEL};
use super::{PluginHostError, Result};

/// Complete `AArch64` user-mode register state.
pub struct Arm64Cpu {
    pub x: [u64; 31], // X0..X30 (X30 = LR)
    pub sp: u64,
    pub pc: u64,
    pub v: [u128; 32],   // V0..V31 FP/NEON
    pub nzcv: [bool; 4], // N, Z, C, V
}

impl Arm64Cpu {
    pub fn new_call(fn_addr: u64, sp_base: u64, args: &[u64], mem: &mut PagedMemory) -> Result<Self> {
        let mut cpu = Self { x: [0; 31], sp: sp_base & !15, pc: fn_addr, v: [0; 32], nzcv: [false; 4] };
        for (i, &val) in args.iter().take(8).enumerate() {
            cpu.x[i] = val;
        }
        if args.len() > 8 {
            let extra = args.len() - 8;
            let bytes = (((extra * 8) + 15) & !15) as u64;
            cpu.sp -= bytes;
            for (i, &val) in args.iter().skip(8).enumerate() {
                mem.write_u64(cpu.sp + (i as u64) * 8, val)?;
            }
        }
        cpu.x[30] = RETURN_SENTINEL;
        Ok(cpu)
    }

    pub fn run(&mut self, vm: &mut GuestVm, handler: &mut dyn HostCallbackHandler) -> Result<u64> {
        loop {
            if self.pc == RETURN_SENTINEL {
                return Ok(self.x[0]);
            }
            if let Some(thunk_idx) = PagedMemory::is_thunk_addr(self.pc) {
                self.handle_thunk_trap(thunk_idx, vm, handler)?;
                continue;
            }
            if vm.fuel_remaining == 0 {
                return Err(PluginHostError::Limit(format!("AArch64 instruction budget exhausted at {:#x}", self.pc)));
            }
            vm.fuel_remaining -= 1;
            let insn = vm.mem.read_u32(self.pc)?;
            let cur_pc = self.pc;
            self.pc = self.pc.wrapping_add(4);
            self.step_insn(insn, cur_pc, &mut vm.mem)?;
        }
    }

    fn handle_thunk_trap(&mut self, thunk_idx: u32, vm: &mut GuestVm, handler: &mut dyn HostCallbackHandler) -> Result<()> {
        let args = [self.x[0], self.x[1], self.x[2], self.x[3], self.x[4], self.x[5], self.x[6], self.x[7]];
        let fargs = [f64::from_bits(self.v[0] as u64), f64::from_bits(self.v[1] as u64), f64::from_bits(self.v[2] as u64), f64::from_bits(self.v[3] as u64)];
        let outcome = vm.dispatch_thunk(thunk_idx, args, fargs, handler)?;
        match outcome {
            ThunkOutcome::Int(v) => self.x[0] = v,
            ThunkOutcome::Float64(f) => self.v[0] = u128::from(f.to_bits()),
            ThunkOutcome::Float32(f) => self.v[0] = u128::from(f.to_bits()),
            ThunkOutcome::InitTerm { .. } | ThunkOutcome::PiThreadsDoAll { .. } => {}
        }
        self.pc = self.x[30];
        Ok(())
    }

    fn rx(&self, r: u32, sf: bool) -> u64 {
        if r >= 31 {
            return 0; // XZR / WZR
        }
        let v = self.x[r as usize];
        if sf { v } else { v & 0xFFFF_FFFF }
    }

    fn rx_sp(&self, r: u32, sf: bool) -> u64 {
        let v = if r >= 31 { self.sp } else { self.x[r as usize] };
        if sf { v } else { v & 0xFFFF_FFFF }
    }

    fn wx(&mut self, r: u32, sf: bool, val: u64) {
        if r < 31 {
            self.x[r as usize] = if sf { val } else { val & 0xFFFF_FFFF };
        }
    }

    fn wx_sp(&mut self, r: u32, sf: bool, val: u64) {
        let masked = if sf { val } else { val & 0xFFFF_FFFF };
        if r >= 31 {
            self.sp = masked;
        } else {
            self.x[r as usize] = masked;
        }
    }

    fn cond_holds(&self, cond: u32) -> bool {
        let [n, z, c, v] = self.nzcv;
        let base = match (cond & 0xF) >> 1 {
            0 => z,
            1 => c,
            2 => n,
            3 => v,
            4 => c && !z,
            5 => n == v,
            6 => (n == v) && !z,
            _ => true,
        };
        if (cond & 1) != 0 && (cond & 0xF) != 0xF { !base } else { base }
    }

    fn step_insn(&mut self, insn: u32, cur_pc: u64, mem: &mut PagedMemory) -> Result<()> {
        if insn == 0xD503_201F {
            return Ok(()); // NOP
        }
        let op0 = (insn >> 25) & 0xF;
        match op0 {
            0b1000 | 0b1001 => self.step_dp_imm(insn, cur_pc),
            0b1010 | 0b1011 => self.step_branch_sys(insn, cur_pc),
            0b0100 | 0b0110 | 0b1100 | 0b1110 => self.step_ldst(insn, cur_pc, mem),
            0b0101 | 0b1101 => self.step_dp_reg(insn),
            0b0111 | 0b1111 => self.step_fpsimd(insn),
            _ => Err(PluginHostError::CpuTrap(format!("unimplemented AArch64 insn {insn:#010x} at {cur_pc:#x}"))),
        }
    }

    fn step_dp_imm(&mut self, insn: u32, cur_pc: u64) -> Result<()> {
        let op = (insn >> 23) & 0x7;
        let sf = ((insn >> 31) & 1) != 0;
        let rd = insn & 0x1F;
        let rn = (insn >> 5) & 0x1F;
        match op {
            0b000 | 0b001 => {
                // ADR / ADRP
                let immlo = i64::from((insn >> 29) & 3);
                let immhi = ((insn as i32) << 8 >> 13) as i64;
                let imm = (immhi << 2) | immlo;
                let val = if ((insn >> 31) & 1) != 0 { (cur_pc & !0xFFF).wrapping_add_signed(imm << 12) } else { cur_pc.wrapping_add_signed(imm) };
                self.wx(rd, true, val);
                Ok(())
            }
            0b010 | 0b011 => {
                // ADD / ADDS / SUB / SUBS (immediate)
                let is_sub = ((insn >> 30) & 1) != 0;
                let set_flags = ((insn >> 29) & 1) != 0;
                let sh = ((insn >> 22) & 1) * 12;
                let imm12 = u64::from((insn >> 10) & 0xFFF) << sh;
                let lhs = self.rx_sp(rn, sf);
                let res = self.add_sub_flags(lhs, imm12, is_sub, sf, set_flags);
                if set_flags {
                    self.wx(rd, sf, res);
                } else {
                    self.wx_sp(rd, sf, res);
                }
                Ok(())
            }
            0b100 => {
                // AND / ORR / EOR / ANDS (bitmask immediate)
                let opc = (insn >> 29) & 3;
                let mask = decode_bitmask_imm(insn, sf)?;
                let lhs = self.rx(rn, sf);
                let res = match opc {
                    0 => lhs & mask,
                    1 => lhs | mask,
                    2 => lhs ^ mask,
                    _ => {
                        let r = lhs & mask;
                        self.set_logic_flags(r, sf);
                        r
                    }
                };
                if opc == 3 {
                    self.wx(rd, sf, res);
                } else {
                    self.wx_sp(rd, sf, res);
                }
                Ok(())
            }
            0b101 => {
                // MOVN / MOVZ / MOVK
                let opc = (insn >> 29) & 3;
                let hw = ((insn >> 21) & 3) * 16;
                let imm16 = u64::from((insn >> 5) & 0xFFFF);
                let res = match opc {
                    0 => !(imm16 << hw),
                    2 => imm16 << hw,
                    3 => (self.rx(rd, sf) & !(0xFFFFu64 << hw)) | (imm16 << hw),
                    _ => 0,
                };
                self.wx(rd, sf, res);
                Ok(())
            }
            0b110 => self.step_bitfield(insn, sf, rd, rn),
            0b111 => {
                // EXTR / ROR
                let rm = (insn >> 16) & 0x1F;
                let imms = (insn >> 10) & 0x3F;
                let a = self.rx(rn, sf);
                let b = self.rx(rm, sf);
                let res = if imms == 0 {
                    b
                } else if sf {
                    (a << (64 - imms)) | (b >> imms)
                } else {
                    (((a as u32) << (32 - imms)) | ((b as u32) >> imms)) as u64
                };
                self.wx(rd, sf, res);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn step_bitfield(&mut self, insn: u32, sf: bool, rd: u32, rn: u32) -> Result<()> {
        let opc = (insn >> 29) & 3; // 0 = SBFM, 1 = BFM, 2 = UBFM
        let immr = (insn >> 16) & 0x3F;
        let imms = (insn >> 10) & 0x3F;
        let width = if sf { 64u32 } else { 32u32 };
        let src = self.rx(rn, sf);
        let dst = self.rx(rd, sf);
        let res = if imms >= immr {
            let len = imms - immr + 1;
            let mask = if len >= 64 { u64::MAX } else { (1u64 << len) - 1 };
            let field = (src >> immr) & mask;
            match opc {
                0 => {
                    let sign_bit = 1u64 << (len - 1);
                    if (field & sign_bit) != 0 { field | !mask } else { field }
                }
                1 => (dst & !mask) | field,
                _ => field,
            }
        } else {
            let lshift = width - immr;
            let len = imms + 1;
            let mask = if len >= 64 { u64::MAX } else { (1u64 << len) - 1 };
            let field = (src & mask) << lshift;
            match opc {
                0 => {
                    let top = lshift + len;
                    let sign = (src >> imms) & 1;
                    if sign != 0 && top < 64 { field | (!0u64 << top) } else { field }
                }
                1 => (dst & !(mask << lshift)) | field,
                _ => field,
            }
        };
        self.wx(rd, sf, res);
        Ok(())
    }

    fn step_branch_sys(&mut self, insn: u32, cur_pc: u64) -> Result<()> {
        if (insn & 0xFC00_0000) == 0x1400_0000 || (insn & 0xFC00_0000) == 0x9400_0000 {
            // B / BL imm26
            let is_bl = ((insn >> 31) & 1) != 0;
            let imm26 = ((insn as i32) << 6 >> 6) as i64 * 4;
            if is_bl {
                self.x[30] = self.pc;
            }
            self.pc = cur_pc.wrapping_add_signed(imm26);
            return Ok(());
        }
        if (insn & 0xFF00_0010) == 0x5400_0000 {
            // B.cond
            let cond = insn & 0xF;
            let imm19 = ((insn as i32) << 8 >> 13) as i64 * 4;
            if self.cond_holds(cond) {
                self.pc = cur_pc.wrapping_add_signed(imm19);
            }
            return Ok(());
        }
        if (insn & 0x7E00_0000) == 0x3400_0000 {
            // CBZ / CBNZ
            let sf = ((insn >> 31) & 1) != 0;
            let is_nz = ((insn >> 24) & 1) != 0;
            let rt = insn & 0x1F;
            let imm19 = ((insn as i32) << 8 >> 13) as i64 * 4;
            let val = self.rx(rt, sf);
            if (val != 0) == is_nz {
                self.pc = cur_pc.wrapping_add_signed(imm19);
            }
            return Ok(());
        }
        if (insn & 0x7E00_0000) == 0x3600_0000 {
            // TBZ / TBNZ
            let is_nz = ((insn >> 24) & 1) != 0;
            let bit = (((insn >> 31) & 1) << 5) | ((insn >> 19) & 0x1F);
            let rt = insn & 0x1F;
            let imm14 = ((insn as i32) << 13 >> 18) as i64 * 4;
            let bit_set = ((self.rx(rt, true) >> bit) & 1) != 0;
            if bit_set == is_nz {
                self.pc = cur_pc.wrapping_add_signed(imm14);
            }
            return Ok(());
        }
        if (insn & 0xFE1F_FC1F) == 0xD61F_0000 {
            // BR / BLR / RET
            let opc = (insn >> 21) & 3;
            let rn = (insn >> 5) & 0x1F;
            let target = self.rx(rn, true);
            if opc == 1 {
                self.x[30] = self.pc; // BLR
            }
            self.pc = target;
            return Ok(());
        }
        if (insn & 0xFFFF_F01F) == 0xD503_201F {
            return Ok(()); // HINT / BTI / PACIASP NOP-equivalents in user-mode
        }
        Err(PluginHostError::CpuTrap(format!("unimplemented AArch64 branch/sys {insn:#010x} at {cur_pc:#x}")))
    }

    fn step_ldst(&mut self, insn: u32, cur_pc: u64, mem: &mut PagedMemory) -> Result<()> {
        if (insn & 0x3B00_0000) == 0x1800_0000 {
            // LDR literal
            let opc = (insn >> 30) & 3;
            let is_fp = ((insn >> 26) & 1) != 0;
            let rt = insn & 0x1F;
            let imm19 = ((insn as i32) << 8 >> 13) as i64 * 4;
            let addr = cur_pc.wrapping_add_signed(imm19);
            if is_fp {
                self.v[rt as usize] = match opc {
                    0 => u128::from(mem.read_u32(addr)?),
                    1 => u128::from(mem.read_u64(addr)?),
                    _ => mem.read_u128(addr)?,
                };
            } else {
                let v = match opc {
                    0 => u64::from(mem.read_u32(addr)?),
                    1 => mem.read_u64(addr)?,
                    _ => mem.read_i32(addr)? as i64 as u64,
                };
                self.wx(rt, opc != 0, v);
            }
            return Ok(());
        }
        if (insn & 0x3A00_0000) == 0x2800_0000 {
            return self.step_ldp_stp(insn, mem);
        }
        if (insn & 0x3B00_0000) == 0x3900_0000 || (insn & 0x3B20_0000) == 0x3800_0000 || (insn & 0x3B20_0800) == 0x3820_0800 {
            return self.step_single_ldst(insn, mem);
        }
        Err(PluginHostError::CpuTrap(format!("unimplemented AArch64 load/store {insn:#010x} at {cur_pc:#x}")))
    }

    fn step_ldp_stp(&mut self, insn: u32, mem: &mut PagedMemory) -> Result<()> {
        let opc = (insn >> 30) & 3;
        let is_fp = ((insn >> 26) & 1) != 0;
        let mode = (insn >> 23) & 3; // 1 = post, 2 = signed offset, 3 = pre
        let is_load = ((insn >> 22) & 1) != 0;
        let imm7 = ((insn as i32) << 10 >> 25) as i64;
        let rt2 = (insn >> 10) & 0x1F;
        let rn = (insn >> 5) & 0x1F;
        let rt = insn & 0x1F;
        let scale = if is_fp {
            4usize << opc
        } else if (opc & 2) != 0 {
            8
        } else {
            4
        };
        let offset = imm7 * (scale as i64);
        let base = self.rx_sp(rn, true);
        let addr = if mode == 1 { base } else { base.wrapping_add_signed(offset) };

        if is_load {
            if is_fp {
                self.v[rt as usize] = read_fp_mem(mem, addr, scale)?;
                self.v[rt2 as usize] = read_fp_mem(mem, addr + scale as u64, scale)?;
            } else if opc == 1 {
                self.wx(rt, true, mem.read_i32(addr)? as i64 as u64);
                self.wx(rt2, true, mem.read_i32(addr + 4)? as i64 as u64);
            } else {
                let sf = scale == 8;
                self.wx(rt, sf, if sf { mem.read_u64(addr)? } else { u64::from(mem.read_u32(addr)?) });
                self.wx(rt2, sf, if sf { mem.read_u64(addr + 8)? } else { u64::from(mem.read_u32(addr + 4)?) });
            }
        } else if is_fp {
            write_fp_mem(mem, addr, scale, self.v[rt as usize])?;
            write_fp_mem(mem, addr + scale as u64, scale, self.v[rt2 as usize])?;
        } else if scale == 8 {
            mem.write_u64(addr, self.rx(rt, true))?;
            mem.write_u64(addr + 8, self.rx(rt2, true))?;
        } else {
            mem.write_u32(addr, self.rx(rt, false) as u32)?;
            mem.write_u32(addr + 4, self.rx(rt2, false) as u32)?;
        }

        if mode == 1 || mode == 3 {
            self.wx_sp(rn, true, base.wrapping_add_signed(offset));
        }
        Ok(())
    }

    fn step_single_ldst(&mut self, insn: u32, mem: &mut PagedMemory) -> Result<()> {
        let size = (insn >> 30) & 3;
        let is_fp = ((insn >> 26) & 1) != 0;
        let opc = (insn >> 22) & 3;
        let rn = (insn >> 5) & 0x1F;
        let rt = insn & 0x1F;
        let byte_size = if is_fp && (opc & 2) != 0 && size == 0 { 16usize } else { 1usize << size };

        let base = self.rx_sp(rn, true);
        let (addr, writeback) = if ((insn >> 24) & 1) != 0 {
            let imm12 = u64::from((insn >> 10) & 0xFFF) * (byte_size as u64);
            (base.wrapping_add(imm12), None)
        } else if ((insn >> 21) & 1) != 0 && ((insn >> 10) & 3) == 2 {
            // Register offset: [Xn, Xm, extend #amount]
            let rm = (insn >> 16) & 0x1F;
            let opt = (insn >> 13) & 7;
            let s_bit = (insn >> 12) & 1;
            let shift = if s_bit != 0 { byte_size.trailing_zeros() } else { 0 };
            let idx = extend_reg(self.rx(rm, true), opt) << shift;
            (base.wrapping_add(idx), None)
        } else {
            let imm9 = ((insn as i32) << 11 >> 23) as i64;
            let mode = (insn >> 10) & 3;
            match mode {
                1 => (base, Some(base.wrapping_add_signed(imm9))), // post-index
                3 => {
                    let ea = base.wrapping_add_signed(imm9);
                    (ea, Some(ea)) // pre-index
                }
                _ => (base.wrapping_add_signed(imm9), None), // unscaled LDUR/STUR
            }
        };

        if is_fp {
            if (opc & 1) != 0 {
                self.v[rt as usize] = read_fp_mem(mem, addr, byte_size)?;
            } else {
                write_fp_mem(mem, addr, byte_size, self.v[rt as usize])?;
            }
        } else if opc == 0 {
            // Store GPR
            let val = self.rx(rt, byte_size == 8);
            match byte_size {
                1 => mem.write_u8(addr, val as u8)?,
                2 => mem.write_u16(addr, val as u16)?,
                4 => mem.write_u32(addr, val as u32)?,
                _ => mem.write_u64(addr, val)?,
            }
        } else if opc == 1 {
            // Unsigned load GPR
            let val = match byte_size {
                1 => u64::from(mem.read_u8(addr)?),
                2 => u64::from(mem.read_u16(addr)?),
                4 => u64::from(mem.read_u32(addr)?),
                _ => mem.read_u64(addr)?,
            };
            self.wx(rt, byte_size == 8, val);
        } else {
            // Signed load (`LDRSB`, `LDRSH`, `LDRSW`) -> opc 2 = 64-bit dst, opc 3 = 32-bit dst
            let dst_sf = opc == 2;
            let sval = match byte_size {
                1 => mem.read_u8(addr)? as i8 as i64 as u64,
                2 => mem.read_i16(addr)? as i64 as u64,
                _ => mem.read_i32(addr)? as i64 as u64,
            };
            self.wx(rt, dst_sf, sval);
        }

        if let Some(wb) = writeback {
            self.wx_sp(rn, true, wb);
        }
        Ok(())
    }

    fn step_dp_reg(&mut self, insn: u32) -> Result<()> {
        let sf = ((insn >> 31) & 1) != 0;
        let opc = (insn >> 29) & 3;
        let rd = insn & 0x1F;
        let rn = (insn >> 5) & 0x1F;
        let rm = (insn >> 16) & 0x1F;

        if (insn & 0x1FE0_0000) == 0x1B00_0000 {
            // Data-processing 3-source: MADD, MSUB, SMADDL, UMADDL
            let ra = (insn >> 10) & 0x1F;
            let is_sub = ((insn >> 15) & 1) != 0;
            let op31 = (insn >> 21) & 7;
            let res = match op31 {
                0 => {
                    let prod = self.rx(rn, sf).wrapping_mul(self.rx(rm, sf));
                    let acc = self.rx(ra, sf);
                    if is_sub { acc.wrapping_sub(prod) } else { acc.wrapping_add(prod) }
                }
                1 => {
                    // SMADDL / SMSUBL
                    let prod = (self.rx(rn, false) as u32 as i32 as i64).wrapping_mul(self.rx(rm, false) as u32 as i32 as i64) as u64;
                    let acc = self.rx(ra, true);
                    if is_sub { acc.wrapping_sub(prod) } else { acc.wrapping_add(prod) }
                }
                5 => {
                    // UMADDL / UMSUBL
                    let prod = (self.rx(rn, false) & 0xFFFF_FFFF) * (self.rx(rm, false) & 0xFFFF_FFFF);
                    let acc = self.rx(ra, true);
                    if is_sub { acc.wrapping_sub(prod) } else { acc.wrapping_add(prod) }
                }
                _ => 0,
            };
            self.wx(rd, sf || op31 != 0, res);
            return Ok(());
        }

        if (insn & 0x1FE0_0000) == 0x1AC0_0000 {
            // Data-processing 2-source & 1-source (`UDIV`, `SDIV`, `LSLV`, `LSRV`, `ASRV`, `RORV`, `CLZ`, `REV`)
            if ((insn >> 30) & 1) != 0 {
                // 1-source (`CLZ`, `RBIT`, `REV`)
                let opcode2 = (insn >> 10) & 0x3F;
                let v = self.rx(rn, sf);
                let out = match opcode2 {
                    0 => {
                        if sf {
                            v.reverse_bits()
                        } else {
                            u64::from((v as u32).reverse_bits())
                        }
                    }
                    2 | 3 => {
                        if sf {
                            v.swap_bytes()
                        } else {
                            u64::from((v as u32).swap_bytes())
                        }
                    }
                    4 => {
                        if sf {
                            u64::from(v.leading_zeros())
                        } else {
                            u64::from((v as u32).leading_zeros())
                        }
                    }
                    _ => v,
                };
                self.wx(rd, sf, out);
                return Ok(());
            }
            let opcode = (insn >> 10) & 0x3F;
            let a = self.rx(rn, sf);
            let b = self.rx(rm, sf);
            let out = match opcode {
                2 => a.checked_div(b).unwrap_or(0), // UDIV
                3 => {
                    // SDIV
                    if b == 0 {
                        0
                    } else if sf {
                        (a as i64).wrapping_div(b as i64) as u64
                    } else {
                        ((a as u32 as i32).wrapping_div(b as u32 as i32) as u32) as u64
                    }
                }
                8 => shift_reg(a, (b & if sf { 63 } else { 31 }) as u32, 0, sf),  // LSLV
                9 => shift_reg(a, (b & if sf { 63 } else { 31 }) as u32, 1, sf),  // LSRV
                10 => shift_reg(a, (b & if sf { 63 } else { 31 }) as u32, 2, sf), // ASRV
                11 => shift_reg(a, (b & if sf { 63 } else { 31 }) as u32, 3, sf), // RORV
                _ => 0,
            };
            self.wx(rd, sf, out);
            return Ok(());
        }

        if (insn & 0x1FE0_0000) == 0x1A80_0000 {
            // Conditional select: CSEL, CSINC, CSINV, CSNEG
            let cond = (insn >> 12) & 0xF;
            let op2 = (insn >> 10) & 3;
            let res = if self.cond_holds(cond) {
                self.rx(rn, sf)
            } else {
                let b = self.rx(rm, sf);
                match (opc & 1, op2) {
                    (0, 0) => b,
                    (0, 1) => b.wrapping_add(1),
                    (1, 0) => !b,
                    _ => (!b).wrapping_add(1),
                }
            };
            self.wx(rd, sf, res);
            return Ok(());
        }

        if ((insn >> 24) & 1) == 0 {
            // Logical (shifted register): AND, BIC, ORR, ORN, EOR, EON, ANDS, BICS
            let shift_type = (insn >> 22) & 3;
            let inv = ((insn >> 21) & 1) != 0;
            let imm6 = (insn >> 10) & 0x3F;
            let lhs = self.rx(rn, sf);
            let mut rhs = shift_reg(self.rx(rm, sf), imm6, shift_type, sf);
            if inv {
                rhs = !rhs;
            }
            let res = match opc {
                0 => lhs & rhs,
                1 => lhs | rhs,
                2 => lhs ^ rhs,
                _ => {
                    let r = lhs & rhs;
                    self.set_logic_flags(r, sf);
                    r
                }
            };
            self.wx(rd, sf, res);
            return Ok(());
        }

        // Add/subtract (shifted or extended register)
        let is_sub = (opc & 2) != 0;
        let set_flags = (opc & 1) != 0;
        let is_ext = ((insn >> 21) & 1) != 0;
        let lhs = if is_ext { self.rx_sp(rn, sf) } else { self.rx(rn, sf) };
        let rhs = if is_ext {
            let option = (insn >> 13) & 7;
            let imm3 = (insn >> 10) & 7;
            extend_reg(self.rx(rm, true), option) << imm3
        } else {
            let shift_type = (insn >> 22) & 3;
            let imm6 = (insn >> 10) & 0x3F;
            shift_reg(self.rx(rm, sf), imm6, shift_type, sf)
        };
        let res = self.add_sub_flags(lhs, rhs, is_sub, sf, set_flags);
        if is_ext && !set_flags {
            self.wx_sp(rd, sf, res);
        } else {
            self.wx(rd, sf, res);
        }
        Ok(())
    }

    fn step_fpsimd(&mut self, insn: u32) -> Result<()> {
        let rd = (insn & 0x1F) as usize;
        let rn = ((insn >> 5) & 0x1F) as usize;
        let rm = ((insn >> 16) & 0x1F) as usize;

        // MOVI / EOR vector zero (`MOVI v0.2d, #0` = `2F00E400` / `6F00E400`, `EOR v0.16b, v0.16b, v0.16b` = `6E201C00`)
        if (insn & 0xBFDF_FC00) == 0x0F00_E400 || ((insn & 0xBFE0_FC00) == 0x2E20_1C00 && rn == rm) {
            self.v[rd] = 0;
            return Ok(());
        }

        // Scalar FP <-> GPR conversions (`SCVTF`, `UCVTF`, `FCVTZS`, `FCVTZU`, `FMOV`)
        if (insn & 0x5F20_FC00) == 0x1E20_0000 && ((insn >> 21) & 1) == 1 && ((insn >> 10) & 0x3F) == 0 {
            self.exec_fp_gpr_conv(insn, rd, rn);
            return Ok(());
        }

        let ftype = (insn >> 22) & 3;
        let is_f64 = ftype == 1;

        // FMOV immediate (`1E2x_1000`)
        if (insn & 0xFF20_1FE0) == 0x1E20_1000 {
            let imm8 = ((insn >> 13) & 0xFF) as u8;
            let f = decode_arm64_fp_imm8(imm8);
            self.v[rd] = if is_f64 { u128::from(f.to_bits()) } else { u128::from((f as f32).to_bits()) };
            return Ok(());
        }

        // Scalar FP compare (`FCMP` / `FCMPE`: `1E2x_2000`)
        if (insn & 0xFF20_FC0F) == 0x1E20_2000 {
            self.exec_fcmp(insn, rn, rm, is_f64);
            return Ok(());
        }

        // Scalar FP 1-source (`FMOV`, `FABS`, `FNEG`, `FSQRT`, `FCVT`)
        if (insn & 0xFF20_7C00) == 0x1E20_4000 {
            let opc = (insn >> 15) & 0x3F;
            let a = if is_f64 { f64::from_bits(self.v[rn] as u64) } else { f64::from(f32::from_bits(self.v[rn] as u32)) };
            match opc {
                0 => self.v[rd] = if is_f64 { self.v[rn] & 0xFFFF_FFFF_FFFF_FFFF } else { self.v[rn] & 0xFFFF_FFFF },
                1 => self.v[rd] = if is_f64 { u128::from(a.abs().to_bits()) } else { u128::from((a.abs() as f32).to_bits()) },
                2 => self.v[rd] = if is_f64 { u128::from((-a).to_bits()) } else { u128::from((-a as f32).to_bits()) },
                3 => self.v[rd] = if is_f64 { u128::from(a.sqrt().to_bits()) } else { u128::from((a.sqrt() as f32).to_bits()) },
                4 => self.v[rd] = u128::from((a as f32).to_bits()),
                5 => self.v[rd] = u128::from(a.to_bits()),
                _ => {}
            }
            return Ok(());
        }

        // Scalar FP 2-source (`FMUL`, `FDIV`, `FADD`, `FSUB`, `FMAX`, `FMIN`)
        if (insn & 0xFF20_0C00) == 0x1E20_0800 {
            self.exec_fp_2src(insn, rd, rn, rm, is_f64);
            return Ok(());
        }

        // Scalar FP 3-source (`FMADD`, `FMSUB`, `FNMADD`, `FNMSUB`: `1F00_0000`)
        if (insn & 0xFF20_0000) == 0x1F00_0000 {
            self.exec_fp_3src(insn, rd, rn, rm, is_f64);
            return Ok(());
        }

        // FCSEL
        if (insn & 0xFF20_0C00) == 0x1E20_0C00 {
            let cond = (insn >> 12) & 0xF;
            let src = if self.cond_holds(cond) { self.v[rn] } else { self.v[rm] };
            self.v[rd] = if is_f64 { src & 0xFFFF_FFFF_FFFF_FFFF } else { src & 0xFFFF_FFFF };
            return Ok(());
        }

        // NEON vector 4x f32 (`FADD`, `FSUB`, `FMUL`)
        if (insn & 0x9F20_CC00) == 0x0E20_C400 || (insn & 0xBF20_FC00) == 0x2E20_DC00 {
            self.exec_neon_f32(insn, rd, rn, rm);
        }
        Ok(())
    }

    fn exec_fp_gpr_conv(&mut self, insn: u32, rd: usize, rn: usize) {
        let sf = ((insn >> 31) & 1) != 0;
        let ftype = (insn >> 22) & 3;
        let rmode = (insn >> 19) & 3;
        let opcode = (insn >> 16) & 7;
        match (rmode, opcode) {
            (0, 2) => {
                let iv = if sf { self.rx(rn as u32, true) as i64 } else { self.rx(rn as u32, false) as u32 as i32 as i64 };
                self.v[rd] = if ftype == 1 { u128::from((iv as f64).to_bits()) } else { u128::from((iv as f32).to_bits()) };
            }
            (0, 3) => {
                let uv = self.rx(rn as u32, sf);
                self.v[rd] = if ftype == 1 { u128::from((uv as f64).to_bits()) } else { u128::from((uv as f32).to_bits()) };
            }
            (_, 0) => {
                let f = if ftype == 1 { f64::from_bits(self.v[rn] as u64) } else { f64::from(f32::from_bits(self.v[rn] as u32)) };
                let iv = match rmode {
                    1 => f.ceil() as i64,
                    2 => f.floor() as i64,
                    3 => f.trunc() as i64,
                    _ => f.round_ties_even() as i64,
                };
                self.wx(rd as u32, sf, iv as u64);
            }
            (_, 1) => {
                let f = if ftype == 1 { f64::from_bits(self.v[rn] as u64) } else { f64::from(f32::from_bits(self.v[rn] as u32)) };
                self.wx(rd as u32, sf, f.max(0.0).trunc() as u64);
            }
            (0, 6) => self.wx(rd as u32, sf, self.v[rn] as u64),
            (0, 7) => self.v[rd] = u128::from(self.rx(rn as u32, sf)),
            _ => {}
        }
    }

    fn exec_fcmp(&mut self, insn: u32, rn: usize, rm: usize, is_f64: bool) {
        let cmp_zero = ((insn >> 3) & 1) != 0;
        let (a, b) = if is_f64 {
            (f64::from_bits(self.v[rn] as u64), if cmp_zero { 0.0 } else { f64::from_bits(self.v[rm] as u64) })
        } else {
            (f64::from(f32::from_bits(self.v[rn] as u32)), if cmp_zero { 0.0 } else { f64::from(f32::from_bits(self.v[rm] as u32)) })
        };
        self.nzcv = if a.is_nan() || b.is_nan() {
            [false, false, true, true]
        } else if a == b {
            [false, true, true, false]
        } else if a < b {
            [true, false, false, false]
        } else {
            [false, false, true, false]
        };
    }

    fn exec_fp_2src(&mut self, insn: u32, rd: usize, rn: usize, rm: usize, is_f64: bool) {
        let opc = (insn >> 12) & 0xF;
        if is_f64 {
            let a = f64::from_bits(self.v[rn] as u64);
            let b = f64::from_bits(self.v[rm] as u64);
            let r = match opc {
                0 => a * b,
                1 => a / b,
                2 => a + b,
                3 => a - b,
                4 | 6 => a.max(b),
                5 | 7 => a.min(b),
                _ => a,
            };
            self.v[rd] = u128::from(r.to_bits());
        } else {
            let a = f32::from_bits(self.v[rn] as u32);
            let b = f32::from_bits(self.v[rm] as u32);
            let r = match opc {
                0 => a * b,
                1 => a / b,
                2 => a + b,
                3 => a - b,
                4 | 6 => a.max(b),
                5 | 7 => a.min(b),
                _ => a,
            };
            self.v[rd] = u128::from(r.to_bits());
        }
    }

    fn exec_fp_3src(&mut self, insn: u32, rd: usize, rn: usize, rm: usize, is_f64: bool) {
        let ra = ((insn >> 10) & 0x1F) as usize;
        let o1 = ((insn >> 21) & 1) != 0;
        let o0 = ((insn >> 15) & 1) != 0;
        if is_f64 {
            let n = f64::from_bits(self.v[rn] as u64);
            let m = f64::from_bits(self.v[rm] as u64);
            let a = f64::from_bits(self.v[ra] as u64);
            let r = match (o1, o0) {
                (false, false) => a + n * m,
                (false, true) => a - n * m,
                (true, false) => -a - n * m,
                (true, true) => -a + n * m,
            };
            self.v[rd] = u128::from(r.to_bits());
        } else {
            let n = f32::from_bits(self.v[rn] as u32);
            let m = f32::from_bits(self.v[rm] as u32);
            let a = f32::from_bits(self.v[ra] as u32);
            let r = match (o1, o0) {
                (false, false) => a + n * m,
                (false, true) => a - n * m,
                (true, false) => -a - n * m,
                (true, true) => -a + n * m,
            };
            self.v[rd] = u128::from(r.to_bits());
        }
    }

    fn exec_neon_f32(&mut self, insn: u32, rd: usize, rn: usize, rm: usize) {
        let lb = self.v[rn].to_le_bytes();
        let rb = self.v[rm].to_le_bytes();
        let mut out = [0u8; 16];
        let is_mul = (insn & 0x2000_0000) != 0 && ((insn >> 11) & 0x1F) == 0x1B;
        let is_sub = ((insn >> 21) & 1) != 0;
        let lanes = if ((insn >> 30) & 1) != 0 { 4 } else { 2 };
        for i in 0..lanes {
            let o = i * 4;
            let a = f32::from_bits(u32::from_le_bytes([lb[o], lb[o + 1], lb[o + 2], lb[o + 3]]));
            let b = f32::from_bits(u32::from_le_bytes([rb[o], rb[o + 1], rb[o + 2], rb[o + 3]]));
            let r = if is_mul {
                a * b
            } else if is_sub {
                a - b
            } else {
                a + b
            };
            out[o..o + 4].copy_from_slice(&r.to_bits().to_le_bytes());
        }
        self.v[rd] = u128::from_le_bytes(out);
    }

    fn add_sub_flags(&mut self, lhs: u64, rhs: u64, is_sub: bool, sf: bool, set_flags: bool) -> u64 {
        if sf {
            if is_sub {
                let (res, borrow) = lhs.overflowing_sub(rhs);
                if set_flags {
                    let ov = (((lhs ^ rhs) & (lhs ^ res)) >> 63) != 0;
                    self.nzcv = [(res >> 63) != 0, res == 0, !borrow, ov];
                }
                res
            } else {
                let (res, carry) = lhs.overflowing_add(rhs);
                if set_flags {
                    let ov = ((!(lhs ^ rhs) & (lhs ^ res)) >> 63) != 0;
                    self.nzcv = [(res >> 63) != 0, res == 0, carry, ov];
                }
                res
            }
        } else {
            let l32 = lhs as u32;
            let r32 = rhs as u32;
            if is_sub {
                let (res, borrow) = l32.overflowing_sub(r32);
                if set_flags {
                    let ov = (((l32 ^ r32) & (l32 ^ res)) >> 31) != 0;
                    self.nzcv = [(res >> 31) != 0, res == 0, !borrow, ov];
                }
                u64::from(res)
            } else {
                let (res, carry) = l32.overflowing_add(r32);
                if set_flags {
                    let ov = ((!(l32 ^ r32) & (l32 ^ res)) >> 31) != 0;
                    self.nzcv = [(res >> 31) != 0, res == 0, carry, ov];
                }
                u64::from(res)
            }
        }
    }

    fn set_logic_flags(&mut self, res: u64, sf: bool) {
        if sf {
            self.nzcv = [(res >> 63) != 0, res == 0, false, false];
        } else {
            let r32 = res as u32;
            self.nzcv = [(r32 >> 31) != 0, r32 == 0, false, false];
        }
    }
}

fn shift_reg(val: u64, amt: u32, shift_type: u32, sf: bool) -> u64 {
    if amt == 0 {
        return if sf { val } else { val & 0xFFFF_FFFF };
    }
    if sf {
        let a = amt & 63;
        match shift_type {
            0 => val << a,
            1 => val >> a,
            2 => ((val as i64) >> a) as u64,
            _ => val.rotate_right(a),
        }
    } else {
        let v = val as u32;
        let a = amt & 31;
        let r = match shift_type {
            0 => v << a,
            1 => v >> a,
            2 => ((v as i32) >> a) as u32,
            _ => v.rotate_right(a),
        };
        u64::from(r)
    }
}

fn extend_reg(val: u64, option: u32) -> u64 {
    match option & 7 {
        0 => val & 0xFF,
        1 => val & 0xFFFF,
        2 => val & 0xFFFF_FFFF,
        3 | 7 => val,
        4 => val as u8 as i8 as i64 as u64,
        5 => val as u16 as i16 as i64 as u64,
        _ => val as u32 as i32 as i64 as u64,
    }
}

fn decode_bitmask_imm(insn: u32, sf: bool) -> Result<u64> {
    let n = (insn >> 22) & 1;
    let immr = (insn >> 16) & 0x3F;
    let imms = (insn >> 10) & 0x3F;
    let combined = (n << 6) | (!imms & 0x3F);
    if combined == 0 {
        return Err(PluginHostError::CpuTrap("invalid AArch64 bitmask immediate".into()));
    }
    let len = 31 - combined.leading_zeros();
    let esize = 1u32 << len;
    let levels = esize - 1;
    let s = imms & levels;
    let r = immr & levels;
    let welem = if s + 1 >= 64 { u64::MAX } else { (1u64 << (s + 1)) - 1 };
    let rot = if r == 0 {
        welem
    } else if esize == 64 {
        welem.rotate_right(r)
    } else {
        let emask = (1u64 << esize) - 1;
        ((welem >> r) | (welem << (esize - r))) & emask
    };
    let mut out = 0u64;
    let mut pos = 0u32;
    while pos < 64 {
        out |= rot << pos;
        pos += esize;
    }
    Ok(if sf { out } else { out & 0xFFFF_FFFF })
}

fn decode_arm64_fp_imm8(imm8: u8) -> f64 {
    let sign = if (imm8 & 0x80) != 0 { -1.0 } else { 1.0 };
    // ARMv8 VFPExpandImm: `imm8<6>` is `NOT(exp<2>)` -> `0` means +1..=+4, `1` means -3..=0!
    let exp = if (imm8 & 0x40) == 0 { i32::from((imm8 >> 4) & 3) + 1 } else { i32::from((imm8 >> 4) & 3) - 3 };
    let mant = 1.0 + f64::from(imm8 & 0x0F) / 16.0;
    sign * mant * 2.0f64.powi(exp)
}

fn read_fp_mem(mem: &PagedMemory, addr: u64, byte_size: usize) -> Result<u128> {
    match byte_size {
        1 => Ok(u128::from(mem.read_u8(addr)?)),
        2 => Ok(u128::from(mem.read_u16(addr)?)),
        4 => Ok(u128::from(mem.read_u32(addr)?)),
        8 => Ok(u128::from(mem.read_u64(addr)?)),
        _ => mem.read_u128(addr),
    }
}

fn write_fp_mem(mem: &mut PagedMemory, addr: u64, byte_size: usize, val: u128) -> Result<()> {
    match byte_size {
        1 => mem.write_u8(addr, val as u8),
        2 => mem.write_u16(addr, val as u16),
        4 => mem.write_u32(addr, val as u32),
        8 => mem.write_u64(addr, val as u64),
        _ => mem.write_u128(addr, val),
    }
}
