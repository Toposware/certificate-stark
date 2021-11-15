// Copyright (c) ToposWare and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

use super::constants::*;
use super::{ecc, field, rescue};
use bitvec::{order::Lsb0, slice::BitSlice, view::AsBits};
use core::cmp::Ordering;
use winterfell::{
    math::{curves::curve_f63::Scalar, fields::f63::BaseElement, FieldElement},
    ExecutionTrace,
};

#[cfg(feature = "concurrent")]
use winterfell::iterators::*;

// TRACE GENERATOR
// ================================================================================================

pub fn build_trace(
    messages: &[[BaseElement; AFFINE_POINT_WIDTH * 2 + 4]],
    signatures: &[([BaseElement; POINT_COORDINATE_WIDTH], Scalar)],
) -> ExecutionTrace<BaseElement> {
    // allocate memory to hold the trace table
    let trace_length = SIG_CYCLE_LENGTH * messages.len();
    let mut trace = ExecutionTrace::new(TRACE_WIDTH, trace_length);

    trace.fragments(SIG_CYCLE_LENGTH).for_each(|mut sig_trace| {
        let i = sig_trace.index();
        let (pkey_point, s_bytes, h_bytes) = build_sig_info(&messages[i], &signatures[i]);
        let s_bits = s_bytes.as_bits::<Lsb0>();
        let h_bits = h_bytes.as_bits::<Lsb0>();

        sig_trace.fill(
            |state| {
                init_sig_verification_state(signatures[i], state);
            },
            |step, state| {
                update_sig_verification_state(step, messages[i], pkey_point, s_bits, h_bits, state);
            },
        );
    });

    trace
}

// TRACE INITIALIZATION
// ================================================================================================

pub fn init_sig_verification_state(
    signature: ([BaseElement; POINT_COORDINATE_WIDTH], Scalar),
    state: &mut [BaseElement],
) {
    // initialize first state of the computation
    state[0..TRACE_WIDTH].copy_from_slice(&[BaseElement::ZERO; TRACE_WIDTH]);
    state[POINT_COORDINATE_WIDTH] = BaseElement::ONE; // y(S)

    state[PROJECTIVE_POINT_WIDTH + POINT_COORDINATE_WIDTH + 1] = BaseElement::ONE; // y(h.P)

    state[PROJECTIVE_POINT_WIDTH * 2 + 3..PROJECTIVE_POINT_WIDTH * 2 + POINT_COORDINATE_WIDTH + 3]
        .copy_from_slice(&signature.0[..]); // x(R)
}

// TRANSITION FUNCTION
// ================================================================================================

pub fn update_sig_verification_state(
    step: usize,
    message: [BaseElement; AFFINE_POINT_WIDTH * 2 + 4],
    pkey_point: [BaseElement; PROJECTIVE_POINT_WIDTH],
    s_bits: &BitSlice<Lsb0, u8>,
    h_bits: &BitSlice<Lsb0, u8>,
    state: &mut [BaseElement],
) {
    let bit_length = SCALAR_MUL_LENGTH / 2;
    let rescue_flag = step < TOTAL_HASH_LENGTH;
    let rescue_step = step % HASH_CYCLE_LENGTH;

    // enforcing the three kind of rescue operations
    if rescue_flag && (rescue_step < NUM_HASH_ROUNDS) {
        // for the first NUM_HASH_ROUNDS steps in every cycle, compute a single round of Rescue hash
        rescue::apply_round(&mut state[PROJECTIVE_POINT_WIDTH * 2 + 3..], step);
    } else if rescue_flag && (step < (NUM_HASH_ITER - 1) * HASH_CYCLE_LENGTH) {
        // for the next step, insert message chunks in the state registers
        let index = step / HASH_CYCLE_LENGTH;
        for i in 0..rescue::RATE_WIDTH {
            state[PROJECTIVE_POINT_WIDTH * 2 + rescue::RATE_WIDTH + 3 + i] =
                message[rescue::RATE_WIDTH * index + i];
        }
    } else if rescue_flag {
        // Register cells are by default copied from the previous state if no operation
        // is specified. This would conflict for here, as the "periodic" values for the
        // enforce_hash_copy() internal inputs are set to 0 at almost every step.
        // Hence we manually set them to zero for the final hash iteration, and this will
        // carry over until the end of the trace
        for i in 0..rescue::RATE_WIDTH {
            state[PROJECTIVE_POINT_WIDTH * 2 + rescue::RATE_WIDTH + 3 + i] = BaseElement::ZERO;
        }
    }

    // enforcing scalar multiplications
    match step.cmp(&SCALAR_MUL_LENGTH) {
        Ordering::Less => {
            let real_step = step / 2;
            let is_doubling_step = step % 2 == 0;
            state[PROJECTIVE_POINT_WIDTH] =
                BaseElement::from(s_bits[bit_length - 1 - real_step] as u8);
            state[2 * PROJECTIVE_POINT_WIDTH + 1] =
                BaseElement::from(h_bits[bit_length - 1 - real_step] as u8);

            if is_doubling_step {
                ecc::apply_point_doubling(&mut state[0..PROJECTIVE_POINT_WIDTH + 1]);
                ecc::apply_point_doubling(
                    &mut state[PROJECTIVE_POINT_WIDTH + 1..2 * PROJECTIVE_POINT_WIDTH + 2],
                );
                field::apply_double_and_add_step(
                    &mut state[2 * PROJECTIVE_POINT_WIDTH + 1..2 * PROJECTIVE_POINT_WIDTH + 3],
                    1,
                    0,
                );
            } else {
                ecc::apply_point_addition(&mut state[0..PROJECTIVE_POINT_WIDTH + 1], &GENERATOR);
                ecc::apply_point_addition(
                    &mut state[PROJECTIVE_POINT_WIDTH + 1..2 * PROJECTIVE_POINT_WIDTH + 2],
                    &pkey_point,
                );
            }
        }
        Ordering::Equal => {
            let mut hp_point = [BaseElement::ZERO; PROJECTIVE_POINT_WIDTH];
            hp_point.copy_from_slice(
                &state[PROJECTIVE_POINT_WIDTH + 1..PROJECTIVE_POINT_WIDTH * 2 + 1],
            );
            state[PROJECTIVE_POINT_WIDTH] = BaseElement::ONE;
            ecc::apply_point_addition(&mut state[..PROJECTIVE_POINT_WIDTH + 1], &hp_point);
            // Affine coordinates, hence do X/Z
            let mut x = [BaseElement::ZERO; POINT_COORDINATE_WIDTH];
            x.copy_from_slice(&state[0..POINT_COORDINATE_WIDTH]);
            let mut z = [BaseElement::ZERO; POINT_COORDINATE_WIDTH];
            z.copy_from_slice(&state[AFFINE_POINT_WIDTH..PROJECTIVE_POINT_WIDTH]);
            state[0..POINT_COORDINATE_WIDTH]
                .copy_from_slice(&ecc::mul_fp6(&x, &ecc::invert_fp6(&z)));
        }
        _ => {}
    }
}

// HELPER FUNCTIONS
// ================================================================================================

pub fn build_sig_info(
    message: &[BaseElement; AFFINE_POINT_WIDTH * 2 + 4],
    signature: &([BaseElement; POINT_COORDINATE_WIDTH], Scalar),
) -> ([BaseElement; PROJECTIVE_POINT_WIDTH], [u8; 32], [u8; 32]) {
    let mut pkey_point = [BaseElement::ZERO; PROJECTIVE_POINT_WIDTH];
    pkey_point[..AFFINE_POINT_WIDTH].clone_from_slice(&message[..AFFINE_POINT_WIDTH]);
    pkey_point[AFFINE_POINT_WIDTH] = BaseElement::ONE;
    let s_bytes = signature.1.to_bytes();

    let h = super::hash_message(signature.0, *message);
    // TODO: getting only one 64-bit word to not have wrong field arithmetic,
    // but should take 4 at least.
    let mut h_bytes = [0u8; 32];
    h_bytes[0..8].copy_from_slice(&h[0].to_bytes());

    (pkey_point, s_bytes, h_bytes)
}
