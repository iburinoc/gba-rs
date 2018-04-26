use std::collections::HashSet;
use std::default::Default;
use std::iter::IntoIterator;

use shared::Shared;

use mmu::Mmu;

mod arm;
mod thumb;
mod util;
pub mod reg;

use self::reg::*;

pub struct Cpu<T: Mmu> {
    reg: RegFile,
    mmu: Shared<T>,
    brk: HashSet<u32>,
}

impl<T: Mmu> Cpu<T> {
    pub fn new<'a, I>(mmu: Shared<T>, regs: I) -> Cpu<T>
    where
        I: IntoIterator<Item = &'a (Reg, u32)>,
    {
        let mut cpu = Cpu {
            reg: Default::default(),
            mmu: mmu,
            brk: Default::default(),
        };
        cpu.init(regs);

        cpu
    }

    fn init<'a, I>(&mut self, regs: I)
    where
        I: IntoIterator<Item = &'a (Reg, u32)>,
    {
        // start in system mode
        self.reg.set(0, reg::CPSR, 0x1F);
        for &(reg, val) in regs.into_iter() {
            self.reg[reg] = val;
        }
    }

    pub fn set_breaks<'a, I>(&mut self, brks: I)
    where
        I: IntoIterator<Item = &'a u32>
    {
        for addr in brks.into_iter() {
            self.brk.insert(*addr);
        }
    }

    pub fn run(&mut self) {
        let mut run = true;
        while run {
            run = self.cycle();
        }
    }

    pub fn cycle(&mut self) -> bool {
        if self.brk.contains(&self.reg[reg::PC]) {
            debug!("Breakpoint {:#010x} hit!", self.reg[reg::PC]);
        }
        if !self.thumb_mode() {
            self.execute_arm()
        } else {
            self.execute_thumb()
        }
    }

    pub fn set_thumb_mode(&mut self, thumb: bool) {
        let mask = 1u32 << cpsr::T;
        let cpsr = self.reg[reg::CPSR];
        self.reg[reg::CPSR] = (cpsr & !mask) | ((thumb as u32) * mask);
    }

    fn thumb_mode(&self) -> bool {
        (self.reg[reg::CPSR] & (1u32 << cpsr::T)) != 0
    }
}
