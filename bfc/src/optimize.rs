use bfformat::Op;
use std::collections::BTreeMap;

const INITIAL_TAPE_SIZE: i128 = 30_000;
const TAPE_LIMIT: i128 = 1 << 30;

/// Runs optimization passes to a fixed point and resolves jumps after each
/// structural rewrite.
pub fn optimize(ops: Vec<Op>) -> Vec<Op> {
    let mut current = ops;
    loop {
        let previous = current.clone();
        current = fold_runs(&current);
        current = fold_zero_loops(current);
        current = fold_scan_loops(current);
        current = fold_generalized_linear_loops(current);
        current = resolve_jumps(current);
        current = window_pointer_ops(&current);
        current = fold_constant_stores(current);
        current = eliminate_dead_code(current);
        current = resolve_jumps(current);
        if current == previous {
            return current;
        }
    }
}

fn fold_runs(ops: &[Op]) -> Vec<Op> {
    let mut out = Vec::with_capacity(ops.len());
    let mut i = 0;
    while i < ops.len() {
        match ops[i] {
            Op::Add(_) => {
                let mut total = 0u64;
                while let Some(Op::Add(n)) = ops.get(i).copied() {
                    total += n as u64;
                    i += 1;
                }
                push_add_sub(&mut out, total, true);
            }
            Op::Sub(_) => {
                let mut total = 0u64;
                while let Some(Op::Sub(n)) = ops.get(i).copied() {
                    total += n as u64;
                    i += 1;
                }
                push_add_sub(&mut out, total, false);
            }
            Op::MoveRight(_) => {
                let mut total = 0u64;
                while let Some(Op::MoveRight(n)) = ops.get(i).copied() {
                    total += n as u64;
                    i += 1;
                }
                push_move(&mut out, total, true);
            }
            Op::MoveLeft(_) => {
                let mut total = 0u64;
                while let Some(Op::MoveLeft(n)) = ops.get(i).copied() {
                    total += n as u64;
                    i += 1;
                }
                push_move(&mut out, total, false);
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    out
}

fn push_add_sub(out: &mut Vec<Op>, total: u64, is_add: bool) {
    let mut remaining = total;
    while remaining > 0 {
        let chunk = remaining.min(u8::MAX as u64) as u8;
        out.push(if is_add {
            Op::Add(chunk)
        } else {
            Op::Sub(chunk)
        });
        remaining -= chunk as u64;
    }
}

fn push_move(out: &mut Vec<Op>, total: u64, is_right: bool) {
    let mut remaining = total;
    while remaining > 0 {
        let chunk = remaining.min(u32::MAX as u64) as u32;
        out.push(if is_right {
            Op::MoveRight(chunk)
        } else {
            Op::MoveLeft(chunk)
        });
        remaining -= chunk as u64;
    }
}

fn fold_zero_loops(ops: Vec<Op>) -> Vec<Op> {
    let mut out = Vec::with_capacity(ops.len());
    let mut i = 0;
    while i < ops.len() {
        let has_odd_step = match ops.get(i + 1) {
            Some(Op::Add(n) | Op::Sub(n)) => n & 1 == 1,
            _ => false,
        };
        if matches!(ops.get(i), Some(Op::JumpIfZero { .. }))
            && has_odd_step
            && matches!(ops.get(i + 2), Some(Op::JumpIfNonZero { .. }))
        {
            out.push(Op::Zero);
            i += 3;
        } else {
            out.push(ops[i]);
            i += 1;
        }
    }
    out
}

fn fold_scan_loops(ops: Vec<Op>) -> Vec<Op> {
    let mut out = Vec::with_capacity(ops.len());
    let mut i = 0;
    while i < ops.len() {
        let stride = match (ops.get(i), ops.get(i + 1), ops.get(i + 2)) {
            (
                Some(Op::JumpIfZero { .. }),
                Some(Op::MoveRight(n)),
                Some(Op::JumpIfNonZero { .. }),
            ) if *n <= i32::MAX as u32 => Some(*n as i32),
            (
                Some(Op::JumpIfZero { .. }),
                Some(Op::MoveLeft(n)),
                Some(Op::JumpIfNonZero { .. }),
            ) if *n <= i32::MAX as u32 => Some(-(*n as i32)),
            _ => None,
        };
        if let Some(stride) = stride {
            out.push(Op::Scan { stride });
            i += 3;
        } else {
            out.push(ops[i]);
            i += 1;
        }
    }
    out
}

/// Converts terminating linear loops into wrapping multiply-adds. The
/// counter step must be odd modulo 256; target updates may add or subtract.
fn fold_generalized_linear_loops(ops: Vec<Op>) -> Vec<Op> {
    let mut out = Vec::with_capacity(ops.len());
    let mut i = 0;
    while i < ops.len() {
        if let Some((end, deltas)) = find_linear_loop(&ops, i) {
            for (offset, factor) in deltas {
                out.push(Op::MulAdd { offset, factor });
            }
            out.push(Op::ZeroAt { offset: 0 });
            i = end;
        } else {
            out.push(ops[i]);
            i += 1;
        }
    }
    out
}

fn find_linear_loop(ops: &[Op], start: usize) -> Option<(usize, Vec<(isize, u8)>)> {
    if !matches!(ops.get(start), Some(Op::JumpIfZero { .. })) {
        return None;
    }

    let mut pointer = 0i64;
    let mut cells: BTreeMap<i64, (u8, bool)> = BTreeMap::new();
    let mut i = start + 1;
    loop {
        match ops.get(i)? {
            Op::Add(n) => {
                let cell = cells.entry(pointer).or_insert((0, false));
                cell.0 = cell.0.wrapping_add(*n);
                cell.1 = true;
                i += 1;
            }
            Op::Sub(n) => {
                let cell = cells.entry(pointer).or_insert((0, false));
                cell.0 = cell.0.wrapping_sub(*n);
                cell.1 = true;
                i += 1;
            }
            Op::MoveRight(n) => {
                pointer += *n as i64;
                cells.entry(pointer).or_insert((0, false));
                i += 1;
            }
            Op::MoveLeft(n) => {
                pointer -= *n as i64;
                cells.entry(pointer).or_insert((0, false));
                i += 1;
            }
            Op::JumpIfNonZero { .. } => break,
            _ => return None,
        }
    }

    if pointer != 0 {
        return None;
    }
    let step = cells.get(&0)?.0;
    if step & 1 == 0 || cells.values().any(|(_, written)| !written) {
        return None;
    }
    let inverse = inverse_mod_256(step);
    let deltas = cells
        .into_iter()
        .filter_map(|(offset, (delta, _))| {
            (offset != 0).then_some((offset as isize, delta.wrapping_neg().wrapping_mul(inverse)))
        })
        .collect();
    Some((i + 1, deltas))
}

fn inverse_mod_256(value: u8) -> u8 {
    let mut inverse = 1u8;
    inverse = inverse.wrapping_mul(2u8.wrapping_sub(value.wrapping_mul(inverse)));
    inverse = inverse.wrapping_mul(2u8.wrapping_sub(value.wrapping_mul(inverse)));
    inverse = inverse.wrapping_mul(2u8.wrapping_sub(value.wrapping_mul(inverse)));
    inverse = inverse.wrapping_mul(2u8.wrapping_sub(value.wrapping_mul(inverse)));
    inverse
}

fn is_window_op(op: Op) -> bool {
    matches!(
        op,
        Op::Add(_) | Op::Sub(_) | Op::MoveRight(_) | Op::MoveLeft(_) | Op::Zero
    )
}

/// Replaces straight-line pointer movement with fixed-offset accesses and a
/// single final pointer move. BoundsCheck preserves each intermediate move's
/// underflow and growth behavior before any stores in the window execute.
fn window_pointer_ops(ops: &[Op]) -> Vec<Op> {
    let mut out = Vec::with_capacity(ops.len());
    let mut i = 0;
    while i < ops.len() {
        if !is_window_op(ops[i]) {
            out.push(ops[i]);
            i += 1;
            continue;
        }

        let start = i;
        while i < ops.len() && is_window_op(ops[i]) {
            i += 1;
        }
        let segment = &ops[start..i];
        let Some(transformed) = window_segment(segment) else {
            out.extend_from_slice(segment);
            continue;
        };
        out.extend(transformed);
    }
    out
}

fn window_segment(segment: &[Op]) -> Option<Vec<Op>> {
    let mut pointer = 0i128;
    let mut min_offset = 0i128;
    let mut max_offset = 0i128;
    let mut moved = false;
    let mut stores = Vec::with_capacity(segment.len());
    for op in segment {
        match *op {
            Op::MoveRight(n) => {
                pointer += n as i128;
                max_offset = max_offset.max(pointer);
                moved |= n != 0;
            }
            Op::MoveLeft(n) => {
                pointer -= n as i128;
                min_offset = min_offset.min(pointer);
                moved |= n != 0;
            }
            Op::Add(delta) => stores.push(Op::AddAt {
                offset: pointer as isize,
                delta,
            }),
            Op::Sub(delta) => stores.push(Op::AddAt {
                offset: pointer as isize,
                delta: delta.wrapping_neg(),
            }),
            Op::Zero => stores.push(Op::ZeroAt {
                offset: pointer as isize,
            }),
            _ => return None,
        }
    }
    if stores.is_empty() {
        return None;
    }
    if !window_fits(pointer, min_offset, max_offset) {
        return None;
    }
    let mut out = Vec::with_capacity(stores.len() + 2);
    if moved {
        out.push(Op::BoundsCheck {
            min_offset: min_offset as isize,
            max_offset: max_offset as isize,
        });
    }
    out.extend(stores);
    if pointer > 0 {
        out.push(Op::MoveRight(pointer as u32));
    } else if pointer < 0 {
        out.push(Op::MoveLeft(pointer.unsigned_abs() as u32));
    }
    Some(out)
}

fn window_fits(pointer: i128, min_offset: i128, max_offset: i128) -> bool {
    pointer.unsigned_abs() <= u32::MAX as u128
        && min_offset >= isize::MIN as i128
        && max_offset <= isize::MAX as i128
}

fn fold_constant_stores(ops: Vec<Op>) -> Vec<Op> {
    let mut out: Vec<Op> = Vec::with_capacity(ops.len());
    for op in ops {
        let mut next = op;
        while let Some(previous) = out.last().copied() {
            let Some(combined) = combine_stores(previous, next) else {
                break;
            };
            out.pop();
            next = combined;
        }
        out.push(next);
    }
    out
}

fn combine_stores(previous: Op, next: Op) -> Option<Op> {
    match (previous, next) {
        (Op::ZeroAt { offset: a }, Op::AddAt { offset: b, delta }) if a == b => Some(Op::SetAt {
            offset: a,
            value: delta,
        }),
        (Op::ZeroAt { offset: a }, Op::ZeroAt { offset: b }) if a == b => {
            Some(Op::ZeroAt { offset: a })
        }
        (Op::ZeroAt { offset: a }, Op::SetAt { offset: b, value }) if a == b => {
            Some(Op::SetAt { offset: a, value })
        }
        (
            Op::AddAt {
                offset: a,
                delta: x,
            },
            Op::AddAt {
                offset: b,
                delta: y,
            },
        ) if a == b => Some(Op::AddAt {
            offset: a,
            delta: x.wrapping_add(y),
        }),
        (Op::AddAt { offset: a, .. }, Op::ZeroAt { offset: b }) if a == b => {
            Some(Op::ZeroAt { offset: a })
        }
        (Op::AddAt { offset: a, .. }, Op::SetAt { offset: b, value }) if a == b => {
            Some(Op::SetAt { offset: a, value })
        }
        (
            Op::SetAt {
                offset: a,
                value: x,
            },
            Op::AddAt { offset: b, delta },
        ) if a == b => Some(Op::SetAt {
            offset: a,
            value: x.wrapping_add(delta),
        }),
        (Op::SetAt { offset: a, .. }, Op::ZeroAt { offset: b }) if a == b => {
            Some(Op::ZeroAt { offset: a })
        }
        (Op::SetAt { offset: a, .. }, Op::SetAt { offset: b, value }) if a == b => {
            Some(Op::SetAt { offset: a, value })
        }
        _ => None,
    }
}

#[derive(Clone)]
struct KnownCells {
    default: Option<u8>,
    cells: BTreeMap<i128, Option<u8>>,
}

impl KnownCells {
    fn initially_zero() -> Self {
        Self {
            default: Some(0),
            cells: BTreeMap::new(),
        }
    }

    fn unknown() -> Self {
        Self {
            default: None,
            cells: BTreeMap::new(),
        }
    }

    fn get(&self, address: i128) -> Option<u8> {
        if address < 0 {
            None
        } else {
            self.cells.get(&address).copied().unwrap_or(self.default)
        }
    }

    fn set(&mut self, address: i128, value: Option<u8>) {
        if address >= 0 {
            self.cells.insert(address, value);
        }
    }

    fn reset_at_zero(&mut self) {
        *self = Self::unknown();
        self.set(0, Some(0));
    }
}

/// Forward known-value analysis. Control-flow joins and dynamic scans are
/// rebased conservatively; only provably skipped zero-cell loops are deleted.
fn eliminate_dead_code(ops: Vec<Op>) -> Vec<Op> {
    let mut out = Vec::with_capacity(ops.len());
    let mut known = KnownCells::initially_zero();
    let mut pointer = 0i128;
    let mut tape_len = INITIAL_TAPE_SIZE;
    let mut i = 0;
    while i < ops.len() {
        match ops[i] {
            Op::Add(delta) => {
                let old = known.get(pointer);
                let value = old.map(|v| v.wrapping_add(delta));
                if delta != 0 || old.is_none() {
                    out.push(ops[i]);
                }
                known.set(pointer, value);
                i += 1;
            }
            Op::Sub(delta) => {
                let old = known.get(pointer);
                let value = old.map(|v| v.wrapping_sub(delta));
                if delta != 0 || old.is_none() {
                    out.push(ops[i]);
                }
                known.set(pointer, value);
                i += 1;
            }
            Op::Zero => {
                if known.get(pointer) != Some(0) {
                    out.push(Op::Zero);
                }
                known.set(pointer, Some(0));
                i += 1;
            }
            Op::MoveRight(n) => {
                pointer += n as i128;
                out.push(ops[i]);
                if pointer >= TAPE_LIMIT {
                    out.extend_from_slice(&ops[i + 1..]);
                    break;
                }
                tape_len = tape_len.max(pointer + 1);
                i += 1;
            }
            Op::MoveLeft(n) => {
                pointer -= n as i128;
                out.push(ops[i]);
                if pointer < 0 {
                    out.extend_from_slice(&ops[i + 1..]);
                    break;
                }
                i += 1;
            }
            Op::Output => {
                out.push(Op::Output);
                i += 1;
            }
            Op::Input => {
                out.push(Op::Input);
                known.set(pointer, None);
                i += 1;
            }
            Op::JumpIfZero { target } => {
                let close = target as usize;
                if close <= i || close >= ops.len() {
                    out.push(ops[i]);
                    known = KnownCells::unknown();
                    i += 1;
                } else if known.get(pointer) == Some(0) {
                    i = close + 1;
                } else {
                    out.extend_from_slice(&ops[i..=close]);
                    pointer = 0;
                    tape_len = 1;
                    known.reset_at_zero();
                    i = close + 1;
                }
            }
            Op::JumpIfNonZero { .. } => {
                out.push(ops[i]);
                known = KnownCells::unknown();
                i += 1;
            }
            Op::Scan { .. } => {
                if known.get(pointer) != Some(0) {
                    out.push(ops[i]);
                    pointer = 0;
                    tape_len = 1;
                    known.reset_at_zero();
                }
                i += 1;
            }
            Op::BoundsCheck {
                min_offset,
                max_offset,
            } => {
                let min = pointer + min_offset as i128;
                let max = pointer + max_offset as i128;
                if !(0..TAPE_LIMIT).contains(&min) || max >= TAPE_LIMIT {
                    out.push(ops[i]);
                    out.extend_from_slice(&ops[i + 1..]);
                    break;
                }
                if max >= tape_len {
                    out.push(ops[i]);
                    tape_len = tape_len.max(max + 1);
                }
                i += 1;
            }
            Op::AddAt { offset, delta } => {
                let address = pointer + offset as i128;
                if !(0..TAPE_LIMIT).contains(&address) {
                    out.push(ops[i]);
                    out.extend_from_slice(&ops[i + 1..]);
                    break;
                }
                let was_allocated = address < tape_len;
                let old = known.get(address);
                let value = old.map(|v| v.wrapping_add(delta));
                if !(delta == 0 && was_allocated) {
                    if old == Some(0) {
                        out.push(Op::SetAt {
                            offset,
                            value: delta,
                        });
                    } else {
                        out.push(ops[i]);
                    }
                }
                tape_len = tape_len.max(address + 1);
                known.set(address, value);
                i += 1;
            }
            Op::ZeroAt { offset } => {
                let address = pointer + offset as i128;
                if !(0..TAPE_LIMIT).contains(&address) {
                    out.push(ops[i]);
                    out.extend_from_slice(&ops[i + 1..]);
                    break;
                }
                if known.get(address) != Some(0) || address >= tape_len {
                    out.push(ops[i]);
                }
                tape_len = tape_len.max(address + 1);
                known.set(address, Some(0));
                i += 1;
            }
            Op::SetAt { offset, value } => {
                let address = pointer + offset as i128;
                if !(0..TAPE_LIMIT).contains(&address) {
                    out.push(ops[i]);
                    out.extend_from_slice(&ops[i + 1..]);
                    break;
                }
                if known.get(address) != Some(value) || address >= tape_len {
                    out.push(ops[i]);
                }
                tape_len = tape_len.max(address + 1);
                known.set(address, Some(value));
                i += 1;
            }
            Op::MulAdd { offset, factor } => {
                let source = known.get(pointer);
                if source == Some(0) {
                    i += 1;
                    continue;
                }
                let address = pointer + offset as i128;
                if !(0..TAPE_LIMIT).contains(&address) {
                    out.push(ops[i]);
                    out.extend_from_slice(&ops[i + 1..]);
                    break;
                }
                if let Some(source) = source {
                    let delta = source.wrapping_mul(factor);
                    let was_allocated = address < tape_len;
                    let old = known.get(address);
                    let value = old.map(|v| v.wrapping_add(delta));
                    if !(delta == 0 && was_allocated) {
                        if old == Some(0) {
                            out.push(Op::SetAt {
                                offset,
                                value: delta,
                            });
                        } else {
                            out.push(Op::AddAt { offset, delta });
                        }
                    }
                    tape_len = tape_len.max(address + 1);
                    known.set(address, value);
                } else {
                    out.push(ops[i]);
                    known.set(address, None);
                }
                i += 1;
            }
        }
    }
    out
}

fn resolve_jumps(ops: Vec<Op>) -> Vec<Op> {
    let mut resolved = ops;
    let mut open_brackets: Vec<usize> = Vec::new();
    for i in 0..resolved.len() {
        match resolved[i] {
            Op::JumpIfZero { .. } => open_brackets.push(i),
            Op::JumpIfNonZero { .. } => {
                let open_index = open_brackets
                    .pop()
                    .expect("unbalanced brackets survived parsing");
                resolved[open_index] = Op::JumpIfZero { target: i as u32 };
                resolved[i] = Op::JumpIfNonZero {
                    target: open_index as u32,
                };
            }
            _ => {}
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bracketed(body: &[Op]) -> Vec<Op> {
        let mut ops = vec![Op::JumpIfZero { target: 0 }];
        ops.extend_from_slice(body);
        ops.push(Op::JumpIfNonZero { target: 0 });
        resolve_jumps(ops)
    }

    #[test]
    fn run_folding_accumulates_counts_and_splits_large_runs() {
        assert_eq!(
            fold_runs(&[
                Op::Add(4),
                Op::Add(3),
                Op::Sub(2),
                Op::MoveRight(4),
                Op::MoveRight(5)
            ]),
            vec![Op::Add(7), Op::Sub(2), Op::MoveRight(9)]
        );
        assert_eq!(
            fold_runs(&[Op::Add(255), Op::Add(1)]),
            vec![Op::Add(255), Op::Add(1)]
        );
        assert!(fold_runs(&[Op::MoveLeft(0)]).is_empty());
        assert_eq!(fold_runs(&[Op::MoveLeft(2)]), vec![Op::MoveLeft(2)]);
    }

    #[test]
    fn zero_loop_folding_requires_an_odd_step() {
        assert_eq!(fold_zero_loops(bracketed(&[Op::Sub(3)])), vec![Op::Zero]);
        assert_eq!(
            fold_zero_loops(bracketed(&[Op::Add(2)])),
            bracketed(&[Op::Add(2)])
        );
        assert_eq!(
            fold_zero_loops(vec![Op::JumpIfZero { target: 0 }]),
            vec![Op::JumpIfZero { target: 0 }]
        );
    }

    #[test]
    fn generalized_linear_loops_fold_addition_and_subtraction() {
        let add = fold_generalized_linear_loops(bracketed(&[
            Op::Sub(1),
            Op::MoveRight(1),
            Op::Add(1),
            Op::MoveLeft(1),
        ]));
        assert_eq!(
            add,
            vec![
                Op::MulAdd {
                    offset: 1,
                    factor: 1
                },
                Op::ZeroAt { offset: 0 }
            ]
        );

        let subtract = fold_generalized_linear_loops(bracketed(&[
            Op::Sub(1),
            Op::MoveRight(1),
            Op::Sub(1),
            Op::MoveLeft(1),
        ]));
        assert_eq!(
            subtract,
            vec![
                Op::MulAdd {
                    offset: 1,
                    factor: 255
                },
                Op::ZeroAt { offset: 0 }
            ]
        );
    }

    #[test]
    fn generalized_linear_loops_support_odd_counter_steps() {
        let ops = fold_generalized_linear_loops(bracketed(&[
            Op::Sub(3),
            Op::MoveRight(3),
            Op::Add(1),
            Op::MoveLeft(3),
        ]));
        assert_eq!(
            ops,
            vec![
                Op::MulAdd {
                    offset: 3,
                    factor: 171
                },
                Op::ZeroAt { offset: 0 }
            ]
        );
    }

    #[test]
    fn even_steps_and_untouched_excursions_are_not_folded() {
        let even = bracketed(&[Op::Sub(2), Op::MoveRight(2), Op::Add(1), Op::MoveLeft(2)]);
        assert_eq!(fold_generalized_linear_loops(even.clone()), even);
        let excursion = bracketed(&[Op::MoveLeft(1), Op::MoveRight(1), Op::Sub(1)]);
        assert_eq!(fold_generalized_linear_loops(excursion.clone()), excursion);
    }

    #[test]
    fn other_non_linear_loop_shapes_are_kept() {
        let unbalanced = bracketed(&[Op::Sub(1), Op::MoveRight(1), Op::Add(1)]);
        assert_eq!(
            fold_generalized_linear_loops(unbalanced.clone()),
            unbalanced
        );
        let nested = bracketed(&[
            Op::Sub(1),
            Op::JumpIfZero { target: 0 },
            Op::JumpIfNonZero { target: 0 },
        ]);
        assert_eq!(fold_generalized_linear_loops(nested.clone()), nested);
        let unterminated = vec![Op::JumpIfZero { target: 0 }, Op::Add(1)];
        assert_eq!(
            fold_generalized_linear_loops(unterminated.clone()),
            unterminated
        );
    }

    #[test]
    fn modular_inverse_is_correct_for_every_odd_byte() {
        for value in (1u8..=u8::MAX).step_by(2) {
            assert_eq!(value.wrapping_mul(inverse_mod_256(value)), 1);
        }
    }

    #[test]
    fn scan_folding_handles_both_directions_and_rejects_large_strides() {
        assert_eq!(
            fold_scan_loops(bracketed(&[Op::MoveRight(3)])),
            vec![Op::Scan { stride: 3 }]
        );
        assert_eq!(
            fold_scan_loops(bracketed(&[Op::MoveLeft(3)])),
            vec![Op::Scan { stride: -3 }]
        );
        let too_far = bracketed(&[Op::MoveRight(i32::MAX as u32 + 1)]);
        assert_eq!(fold_scan_loops(too_far.clone()), too_far);
        let too_far_left = bracketed(&[Op::MoveLeft(i32::MAX as u32 + 1)]);
        assert_eq!(fold_scan_loops(too_far_left.clone()), too_far_left);
        let with_add = bracketed(&[Op::MoveRight(1), Op::Add(1)]);
        assert_eq!(fold_scan_loops(with_add.clone()), with_add);
    }

    #[test]
    fn windows_convert_arithmetic_and_preserve_min_max_bounds() {
        let fused = window_pointer_ops(&[
            Op::MoveLeft(2),
            Op::Add(4),
            Op::MoveRight(5),
            Op::Zero,
            Op::MoveLeft(3),
        ]);
        assert_eq!(
            fused,
            vec![
                Op::BoundsCheck {
                    min_offset: -2,
                    max_offset: 3
                },
                Op::AddAt {
                    offset: -2,
                    delta: 4
                },
                Op::ZeroAt { offset: 3 }
            ]
        );
    }

    #[test]
    fn window_keeps_oversized_final_displacements_unfused() {
        let ops = [Op::MoveRight(u32::MAX), Op::MoveRight(1)];
        assert_eq!(window_pointer_ops(&ops), ops);
        assert_eq!(
            window_pointer_ops(&[Op::MoveLeft(1), Op::MoveRight(1)]),
            vec![Op::MoveLeft(1), Op::MoveRight(1)]
        );
        assert_eq!(
            window_segment(&[Op::Add(1), Op::MoveRight(1)]),
            Some(vec![
                Op::BoundsCheck {
                    min_offset: 0,
                    max_offset: 1
                },
                Op::AddAt {
                    offset: 0,
                    delta: 1
                },
                Op::MoveRight(1)
            ])
        );
        assert_eq!(
            window_segment(&[Op::Sub(2)]),
            Some(vec![Op::AddAt {
                offset: 0,
                delta: 254
            }])
        );
        assert_eq!(
            window_segment(&[Op::Add(1), Op::MoveLeft(1)]),
            Some(vec![
                Op::BoundsCheck {
                    min_offset: -1,
                    max_offset: 0
                },
                Op::AddAt {
                    offset: 0,
                    delta: 1
                },
                Op::MoveLeft(1)
            ])
        );
        assert_eq!(window_segment(&[Op::MoveRight(1)]), None);
        assert_eq!(
            window_segment(&[Op::MoveRight(u32::MAX), Op::MoveRight(1), Op::Add(1)]),
            None
        );
        for barrier in [
            Op::Input,
            Op::Output,
            Op::Scan { stride: 1 },
            Op::MulAdd {
                offset: 1,
                factor: 2,
            },
            Op::JumpIfZero { target: 0 },
        ] {
            assert_eq!(window_segment(&[barrier]), None);
        }
        assert!(window_segment(&[Op::Add(1)]).is_some());
        assert!(window_fits(0, isize::MIN as i128, isize::MAX as i128));
        assert!(!window_fits(u32::MAX as i128 + 1, 0, 0));
        assert!(!window_fits(0, isize::MIN as i128 - 1, 0));
        assert!(!window_fits(0, 0, isize::MAX as i128 + 1));
    }

    #[test]
    fn store_fusion_coalesces_zero_add_and_same_cell_updates() {
        assert_eq!(
            fold_constant_stores(vec![
                Op::ZeroAt { offset: 2 },
                Op::AddAt {
                    offset: 2,
                    delta: 9
                },
                Op::AddAt {
                    offset: 2,
                    delta: 3
                },
                Op::SetAt {
                    offset: 2,
                    value: 4
                }
            ]),
            vec![Op::SetAt {
                offset: 2,
                value: 4
            }]
        );
        assert_eq!(
            fold_constant_stores(vec![
                Op::AddAt {
                    offset: 1,
                    delta: 255
                },
                Op::AddAt {
                    offset: 1,
                    delta: 2
                }
            ]),
            vec![Op::AddAt {
                offset: 1,
                delta: 1
            }]
        );
        assert_eq!(
            fold_constant_stores(vec![Op::ZeroAt { offset: 1 }, Op::ZeroAt { offset: 2 }]),
            vec![Op::ZeroAt { offset: 1 }, Op::ZeroAt { offset: 2 }]
        );
        let cases = [
            (
                Op::ZeroAt { offset: 1 },
                Op::ZeroAt { offset: 1 },
                Some(Op::ZeroAt { offset: 1 }),
            ),
            (
                Op::ZeroAt { offset: 1 },
                Op::SetAt {
                    offset: 1,
                    value: 9,
                },
                Some(Op::SetAt {
                    offset: 1,
                    value: 9,
                }),
            ),
            (
                Op::AddAt {
                    offset: 1,
                    delta: 2,
                },
                Op::ZeroAt { offset: 1 },
                Some(Op::ZeroAt { offset: 1 }),
            ),
            (
                Op::AddAt {
                    offset: 1,
                    delta: 2,
                },
                Op::SetAt {
                    offset: 1,
                    value: 9,
                },
                Some(Op::SetAt {
                    offset: 1,
                    value: 9,
                }),
            ),
            (
                Op::SetAt {
                    offset: 1,
                    value: 2,
                },
                Op::AddAt {
                    offset: 1,
                    delta: 3,
                },
                Some(Op::SetAt {
                    offset: 1,
                    value: 5,
                }),
            ),
            (
                Op::SetAt {
                    offset: 1,
                    value: 2,
                },
                Op::ZeroAt { offset: 1 },
                Some(Op::ZeroAt { offset: 1 }),
            ),
            (
                Op::SetAt {
                    offset: 1,
                    value: 2,
                },
                Op::SetAt {
                    offset: 1,
                    value: 3,
                },
                Some(Op::SetAt {
                    offset: 1,
                    value: 3,
                }),
            ),
            (
                Op::ZeroAt { offset: 1 },
                Op::SetAt {
                    offset: 2,
                    value: 3,
                },
                None,
            ),
            (
                Op::ZeroAt { offset: 1 },
                Op::AddAt {
                    offset: 2,
                    delta: 1,
                },
                None,
            ),
            (Op::ZeroAt { offset: 1 }, Op::ZeroAt { offset: 2 }, None),
            (
                Op::AddAt {
                    offset: 1,
                    delta: 1,
                },
                Op::AddAt {
                    offset: 2,
                    delta: 1,
                },
                None,
            ),
            (
                Op::AddAt {
                    offset: 1,
                    delta: 1,
                },
                Op::ZeroAt { offset: 2 },
                None,
            ),
            (
                Op::AddAt {
                    offset: 1,
                    delta: 1,
                },
                Op::SetAt {
                    offset: 2,
                    value: 3,
                },
                None,
            ),
            (
                Op::SetAt {
                    offset: 1,
                    value: 1,
                },
                Op::AddAt {
                    offset: 2,
                    delta: 1,
                },
                None,
            ),
            (
                Op::SetAt {
                    offset: 1,
                    value: 1,
                },
                Op::ZeroAt { offset: 2 },
                None,
            ),
            (
                Op::SetAt {
                    offset: 1,
                    value: 1,
                },
                Op::SetAt {
                    offset: 2,
                    value: 3,
                },
                None,
            ),
        ];
        for (previous, next, expected) in cases {
            assert_eq!(combine_stores(previous, next), expected);
        }
    }

    #[test]
    fn dce_removes_entry_zero_loops_and_redundant_stores() {
        assert!(optimize(bracketed(&[])).is_empty());
        assert!(optimize(bracketed(&[Op::Sub(1)])).is_empty());
        assert_eq!(
            eliminate_dead_code(vec![
                Op::AddAt {
                    offset: 1,
                    delta: 7
                },
                Op::AddAt {
                    offset: 1,
                    delta: 1
                }
            ]),
            vec![
                Op::SetAt {
                    offset: 1,
                    value: 7
                },
                Op::AddAt {
                    offset: 1,
                    delta: 1
                }
            ]
        );
        assert_eq!(
            eliminate_dead_code(vec![
                Op::SetAt {
                    offset: 1,
                    value: 3
                },
                Op::AddAt {
                    offset: 1,
                    delta: 2
                }
            ]),
            vec![
                Op::SetAt {
                    offset: 1,
                    value: 3
                },
                Op::AddAt {
                    offset: 1,
                    delta: 2
                }
            ]
        );
        assert_eq!(
            eliminate_dead_code(vec![
                Op::SetAt {
                    offset: 1,
                    value: 0
                },
                Op::AddAt {
                    offset: 1,
                    delta: 0
                }
            ]),
            vec![]
        );
    }

    #[test]
    fn dce_tracks_constants_inputs_muladds_and_scans() {
        assert_eq!(
            eliminate_dead_code(vec![
                Op::SetAt {
                    offset: 1,
                    value: 0
                },
                Op::AddAt {
                    offset: 1,
                    delta: 5
                },
                Op::MulAdd {
                    offset: 2,
                    factor: 9
                }
            ]),
            vec![Op::SetAt {
                offset: 1,
                value: 5
            }]
        );
        let dynamic = eliminate_dead_code(vec![
            Op::Input,
            Op::MulAdd {
                offset: 1,
                factor: 2,
            },
            Op::Scan { stride: 1 },
            Op::SetAt {
                offset: 0,
                value: 0,
            },
        ]);
        assert_eq!(
            dynamic,
            vec![
                Op::Input,
                Op::MulAdd {
                    offset: 1,
                    factor: 2
                },
                Op::Scan { stride: 1 }
            ]
        );
    }

    #[test]
    fn dce_covers_control_flow_and_bounds_decisions() {
        assert_eq!(
            eliminate_dead_code(vec![Op::Add(0), Op::Sub(0), Op::Zero]),
            Vec::<Op>::new()
        );
        assert_eq!(
            eliminate_dead_code(vec![Op::Input, Op::Add(0), Op::Sub(0), Op::Zero]),
            vec![Op::Input, Op::Add(0), Op::Sub(0), Op::Zero]
        );
        for move_op in [Op::MoveRight(TAPE_LIMIT as u32), Op::MoveLeft(1)] {
            let input = vec![move_op, Op::Output];
            assert_eq!(eliminate_dead_code(input.clone()), input);
        }
        for target in [0, 99] {
            let input = vec![Op::JumpIfZero { target }, Op::Add(1)];
            assert_eq!(eliminate_dead_code(input.clone()), input);
        }
        assert_eq!(
            eliminate_dead_code(vec![
                Op::JumpIfZero { target: 2 },
                Op::Add(1),
                Op::JumpIfNonZero { target: 0 },
                Op::Output
            ]),
            vec![Op::Output]
        );
        let unknown_loop = vec![
            Op::Input,
            Op::JumpIfZero { target: 3 },
            Op::Add(1),
            Op::JumpIfNonZero { target: 1 },
        ];
        assert_eq!(eliminate_dead_code(unknown_loop.clone()), unknown_loop);
        assert_eq!(
            eliminate_dead_code(vec![Op::Input, Op::JumpIfNonZero { target: 0 }]),
            vec![Op::Input, Op::JumpIfNonZero { target: 0 }]
        );
        assert_eq!(
            eliminate_dead_code(vec![Op::Scan { stride: 1 }]),
            Vec::<Op>::new()
        );
        assert_eq!(
            eliminate_dead_code(vec![Op::Input, Op::Scan { stride: 1 }]),
            vec![Op::Input, Op::Scan { stride: 1 }]
        );

        for bounds in [
            Op::BoundsCheck {
                min_offset: -1,
                max_offset: 0,
            },
            Op::BoundsCheck {
                min_offset: 0,
                max_offset: TAPE_LIMIT as isize,
            },
        ] {
            let input = vec![bounds, Op::Output];
            assert_eq!(eliminate_dead_code(input.clone()), input);
        }
        assert_eq!(
            eliminate_dead_code(vec![Op::BoundsCheck {
                min_offset: 0,
                max_offset: 0,
            }]),
            Vec::<Op>::new()
        );
        assert_eq!(
            eliminate_dead_code(vec![Op::BoundsCheck {
                min_offset: 0,
                max_offset: INITIAL_TAPE_SIZE as isize,
            }]),
            vec![Op::BoundsCheck {
                min_offset: 0,
                max_offset: INITIAL_TAPE_SIZE as isize,
            }]
        );

        for offset in [-1, TAPE_LIMIT as isize] {
            let input = vec![Op::AddAt { offset, delta: 1 }, Op::Output];
            assert_eq!(eliminate_dead_code(input.clone()), input);
            let input = vec![Op::ZeroAt { offset }, Op::Output];
            assert_eq!(eliminate_dead_code(input.clone()), input);
            let input = vec![Op::SetAt { offset, value: 1 }, Op::Output];
            assert_eq!(eliminate_dead_code(input.clone()), input);
        }
        assert_eq!(
            eliminate_dead_code(vec![Op::AddAt {
                offset: 1,
                delta: 0
            }]),
            Vec::<Op>::new()
        );
        assert_eq!(
            eliminate_dead_code(vec![Op::AddAt {
                offset: 1,
                delta: 4
            }]),
            vec![Op::SetAt {
                offset: 1,
                value: 4
            }]
        );
        assert_eq!(
            eliminate_dead_code(vec![
                Op::Input,
                Op::MoveRight(1),
                Op::Input,
                Op::MoveLeft(1),
                Op::AddAt {
                    offset: 1,
                    delta: 4
                }
            ]),
            vec![
                Op::Input,
                Op::MoveRight(1),
                Op::Input,
                Op::MoveLeft(1),
                Op::AddAt {
                    offset: 1,
                    delta: 4
                }
            ]
        );
        assert_eq!(
            eliminate_dead_code(vec![
                Op::Input,
                Op::MoveRight(1),
                Op::Input,
                Op::MoveLeft(1),
                Op::ZeroAt { offset: 1 },
                Op::SetAt {
                    offset: INITIAL_TAPE_SIZE as isize,
                    value: 0
                }
            ]),
            vec![
                Op::Input,
                Op::MoveRight(1),
                Op::Input,
                Op::MoveLeft(1),
                Op::ZeroAt { offset: 1 },
                Op::SetAt {
                    offset: INITIAL_TAPE_SIZE as isize,
                    value: 0
                }
            ]
        );

        assert_eq!(
            eliminate_dead_code(vec![
                Op::Add(1),
                Op::MulAdd {
                    offset: 1,
                    factor: 2
                }
            ]),
            vec![
                Op::Add(1),
                Op::SetAt {
                    offset: 1,
                    value: 2
                }
            ]
        );
        assert_eq!(
            eliminate_dead_code(vec![
                Op::Add(1),
                Op::MulAdd {
                    offset: 1,
                    factor: 0
                }
            ]),
            vec![Op::Add(1)]
        );
        assert_eq!(
            eliminate_dead_code(vec![
                Op::Input,
                Op::MulAdd {
                    offset: 1,
                    factor: 2
                }
            ]),
            vec![
                Op::Input,
                Op::MulAdd {
                    offset: 1,
                    factor: 2
                }
            ]
        );
        assert_eq!(
            eliminate_dead_code(vec![
                Op::SetAt {
                    offset: 1,
                    value: 7
                },
                Op::Add(1),
                Op::MulAdd {
                    offset: 1,
                    factor: 2
                }
            ]),
            vec![
                Op::SetAt {
                    offset: 1,
                    value: 7
                },
                Op::Add(1),
                Op::AddAt {
                    offset: 1,
                    delta: 2
                }
            ]
        );
        for offset in [-1, TAPE_LIMIT as isize] {
            let input = vec![Op::Input, Op::MulAdd { offset, factor: 2 }, Op::Output];
            assert_eq!(eliminate_dead_code(input.clone()), input);
        }
    }

    #[test]
    fn known_cells_keep_negative_addresses_unknown() {
        let mut cells = KnownCells::initially_zero();
        assert_eq!(cells.get(-1), None);
        cells.set(-1, Some(7));
        assert_eq!(cells.get(-1), None);
        cells.set(2, Some(9));
        assert_eq!(cells.get(2), Some(9));
        cells.reset_at_zero();
        assert_eq!(cells.get(0), Some(0));
        assert_eq!(cells.get(2), None);
    }

    #[test]
    fn dce_preserves_bounds_effects_for_unallocated_noop_accesses() {
        assert_eq!(
            eliminate_dead_code(vec![
                Op::ZeroAt {
                    offset: INITIAL_TAPE_SIZE as isize
                },
                Op::SetAt {
                    offset: INITIAL_TAPE_SIZE as isize,
                    value: 0
                }
            ]),
            vec![Op::ZeroAt {
                offset: INITIAL_TAPE_SIZE as isize
            }]
        );
        assert_eq!(
            eliminate_dead_code(vec![Op::BoundsCheck {
                min_offset: -1,
                max_offset: 0
            }]),
            vec![Op::BoundsCheck {
                min_offset: -1,
                max_offset: 0
            }]
        );
        assert_eq!(
            eliminate_dead_code(vec![
                Op::SetAt {
                    offset: -(INITIAL_TAPE_SIZE as isize),
                    value: 1
                },
                Op::Add(1)
            ]),
            vec![
                Op::SetAt {
                    offset: -(INITIAL_TAPE_SIZE as isize),
                    value: 1
                },
                Op::Add(1)
            ]
        );
    }

    #[test]
    fn fixed_point_pipeline_resolves_surviving_jump_targets() {
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(1),
            Op::JumpIfNonZero { target: 0 },
            Op::Output,
        ];
        assert_eq!(optimize(ops), vec![Op::Output]);
        let surviving = vec![
            Op::Add(1),
            Op::JumpIfZero { target: 0 },
            Op::Output,
            Op::JumpIfNonZero { target: 0 },
        ];
        assert_eq!(
            optimize(surviving),
            vec![
                Op::SetAt {
                    offset: 0,
                    value: 1
                },
                Op::JumpIfZero { target: 3 },
                Op::Output,
                Op::JumpIfNonZero { target: 1 }
            ]
        );
    }

    #[test]
    fn dce_keeps_a_nonzero_sub_and_tracks_the_cell_it_leaves_behind() {
        assert_eq!(eliminate_dead_code(vec![Op::Sub(3)]), vec![Op::Sub(3)]);
        // 0 - 3 = 253, so the loop is live. If Sub didn't update the known
        // value the cell would still look zero and the loop would be dropped.
        let live_loop = vec![
            Op::Sub(3),
            Op::JumpIfZero { target: 3 },
            Op::Output,
            Op::JumpIfNonZero { target: 1 },
        ];
        assert_eq!(eliminate_dead_code(live_loop.clone()), live_loop);
    }

    #[test]
    fn zero_deltas_past_the_allocated_tape_still_grow_it() {
        let beyond = INITIAL_TAPE_SIZE as isize;
        // A zero delta on an allocated cell is a no-op and gets dropped. Past
        // the end it still has to grow the tape, so it becomes a SetAt.
        assert_eq!(
            eliminate_dead_code(vec![Op::AddAt {
                offset: beyond,
                delta: 0
            }]),
            vec![Op::SetAt {
                offset: beyond,
                value: 0
            }]
        );
        assert_eq!(
            eliminate_dead_code(vec![
                Op::Add(1),
                Op::MulAdd {
                    offset: beyond,
                    factor: 0
                }
            ]),
            vec![
                Op::Add(1),
                Op::SetAt {
                    offset: beyond,
                    value: 0
                }
            ]
        );
    }
}
