//! spinfv1 for the browser: a C-ABI WebAssembly module with no imports.
//!
//! One `Engine` wraps an FV-1 modelled as in the FX AID XL (`Fv1::fx_aid_xl`) behind the
//! host-rate resampler, so a page runs effects at the XL's measured chip rate (32,774 Hz)
//! inside any AudioContext rate.
//! JS writes SpinASM source into `fv1_scratch`, calls `fv1_assemble`, then
//! streams audio through the planar `fv1_in_ptr` / `fv1_out_ptr` buffers
//! (left at [0, BLOCK), right at [BLOCK, 2 * BLOCK)) with `fv1_process`.
//!
//! Any call that may allocate (`fv1_scratch`, `fv1_assemble`) can grow wasm
//! memory, so JS must refresh its typed-array views afterwards.

use spinfv1::resampler::{HostedFv1, XL_CHIP_RATE};
use spinfv1::{Fv1, assemble};

/// Largest number of frames per `fv1_process` call.
pub const BLOCK: usize = 2048;

pub struct Engine {
    hosted: HostedFv1,
    host_rate: f64,
    pots: [f32; 3],
    input: Box<[f32; 2 * BLOCK]>,
    output: Box<[f32; 2 * BLOCK]>,
    scratch: Vec<u8>,
    error: Vec<u8>,
}

#[unsafe(no_mangle)]
pub extern "C" fn fv1_block() -> usize {
    BLOCK
}

/// Create an engine for a host running at `host_rate` Hz.
#[unsafe(no_mangle)]
pub extern "C" fn fv1_new(host_rate: f64) -> *mut Engine {
    Box::into_raw(Box::new(Engine {
        hosted: HostedFv1::with_chip_rate(Fv1::fx_aid_xl(), host_rate, XL_CHIP_RATE),
        host_rate,
        pots: [0.5; 3],
        input: Box::new([0.0; 2 * BLOCK]),
        output: Box::new([0.0; 2 * BLOCK]),
        scratch: Vec::new(),
        error: Vec::new(),
    }))
}

/// Resize the scratch buffer to `len` bytes and return where JS writes them.
///
/// # Safety
/// `e` must come from `fv1_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fv1_scratch(e: *mut Engine, len: usize) -> *mut u8 {
    let e = unsafe { &mut *e };
    e.scratch.clear();
    e.scratch.resize(len, 0);
    e.scratch.as_mut_ptr()
}

/// Assemble the scratch buffer as SpinASM and load it into a fresh chip
/// (current pots kept) behind freshly primed resamplers. That is the same
/// start-up order as the native `fxaid-sim`, so free-running LFOs and chaos
/// maps begin in step with the offline renders.
/// Returns 0 on success, otherwise the length of the error text at
/// `fv1_error_ptr`.
///
/// # Safety
/// `e` must come from `fv1_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fv1_assemble(e: *mut Engine) -> usize {
    let e = unsafe { &mut *e };
    let result = std::str::from_utf8(&e.scratch)
        .map_err(|err| err.to_string())
        .and_then(|src| assemble(src).map_err(|err| err.to_string()));
    match result {
        Ok(program) => {
            let mut chip = Fv1::fx_aid_xl();
            chip.load_program(&program);
            for (i, &p) in e.pots.iter().enumerate() {
                chip.set_pot(i, p);
            }
            e.hosted = HostedFv1::with_chip_rate(chip, e.host_rate, XL_CHIP_RATE);
            0
        }
        Err(msg) => {
            e.error = msg.into_bytes();
            e.error.len()
        }
    }
}

/// # Safety
/// `e` must come from `fv1_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fv1_error_ptr(e: *mut Engine) -> *const u8 {
    unsafe { &*e }.error.as_ptr()
}

/// # Safety
/// `e` must come from `fv1_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fv1_set_pot(e: *mut Engine, index: u32, value: f32) {
    let e = unsafe { &mut *e };
    if index < 3 {
        let v = value.clamp(0.0, 1.0);
        e.pots[index as usize] = v;
        e.hosted.chip_mut().set_pot(index as usize, v);
    }
}

/// # Safety
/// `e` must come from `fv1_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fv1_in_ptr(e: *mut Engine) -> *mut f32 {
    unsafe { &mut *e }.input.as_mut_ptr()
}

/// # Safety
/// `e` must come from `fv1_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fv1_out_ptr(e: *mut Engine) -> *mut f32 {
    unsafe { &mut *e }.output.as_mut_ptr()
}

/// Process `frames` (at most `BLOCK`) planar frames from input to output.
///
/// # Safety
/// `e` must come from `fv1_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fv1_process(e: *mut Engine, frames: usize) {
    let e = unsafe { &mut *e };
    let frames = frames.min(BLOCK);
    for i in 0..frames {
        let (l, r) = e.hosted.process(e.input[i], e.input[BLOCK + i]);
        e.output[i] = l;
        e.output[BLOCK + i] = r;
    }
}
