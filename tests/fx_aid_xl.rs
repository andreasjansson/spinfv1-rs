//! The FX AID XL model (`Fv1::fx_aid_xl`, `ChipModel::FxAidXl`): the arithmetic, register
//! readback and pot lag measured on the module, each checked against the reference model.

use spinfv1::fixed::ONE_S23;
use spinfv1::{ChipModel, Fv1, Instruction, Program, XL_POT_SMOOTHING, coeff, reg};

const OUT: Instruction = Instruction::Wrax {
    reg: reg::DACL,
    c: 0,
};

fn load(mut fv1: Fv1, instructions: &[Instruction]) -> Fv1 {
    fv1.load_program(&Program::from_instructions(instructions).expect("fits"));
    fv1
}

/// One sample of `instructions` on the XL model and on the reference model: raw DACL of each.
fn both(instructions: &[Instruction], input: i32) -> (i32, i32) {
    let mut xl = load(Fv1::fx_aid_xl(), instructions);
    let mut reference = load(Fv1::new(), instructions);
    (
        xl.process_raw(input, 0).0,
        reference.process_raw(input, 0).0,
    )
}

#[test]
fn reference_is_the_default() {
    assert_eq!(Fv1::new().model(), ChipModel::Reference);
    assert_eq!(Fv1::fx_aid_xl().model(), ChipModel::FxAidXl);
}

#[test]
fn mulx_truncates_the_register_to_14_fractional_bits() {
    let program = [
        Instruction::ldax(reg::ADCL),
        Instruction::Wrax {
            reg: reg::user(0),
            c: coeff::s1_14(1.0),
        },
        Instruction::Mulx { reg: reg::user(0) },
        OUT,
    ];
    let x = 0x2A_AAAA; // about 1/3, with low bits set
    let (xl, reference) = both(&program, x);
    let x = i64::from(x);
    assert_eq!(i64::from(xl), (x * (x >> 9 << 9)) >> 23);
    assert_eq!(i64::from(reference), (x * x) >> 23);
    assert_ne!(xl, reference);
}

#[test]
fn log_lands_on_a_grid_of_2_to_the_minus_12() {
    let program = [
        Instruction::ldax(reg::ADCL),
        Instruction::Log {
            c: coeff::s1_14(1.0),
            d: 0,
        },
        OUT,
    ];
    let mut off_grid = 0;
    for x in [1_234_567, 3_000_001, 77_777, 5_555_555, 999] {
        let (xl, reference) = both(&program, x);
        let exact = (f64::from(x) / f64::from(ONE_S23)).log2() * 4096.0;
        assert_eq!(xl, (exact.floor() as i32) << 7, "LOG of {x}");
        off_grid += i32::from(reference & 0x7F != 0);
    }
    assert!(off_grid > 0, "the reference model keeps the full precision");
}

#[test]
fn exp_keeps_12_significant_bits_rounded_down() {
    let program = [
        Instruction::ldax(reg::ADCL),
        Instruction::Exp {
            c: coeff::s1_14(1.0),
            d: 0,
        },
        OUT,
    ];
    for x in [-1_000_003, -333_333, -4_111_111, -77] {
        let (xl, reference) = both(&program, x);
        let exact = (f64::from(x) / f64::from(1 << 19)).exp2() * f64::from(ONE_S23);
        let shift = (32 - (xl as u32).leading_zeros()).saturating_sub(12);
        assert_eq!(xl >> shift << shift, xl, "EXP of {x} has more than 12 bits");
        assert!(f64::from(xl) <= exact && exact - f64::from(xl) < f64::from(1 << shift) + 1.0);
        assert!((f64::from(reference) - exact).abs() <= 0.5 + 1e-6);
    }
}

#[test]
fn delay_ram_keeps_10_significant_bits_rounded_towards_minus_infinity() {
    let program = [
        Instruction::ldax(reg::ADCL),
        Instruction::Wra { addr: 100, c: 0 },
        Instruction::Rda {
            addr: 100,
            c: coeff::s1_9(1.0),
        },
        OUT,
    ];
    let (xl, reference) = both(&program, -123_457);
    assert_eq!(xl, -123_457 >> 7 << 7, "floor to the top 10 bits");
    assert_eq!(
        reference,
        -(123_457 >> 7 << 7),
        "the reference truncates the magnitude"
    );
    let (xl, reference) = both(&program, 300);
    assert_eq!(xl, 300, "below 2^-14 the XL stores exactly");
    assert_eq!(reference, 256);
}

#[test]
fn lfo_and_dac_registers_read_back_zero() {
    for target in [reg::RMP0_RATE, reg::SIN1_RANGE, reg::DACR, reg::ADDR_PTR] {
        let program = [
            Instruction::Sof {
                c: 0,
                d: coeff::s_10(0.25),
            },
            Instruction::Wrax { reg: target, c: 0 },
            Instruction::Rdax {
                reg: target,
                c: coeff::s1_14(1.0),
            },
            OUT,
        ];
        let (xl, reference) = both(&program, 0);
        assert_eq!(xl, 0, "register {target:#x} reads 0 on the XL");
        assert_ne!(
            reference, 0,
            "register {target:#x} reads back in the reference model"
        );
    }
}

#[test]
fn pots_lag_like_the_module() {
    let program = [Instruction::ldax(reg::POT0), OUT];
    let mut fv1 = load(Fv1::fx_aid_xl(), &program);
    fv1.set_pot(0, 0.7);
    assert_eq!(
        fv1.process_raw(0, 0).0,
        coeff::s_23(0.7),
        "no glide from the first setting"
    );

    let mut fv1 = load(Fv1::fx_aid_xl(), &program);
    fv1.set_pot(0, 0.0);
    fv1.process_raw(0, 0);
    fv1.set_pot(0, 1.0);
    let tau = (1.0 / XL_POT_SMOOTHING).round() as usize;
    let mut last = 0;
    for _ in 0..tau {
        last = fv1.process_raw(0, 0).0;
    }
    let reached = f64::from(last) / f64::from(ONE_S23);
    assert!(
        (0.6..0.66).contains(&reached),
        "one time constant reaches 1 - 1/e, got {reached}"
    );

    let mut reference = load(Fv1::new(), &program);
    reference.set_pot(0, 0.0);
    reference.process_raw(0, 0);
    reference.set_pot(0, 0.5);
    assert_eq!(
        reference.process_raw(0, 0).0,
        coeff::s_23(0.5),
        "the reference follows at once"
    );
}
