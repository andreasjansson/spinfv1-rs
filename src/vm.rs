//! The FV-1 virtual machine: registers, delay RAM, LFOs and the
//! per-sample execution loop.

use alloc::boxed::Box;
use alloc::vec;

use crate::fixed::{
    ACC_MAX, ACC_MIN, ONE_S23, f32_to_s23, mul_s1_9, mul_s1_14, mul_s23, s23_to_f32, sat24, sx24,
};
use crate::instruction::{ChoFlags, Instruction, LfoSel, SkpCond, reg};
use crate::lfo::{ChoValue, RampLfo, SinLfo};
use crate::math;
use crate::program::{PROGRAM_LEN, Program};

/// Delay RAM size in samples (~1 second at 32,768 Hz).
pub const DELAY_LEN: usize = 32768;

const DELAY_MASK: i32 = DELAY_LEN as i32 - 1;

/// The chip's native sample rate in Hz. The emulator itself is rate-agnostic
/// (it processes one sample per call); run it at this rate for authentic
/// delay times and LFO frequencies.
pub const SAMPLE_RATE: f64 = 32768.0;

/// A complete FV-1: program memory, 64 registers, 32K-sample delay RAM,
/// two SIN and two RMP LFOs, and the S.23 saturating ALU.
///
/// # Examples
/// ```
/// use spinfv1::{Fv1, Instruction, Program, reg};
///
/// // Pass-through: ACC = ADCL, DACL = ACC.
/// let program = Program::from_instructions(&[
///     Instruction::ldax(reg::ADCL),
///     Instruction::Wrax { reg: reg::DACL, c: 0 },
/// ])
/// .unwrap();
///
/// let mut fv1 = Fv1::new();
/// fv1.load_program(&program);
/// let (l, _r) = fv1.process(0.5, 0.0);
/// assert!((l - 0.5).abs() < 1e-6);
/// ```
pub struct Fv1 {
    instructions: [Instruction; PROGRAM_LEN],
    delay: Box<[i32]>,
    /// Base pointer into delay RAM; decremented every sample.
    cursor: i32,
    regs: [i32; 64],
    acc: i32,
    /// ACC as it was *before the previous* executed instruction — the
    /// hardware pipelines the accumulator by one instruction.
    pacc: i32,
    /// ACC before the current instruction (becomes `pacc` next instruction).
    prev_acc: i32,
    /// Last value read from delay RAM (used by WRAP).
    lr: i32,
    first_run: bool,
    sin_lfo: [SinLfo; 2],
    rmp_lfo: [RampLfo; 2],
    pots: [i32; 3],
    quantize_delay_14bit: bool,
    clear_delay_on_load: bool,
    adc_noise: bool,
    noise_seed: u32,
    noise_state: u32,
    lfo_reads_zero: bool,
    model: ChipModel,
    /// One-pole smoothing of the pots, per sample (0 = none). See `set_pot_smoothing`.
    pot_k: f64,
    pot_state: [f64; 3],
    pot_set: [bool; 3],
}

/// Which chip behaviour the arithmetic models.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipModel {
    /// spinfv1's model (the default): MULX at full precision, LOG/EXP rounded at full
    /// precision, and a 3-bit-exponent delay-RAM word. The golden vectors and the
    /// independent-VM captures in `tests/` pin this model.
    Reference,
    /// The FV-1 as measured bit for bit on a Happy Nerding FX AID XL (2026-10-09, by probe
    /// programs recorded through the module's codec): MULX truncates its register operand to
    /// 14 fractional bits, LOG returns log2 on a 2^-12 grid, EXP keeps about 12 significant
    /// bits, both rounded down, and delay RAM stores a 4-bit-exponent, 9-bit-fraction word,
    /// rounded towards minus infinity. See [`Fv1::fx_aid_xl`].
    FxAidXl,
}

/// The FX AID XL's pots (knob + CV) settle with a first-order lag of about 123 ms (measured
/// 2026-10-09: a CV step gives t10 = 12.9 ms, t90 = 283 ms, ratio 21.9 as for one pole).
/// Per-sample coefficient at the chip rate measured then (32,690 Hz).
pub const XL_POT_SMOOTHING: f64 = 1.0 / (0.123 * 32_690.0);

impl Fv1 {
    /// A fresh FV-1 with an all-NOP program (outputs silence).
    #[must_use]
    pub fn new() -> Self {
        Self {
            instructions: Program::default().instructions(),
            delay: vec![0; DELAY_LEN].into_boxed_slice(),
            cursor: 0,
            regs: [0; 64],
            acc: 0,
            pacc: 0,
            prev_acc: 0,
            lr: 0,
            first_run: true,
            sin_lfo: [SinLfo::new(), SinLfo::new()],
            rmp_lfo: [RampLfo::new(), RampLfo::new()],
            pots: [0; 3],
            quantize_delay_14bit: true,
            clear_delay_on_load: true,
            adc_noise: false,
            noise_seed: 1,
            noise_state: 1,
            lfo_reads_zero: false,
            model: ChipModel::Reference,
            pot_k: 0.0,
            pot_state: [0.0; 3],
            pot_set: [false; 3],
        }
    }

    /// Load a program and reset DSP state (registers, LFOs, accumulator),
    /// as if the chip had just switched programs. Delay RAM is cleared
    /// too unless [`set_clear_delay_on_load`](Self::set_clear_delay_on_load)
    /// disabled that.
    pub fn load_program(&mut self, program: &Program) {
        self.instructions = program.instructions();
        if self.clear_delay_on_load {
            self.delay.fill(0);
        }
        self.reset_registers_and_lfos();
    }

    /// An FV-1 as it behaves in a Happy Nerding FX AID XL: [`ChipModel::FxAidXl`] arithmetic,
    /// LFO-rate/range, DACL/DACR and ADDR_PTR registers that read back as 0
    /// ([`Self::set_lfo_reads_zero`]), and the module's pot lag ([`XL_POT_SMOOTHING`]).
    /// Its chip runs at `resampler::XL_CHIP_RATE` (with the `resampler` feature:
    /// `HostedFv1::with_chip_rate(chip, host_rate, XL_CHIP_RATE)`).
    #[must_use]
    pub fn fx_aid_xl() -> Self {
        let mut fv1 = Self::new();
        fv1.set_model(ChipModel::FxAidXl);
        fv1.set_lfo_reads_zero(true);
        fv1.set_pot_smoothing(XL_POT_SMOOTHING);
        fv1
    }

    /// Choose the arithmetic model ([`ChipModel::Reference`] by default).
    pub fn set_model(&mut self, model: ChipModel) {
        self.model = model;
    }

    /// The arithmetic model in use.
    #[must_use]
    pub fn model(&self) -> ChipModel {
        self.model
    }

    /// Switch programs the way a slot change on the FX AID XL does (measured 2026-10-09):
    /// REG0-31 and the delay RAM start at 0, the `skp run` block runs again, and the LFOs keep
    /// running with the previous program's settings.
    pub fn switch_program_keep_state(&mut self, program: &Program) {
        self.instructions = program.instructions();
        self.first_run = true;
        for r in &mut self.regs[0x20..0x40] {
            *r = 0;
        }
        for v in self.delay.iter_mut() {
            *v = 0;
        }
    }

    /// Fill REG0..=REG31 with full-scale pseudo-random values: a harsh
    /// stand-in for whatever a previous program left in them.
    pub fn scramble_registers(&mut self, seed: u32) {
        let mut state = seed | 1;
        for reg in &mut self.regs[0x20..0x40] {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *reg = (state as i32) >> 8;
        }
    }

    /// Whether a program load also clears delay RAM (the default). Disable
    /// to keep the previous program's delay contents across switches, so
    /// tails carry over instead of cutting to silence — hosts that switch
    /// programs frequently sound far less abrupt this way, and programs
    /// that mine old RAM contents as a noise source keep material to read.
    pub fn set_clear_delay_on_load(&mut self, enabled: bool) {
        self.clear_delay_on_load = enabled;
    }

    /// Reset all DSP state without changing the loaded program.
    pub fn reset(&mut self) {
        self.delay.fill(0);
        self.reset_registers_and_lfos();
    }

    fn reset_registers_and_lfos(&mut self) {
        self.cursor = 0;
        self.regs = [0; 64];
        self.acc = 0;
        self.pacc = 0;
        self.prev_acc = 0;
        self.lr = 0;
        self.first_run = true;
        self.sin_lfo = [SinLfo::new(), SinLfo::new()];
        self.rmp_lfo = [RampLfo::new(), RampLfo::new()];
        self.noise_state = self.noise_seed | 1;
    }

    /// Fill delay RAM with a deterministic pseudo-random noise floor, like
    /// the uninitialized SRAM of a freshly powered chip. Some programs read
    /// regions they never write and rely on that content as a noise
    /// source; with zeroed RAM those programs are silent. The pattern is
    /// quiet (about -60 dBFS): noise-mining programs only need random low
    /// bits, while full-scale garbage rings painfully loud through any
    /// feedback network that reads it. The same seed always produces the
    /// same pattern.
    pub fn randomize_delay_ram(&mut self, seed: u32) {
        let mut state = seed | 1;
        for cell in self.delay.iter_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *cell = ((state >> 8) as i32) << 8 >> 18;
        }
    }

    /// Fill the general-purpose registers (REG0..=REG31) with a
    /// deterministic pseudo-random pattern, like uninitialized power-up
    /// state. Programs that seed a software noise generator from a
    /// register they never wrote need this; a zero seed keeps such
    /// generators stuck at silence. The values are quiet (about
    /// -60 dBFS) for the same reason as
    /// [`randomize_delay_ram`](Self::randomize_delay_ram): generator
    /// seeds only need random bits, while a full-scale value dropped
    /// into a filter or feedback state rings loudly through the program.
    pub fn randomize_registers(&mut self, seed: u32) {
        let mut state = seed ^ 0x9E37_79B9;
        for reg in &mut self.regs[0x20..0x40] {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *reg = ((state >> 8) as i32) << 8 >> 18;
        }
    }

    /// Model the ADC noise floor: a deterministic dither of up to a few
    /// hundred counts (about -96 dBFS) added to each input sample.
    /// Hardware inputs always carry analog noise in the low bits; programs
    /// that expand those bits as a noise source are silent against
    /// digitally clean input without it. The pattern restarts from the
    /// seed on every reset/program load.
    pub fn set_adc_noise(&mut self, enabled: bool, seed: u32) {
        self.adc_noise = enabled;
        self.noise_seed = seed;
        self.noise_state = seed | 1;
    }

    /// Set potentiometer `idx` (0..=2) to `value` in `[0.0, 1.0]`.
    ///
    /// The value is taken as given at full S.23 resolution: input
    /// conditioning — the physical pot's taper, the chip ADC's ~10-bit
    /// resolution — is the host's domain, not the emulator's.
    pub fn set_pot(&mut self, idx: usize, value: f32) {
        if let Some(pot) = self.pots.get_mut(idx) {
            *pot = f32_to_s23(value.clamp(0.0, 1.0));
            if !self.pot_set[idx] {
                // the first setting is where the knob already is: no glide at load
                self.pot_state[idx] = f64::from(*pot);
                self.pot_set[idx] = true;
            }
        }
    }

    /// Per-sample one-pole coefficient for the pot lag. 0, the default, turns it off: the POT
    /// registers follow `set_pot` at once. [`Self::fx_aid_xl`] sets [`XL_POT_SMOOTHING`], the
    /// module's measured lag.
    pub fn set_pot_smoothing(&mut self, k: f64) {
        self.pot_k = k.clamp(0.0, 1.0);
    }

    /// The real chip stores samples in 14-bit delay RAM using "a compressed
    /// floating point format" (FV-1 datasheet, Delay SRAM section), and the
    /// emulator models that by default: delay writes pass through a
    /// sign + mantissa + exponent model for a hardware-flavored noise
    /// floor. Spin does not document the exact field split, so the model
    /// keeps 10 significant magnitude bits across an exponent-capped
    /// range — small signals quantize much finer than a linear 14-bit
    /// store would, which is the point of the compressed format. Disable
    /// for pristine full 24-bit delay storage.
    pub fn set_delay_quantization(&mut self, enabled: bool) {
        self.quantize_delay_14bit = enabled;
    }

    /// Current accumulator, raw S.23.
    #[must_use]
    pub fn acc(&self) -> i32 {
        self.acc
    }

    /// Make instruction reads of the LFO rate/range registers (addresses 0..=7), DACL/DACR
    /// and ADDR_PTR return 0, whatever was written, as on the FX AID XL (off by default;
    /// [`Self::fx_aid_xl`] turns it on). On the module every program that read RMPx_RATE
    /// back as a constant was silent, exactly as with zero readback, and probe programs
    /// confirmed it bit for bit for all of these registers.
    pub fn set_lfo_reads_zero(&mut self, enabled: bool) {
        self.lfo_reads_zero = enabled;
    }

    /// Register read as an instruction sees it (see [`Self::set_lfo_reads_zero`]).
    fn read_reg(&self, reg: u8) -> i32 {
        let r = reg & 0x3F;
        if self.lfo_reads_zero && (r < 8 || r == reg::DACL || r == reg::DACR || r == reg::ADDR_PTR)
        {
            0
        } else {
            self.regs[usize::from(r)]
        }
    }

    /// Raw S.23 value of register `r` (6-bit address).
    #[must_use]
    pub fn register(&self, r: u8) -> i32 {
        self.regs[usize::from(r & 0x3F)]
    }

    /// Read-only view of delay RAM (absolute addresses, raw S.23).
    #[must_use]
    pub fn delay_ram(&self) -> &[i32] {
        &self.delay
    }

    /// Process one stereo sample (floats in `[-1.0, 1.0)`).
    pub fn process(&mut self, in_l: f32, in_r: f32) -> (f32, f32) {
        #[cfg(feature = "satcount")]
        crate::fixed::satcount::set_pc(128);
        let (l, r) = self.process_raw(f32_to_s23(in_l), f32_to_s23(in_r));
        (s23_to_f32(l), s23_to_f32(r))
    }

    /// Process buffers in one call; all four slices must be the same length.
    ///
    /// # Panics
    /// Panics if slice lengths differ.
    pub fn process_block(
        &mut self,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        assert!(
            in_l.len() == in_r.len() && in_l.len() == out_l.len() && in_l.len() == out_r.len(),
            "process_block: all slices must have equal length"
        );
        for i in 0..in_l.len() {
            (out_l[i], out_r[i]) = self.process(in_l[i], in_r[i]);
        }
    }

    /// Process one stereo sample in raw S.23.
    #[allow(clippy::too_many_lines, reason = "one match arm per FV-1 instruction")]
    pub fn process_raw(&mut self, in_l: i32, in_r: i32) -> (i32, i32) {
        let (in_l, in_r) = if self.adc_noise {
            let mut next = || {
                self.noise_state = self
                    .noise_state
                    .wrapping_mul(1_664_525)
                    .wrapping_add(1_013_904_223);
                ((self.noise_state >> 8) & 0xFF) as i32 - 128
            };
            let (nl, nr) = (next(), next());
            (in_l.saturating_add(nl), in_r.saturating_add(nr))
        } else {
            (in_l, in_r)
        };
        #[cfg(feature = "satcount")]
        crate::fixed::satcount::set_pc(128);
        self.regs[usize::from(reg::ADCL)] = sat24(i64::from(in_l));
        self.regs[usize::from(reg::ADCR)] = sat24(i64::from(in_r));
        for i in 0..3 {
            let target = f64::from(self.pots[i]);
            if self.pot_k > 0.0 && self.pot_set[i] {
                self.pot_state[i] += (target - self.pot_state[i]) * self.pot_k;
            } else {
                self.pot_state[i] = target;
            }
            self.regs[usize::from(reg::POT0) + i] = self.pot_state[i] as i32;
        }

        let mut ic = 0usize;
        while ic < PROGRAM_LEN {
            let ins = self.instructions[ic];
            #[cfg(feature = "satcount")]
            crate::fixed::satcount::set_pc(ic);
            ic += 1;
            let acc = i64::from(self.acc);
            match ins {
                Instruction::Rda { addr, c } => {
                    let s = self.delay_read(i32::from(addr));
                    self.acc = sat24(mul_s1_9(i64::from(s), i64::from(c)) + acc);
                }
                Instruction::Rmpa { c } => {
                    let ptr = (self.regs[usize::from(reg::ADDR_PTR)] >> 8) & DELAY_MASK;
                    let s = self.delay_read(ptr);
                    self.acc = sat24(mul_s1_9(i64::from(s), i64::from(c)) + acc);
                }
                Instruction::Wra { addr, c } => {
                    self.delay_write(i32::from(addr), self.acc);
                    self.acc = sat24(mul_s1_9(acc, i64::from(c)));
                }
                Instruction::Wrap { addr, c } => {
                    self.delay_write(i32::from(addr), self.acc);
                    self.acc = sat24(mul_s1_9(acc, i64::from(c)) + i64::from(self.lr));
                }
                Instruction::Rdax { reg, c } => {
                    let r = i64::from(self.read_reg(reg));
                    self.acc = sat24(mul_s1_14(r, i64::from(c)) + acc);
                }
                Instruction::Rdfx { reg, c } => {
                    let r = i64::from(self.read_reg(reg));
                    self.acc = sat24(mul_s1_14(acc - r, i64::from(c)) + r);
                }
                Instruction::Wrax { reg, c } => {
                    self.regs[usize::from(reg & 0x3F)] = self.acc;
                    self.acc = sat24(mul_s1_14(acc, i64::from(c)));
                }
                Instruction::Wrhx { reg, c } => {
                    self.regs[usize::from(reg & 0x3F)] = self.acc;
                    self.acc = sat24(mul_s1_14(acc, i64::from(c)) + i64::from(self.pacc));
                }
                Instruction::Wrlx { reg, c } => {
                    self.regs[usize::from(reg & 0x3F)] = self.acc;
                    let pacc = i64::from(self.pacc);
                    self.acc = sat24(mul_s1_14(pacc - acc, i64::from(c)) + pacc);
                }
                Instruction::Maxx { reg, c } => {
                    let r = i64::from(self.read_reg(reg));
                    let rxc = mul_s1_14(r, i64::from(c)).abs();
                    self.acc = sat24(rxc.max(acc.abs()));
                }
                Instruction::Mulx { reg } => {
                    // FxAidXl: the chip multiplies ACC by the register truncated to 14
                    // fractional bits (its low 9 bits dropped, as a coefficient would be),
                    // product floored: bit-exact on 240/240 measured frames. Full precision
                    // was off by up to ~2^-14 relative.
                    let r = self.read_reg(reg);
                    let r = i64::from(if self.model == ChipModel::FxAidXl {
                        r >> 9 << 9
                    } else {
                        r
                    });
                    self.acc = sat24(mul_s23(acc, r));
                }
                Instruction::Log { c, d } => {
                    // ACC is treated as S4.19 in the log domain. FxAidXl: the chip returns
                    // log2 on a grid of 2^-12 (low 7 bits of the S4.19 result zero); floor to
                    // that grid is the closest simple model (errors stay within ~3 grid steps,
                    // ~1 cent; the exact curve is not known).
                    let l = if self.acc == 0 {
                        i64::from(ACC_MIN)
                    } else {
                        let mag = f64::from(self.acc.unsigned_abs()) / f64::from(ONE_S23);
                        if self.model == ChipModel::FxAidXl {
                            let q = math::floor(math::log2(mag) * f64::from(1 << 12)) as i64;
                            sat24(q << 7).into()
                        } else {
                            sat24(math::round(math::log2(mag) * f64::from(1 << 19)) as i64).into()
                        }
                    };
                    self.acc = sat24(mul_s1_14(l, i64::from(c)) + (i64::from(d) << 13));
                }
                Instruction::Exp { c, d } => {
                    // ACC (S4.19) exponentiated; 2^x >= 1.0 saturates. FxAidXl: the chip keeps
                    // ~12 significant bits of the result, rounded down (124/240 measured frames
                    // bit-exact with this model, rms error 2.4e-4 vs 1.1e-3 for full precision;
                    // the exact curve is not known).
                    let e = if self.acc >= 0 {
                        i64::from(ACC_MAX)
                    } else {
                        let x = f64::from(self.acc) / f64::from(1 << 19);
                        if self.model == ChipModel::FxAidXl {
                            let v = math::floor(math::exp2(x) * f64::from(ONE_S23)) as i64;
                            let bits = 64 - (v.max(1) as u64).leading_zeros() as i64;
                            let sh = (bits - 12).max(0);
                            (v >> sh) << sh
                        } else {
                            math::round(math::exp2(x) * f64::from(ONE_S23)) as i64
                        }
                    };
                    self.acc = sat24(mul_s1_14(e, i64::from(c)) + (i64::from(d) << 13));
                }
                Instruction::Sof { c, d } => {
                    self.acc = sat24(mul_s1_14(acc, i64::from(c)) + (i64::from(d) << 13));
                }
                Instruction::And { mask } => {
                    self.acc = sx24(self.acc & mask as i32);
                }
                Instruction::Or { mask } => {
                    self.acc = sx24((self.acc & 0xFF_FFFF) | mask as i32);
                }
                Instruction::Xor { mask } => {
                    self.acc = sx24((self.acc & 0xFF_FFFF) ^ mask as i32);
                }
                Instruction::Skp { cond, n } => {
                    // An empty condition mask skips unconditionally
                    // (vacuous truth over "all set conditions hold"):
                    // `SKP 0,N` is the chip's unconditional jump, and
                    // `SKP 0,0` is NOP either way.
                    if self.skp_taken(cond) {
                        ic += usize::from(n);
                    }
                }
                Instruction::Wlds { lfo, freq, amp } => {
                    let base = if lfo { reg::SIN1_RATE } else { reg::SIN0_RATE };
                    self.regs[usize::from(base)] = i32::from(freq) << 14;
                    self.regs[usize::from(base) + 1] = i32::from(amp) << 8;
                }
                Instruction::Wldr { lfo, freq, amp } => {
                    let base = if lfo { reg::RMP1_RATE } else { reg::RMP0_RATE };
                    self.regs[usize::from(base)] = i32::from(freq) << 8;
                    self.regs[usize::from(base) + 1] = (amp.code() as i32) << 21;
                }
                Instruction::Jam { lfo } => {
                    self.rmp_lfo[usize::from(lfo)].jam();
                }
                Instruction::ChoRda { lfo, flags, addr } => {
                    let ChoValue { offset, coeff } = self.cho_value(lfo, flags);
                    let s = self.delay_read(i32::from(addr).wrapping_add(offset));
                    self.acc = sat24(mul_s23(i64::from(s), i64::from(coeff)) + acc);
                }
                Instruction::ChoSof { lfo, flags, d } => {
                    let ChoValue { coeff, .. } = self.cho_value(lfo, flags);
                    self.acc = sat24(mul_s23(acc, i64::from(coeff)) + (i64::from(d) << 8));
                }
                Instruction::ChoRdal { lfo, flags } => {
                    // The LFO value is ADDED to ACC, not loaded over it:
                    // RDAL shares the RDA datapath. The datasheet's
                    // "loads" holds only because the common idiom clears
                    // ACC first (`wrax reg,0` / `wra addr,0` right
                    // before). Programs that compute a ramp rate in ACC
                    // and then `cho rdal,rmp0 / wrax rmp0_rate` depend on
                    // the addition — with a load, the computed rate is
                    // discarded and a rate-0 ramp servo deadlocks at
                    // zero, freezing the modulation.
                    self.acc = sat24(
                        acc + i64::from(match lfo {
                            LfoSel::Sin0 => self.sin_lfo[0].value(
                                self.lfo_reg(reg::SIN0_RANGE),
                                flags.contains(ChoFlags::COS),
                            ),
                            LfoSel::Sin1 => self.sin_lfo[1].value(
                                self.lfo_reg(reg::SIN1_RANGE),
                                flags.contains(ChoFlags::COS),
                            ),
                            LfoSel::Rmp0 => self.rmp_lfo[0].value(),
                            LfoSel::Rmp1 => self.rmp_lfo[1].value(),
                        }),
                    );
                }
                Instruction::Raw(_) => {}
            }
            // The accumulator pipeline: PACC lags ACC by one instruction, so
            // during instruction N it holds ACC as of the start of N-1.
            self.pacc = self.prev_acc;
            self.prev_acc = self.acc;
        }

        self.first_run = false;
        self.cursor = (self.cursor - 1) & DELAY_MASK;
        self.sin_lfo[0].tick(self.lfo_reg(reg::SIN0_RATE));
        self.sin_lfo[1].tick(self.lfo_reg(reg::SIN1_RATE));
        self.rmp_lfo[0].tick(self.lfo_reg(reg::RMP0_RATE), self.lfo_reg(reg::RMP0_RANGE));
        self.rmp_lfo[1].tick(self.lfo_reg(reg::RMP1_RATE), self.lfo_reg(reg::RMP1_RANGE));

        (
            self.regs[usize::from(reg::DACL)],
            self.regs[usize::from(reg::DACR)],
        )
    }

    fn skp_taken(&self, cond: SkpCond) -> bool {
        let mut take = true;
        if cond.contains(SkpCond::NEG) {
            take &= self.acc < 0;
        }
        if cond.contains(SkpCond::GEZ) {
            take &= self.acc >= 0;
        }
        if cond.contains(SkpCond::ZRO) {
            take &= self.acc == 0;
        }
        if cond.contains(SkpCond::ZRC) {
            take &= (self.acc >= 0) != (self.pacc >= 0);
        }
        if cond.contains(SkpCond::RUN) {
            take &= !self.first_run;
        }
        take
    }

    fn lfo_reg(&self, r: u8) -> i32 {
        self.regs[usize::from(r)]
    }

    fn cho_value(&self, lfo: LfoSel, flags: ChoFlags) -> ChoValue {
        match lfo {
            LfoSel::Sin0 => self.sin_lfo[0].cho(self.lfo_reg(reg::SIN0_RANGE), flags),
            LfoSel::Sin1 => self.sin_lfo[1].cho(self.lfo_reg(reg::SIN1_RANGE), flags),
            LfoSel::Rmp0 => self.rmp_lfo[0].cho(self.lfo_reg(reg::RMP0_RANGE), flags),
            LfoSel::Rmp1 => self.rmp_lfo[1].cho(self.lfo_reg(reg::RMP1_RANGE), flags),
        }
    }

    fn delay_read(&mut self, offset: i32) -> i32 {
        let idx = (self.cursor.wrapping_add(offset) & DELAY_MASK) as usize;
        let v = self.delay[idx];
        self.lr = v;
        v
    }

    fn delay_write(&mut self, offset: i32, value: i32) {
        let idx = (self.cursor.wrapping_add(offset) & DELAY_MASK) as usize;
        self.delay[idx] = if self.quantize_delay_14bit {
            match self.model {
                ChipModel::Reference => compress14(value),
                ChipModel::FxAidXl => compress14_xl(value),
            }
        } else {
            value
        };
    }
}

/// Model of the chip's 14-bit compressed floating-point delay-RAM word:
/// sign, 10-bit mantissa, 3-bit exponent. The magnitude keeps its top 10
/// significant bits; the quantization step scales with level from 2^6
/// (signals below −42 dBFS) up to 2^13 (full scale), so relative precision
/// stays roughly constant instead of degrading at low level the way a
/// linear 14-bit store would. Spin documents the format's existence but
/// not its field split, so the bit allocation here is a documented model,
/// not a verified silicon layout.
fn compress14(v: i32) -> i32 {
    let mag = v.unsigned_abs().min(0x7F_FFFF);
    let bits = 32 - mag.leading_zeros() as i32;
    let shift = (bits - 10).clamp(6, 13) as u32;
    let q = ((mag >> shift) << shift) as i32;
    if v < 0 { -q } else { q }
}

/// The chip's 14-bit floating-point delay-RAM word as measured on the FX AID XL (2026-10-09,
/// 1,440/1,440 values bit-exact): sign, 4-bit exponent, 9-bit fraction. A value keeps its top
/// 10 significant bits; below 2^-14 it is stored exactly (the step there is 2^-23, as for
/// denormals). The low bits are dropped from the two's-complement value, so the result is
/// always rounded down (towards minus infinity). [`compress14`] matched 13 of the 1,440.
fn compress14_xl(v: i32) -> i32 {
    let bits = 32 - v.unsigned_abs().leading_zeros() as i32;
    let shift = (bits - 10).max(0) as u32;
    (v >> shift) << shift
}

impl Default for Fv1 {
    fn default() -> Self {
        Self::new()
    }
}
