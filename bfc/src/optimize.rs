use bfformat::Op;

/// Runs all optimization passes in order and returns a final op list with
/// jump targets re-resolved to match the new (shorter) instruction sequence.
/// Each pass either shrinks the op list or leaves it unchanged -- none of
/// them grow it -- so resolve_jumps only ever has to run once, at the end.
pub fn optimize(ops: Vec<Op>) -> Vec<Op> {
    let folded = fold_runs(&ops);
    let zeroed = fold_zero_loops(folded);
    let offset_added = fold_offset_adds(zeroed);
    let scanned = fold_scan_loops(offset_added);
    resolve_jumps(scanned)
}

/// Collapses consecutive runs of the same +/-/>/< into one instruction with
/// a count, so e.g. ten `+` in a row becomes one `Add(10)` instead of the
/// runtime dispatching on the same op ten separate times. Add/Sub counts
/// wrap at 256 (matching cell wraparound); Move counts use the full u32
/// range since tape position isn't bounded the same way.
fn fold_runs(ops: &[Op]) -> Vec<Op> {
    let mut out = Vec::with_capacity(ops.len());
    let mut i = 0;

    while i < ops.len() {
        match ops[i] {
            Op::Add(_) => {
                let mut total: u32 = 0;
                while i < ops.len() && matches!(ops[i], Op::Add(_)) {
                    total = total.wrapping_add(1);
                    i += 1;
                }
                push_add_sub(&mut out, total, true);
            }
            Op::Sub(_) => {
                let mut total: u32 = 0;
                while i < ops.len() && matches!(ops[i], Op::Sub(_)) {
                    total = total.wrapping_add(1);
                    i += 1;
                }
                push_add_sub(&mut out, total, false);
            }
            Op::MoveRight(_) => {
                let mut total: u32 = 0;
                while i < ops.len() && matches!(ops[i], Op::MoveRight(_)) {
                    total += 1;
                    i += 1;
                }
                out.push(Op::MoveRight(total));
            }
            Op::MoveLeft(_) => {
                let mut total: u32 = 0;
                while i < ops.len() && matches!(ops[i], Op::MoveLeft(_)) {
                    total += 1;
                    i += 1;
                }
                out.push(Op::MoveLeft(total));
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }

    out
}

/// Add/Sub store a single u8 count, but a run longer than 255 has to become
/// multiple instructions since the format can't express a bigger count in
/// one op. This only matters for pathological source with 256+ repeated
/// symbols in a row, which is rare but not impossible.
fn push_add_sub(out: &mut Vec<Op>, total: u32, is_add: bool) {
    let mut remaining = total;
    while remaining > 0 {
        let chunk = remaining.min(255) as u8;
        out.push(if is_add {
            Op::Add(chunk)
        } else {
            Op::Sub(chunk)
        });
        remaining -= chunk as u32;
    }
}

/// Replaces the `[-]` / `[+]` idiom (a loop whose entire body is one
/// decrement or increment, meaning "clear this cell") with a single `Zero`
/// instruction. This is by far the most common non-trivial pattern in real
/// Brainfuck code, and running it as an actual loop means up to 255 wasted
/// iterations just to reach zero.
fn fold_zero_loops(ops: Vec<Op>) -> Vec<Op> {
    let mut out = Vec::with_capacity(ops.len());
    let mut i = 0;

    while i < ops.len() {
        let is_zero_loop = matches!(ops[i], Op::JumpIfZero { .. })
            && matches!(ops.get(i + 1), Some(Op::Add(_)) | Some(Op::Sub(_)))
            && matches!(ops.get(i + 2), Some(Op::JumpIfNonZero { .. }));

        if is_zero_loop {
            out.push(Op::Zero);
            i += 3;
        } else {
            out.push(ops[i]);
            i += 1;
        }
    }

    out
}

/// Replaces loops like `[->+<]`, `[->++<]`, or `[->+>+<<]` -- a decrement of
/// the current cell paired with adds to one or more other cells, netting
/// back to the starting position -- with direct MulAdd instructions plus a
/// final Zero. These loops always run exactly N times, where N is the
/// current cell's value going in, so there's no reason to actually iterate:
/// the result is fully determined by the starting state.
///
/// A candidate loop body must satisfy all of:
///   - net cell-pointer movement across the whole body is zero (it has to
///     land back where it started, or "offset" from that start doesn't
///     mean anything)
///   - the starting cell (offset 0) has a net change of exactly Sub(1) --
///     anything else means the loop doesn't run exactly N times for a cell
///     holding N, so folding it would change the result
///   - every other touched cell only receives Add, never Sub (this pass
///     only handles the "spread value into other cells" case, not general
///     arithmetic loops)
///   - no Output, Input, or nested Jump in the body -- collapsing the loop
///     would change how many times a side effect fires, which is a
///     behavior change, not just a speed one
///
/// Loops that don't fit -- wrong step size, movement that doesn't return to
/// start, I/O inside, whatever -- are left as real loops. This pass is
/// conservative on purpose: a loop that isn't provably safe to fold stays a
/// loop rather than risk changing what the program does.
fn fold_offset_adds(ops: Vec<Op>) -> Vec<Op> {
    let mut out = Vec::with_capacity(ops.len());
    let mut i = 0;

    while i < ops.len() {
        match find_offset_add_loop(&ops, i) {
            Some((body_end, deltas)) => {
                for (offset, factor) in deltas {
                    if offset != 0 {
                        out.push(Op::MulAdd { offset, factor });
                    }
                }
                out.push(Op::Zero);
                i = body_end;
            }
            None => {
                out.push(ops[i]);
                i += 1;
            }
        }
    }

    out
}

/// Checks whether the loop starting at `ops[start]` (which must be a
/// JumpIfZero) is a valid offset-add candidate. Returns the index just past
/// the loop's closing JumpIfNonZero and the list of (offset, factor) pairs
/// to apply, in the order they should be emitted, if it is.
fn find_offset_add_loop(ops: &[Op], start: usize) -> Option<(usize, Vec<(i32, u8)>)> {
    if !matches!(ops[start], Op::JumpIfZero { .. }) {
        return None;
    }

    let mut pos: i64 = 0;
    let mut cell_delta: std::collections::BTreeMap<i64, i64> = std::collections::BTreeMap::new();
    let mut i = start + 1;

    loop {
        match ops.get(i)? {
            Op::Add(n) => {
                *cell_delta.entry(pos).or_insert(0) += *n as i64;
                i += 1;
            }
            Op::Sub(n) => {
                *cell_delta.entry(pos).or_insert(0) -= *n as i64;
                i += 1;
            }
            Op::MoveRight(n) => {
                pos += *n as i64;
                i += 1;
            }
            Op::MoveLeft(n) => {
                pos -= *n as i64;
                i += 1;
            }
            Op::JumpIfNonZero { .. } => break,
            // Output, Input, nested Jump, Zero, MulAdd, Scan: none of these
            // can appear in a foldable offset-add body. Zero and MulAdd
            // specifically can't show up here because this pass runs right
            // after fold_zero_loops and before any prior fold_offset_adds
            // output could exist in the same body -- but excluding them
            // explicitly (rather than falling through) keeps this correct
            // even if a future pass reordering changes that.
            _ => return None,
        }
    }

    // must return to the starting cell, or "offset" is meaningless
    if pos != 0 {
        return None;
    }

    // starting cell must decrement by exactly 1, or the loop doesn't run
    // exactly N times for a cell holding N
    if cell_delta.get(&0) != Some(&-1) {
        return None;
    }

    let mut deltas = Vec::with_capacity(cell_delta.len());
    for (offset, delta) in &cell_delta {
        if *offset == 0 {
            continue;
        }
        // this pass only folds "spread into other cells," not general
        // in-loop subtraction on non-counting cells -- and the delta has
        // to fit in the factor field's u8 range to be representable at all
        if *delta <= 0 || *delta > u8::MAX as i64 {
            return None;
        }
        deltas.push((*offset as i32, *delta as u8));
    }

    Some((i + 1, deltas))
}

/// Replaces loops like `[>]`, `[<]`, `[>>]`, or `[<<<]` -- a loop whose
/// entire body is a single move in one direction -- with a single Scan
/// instruction. The source pattern walks the tape one step (or one
/// multi-cell stride) at a time, checking for a zero cell after every
/// step; Scan carries the same stride and direction as one instruction
/// instead of a jump-move-jump per iteration. This doesn't change how many
/// cells get inspected -- that's still bounded by wherever the next zero
/// cell is -- it just removes the per-step dispatch overhead.
fn fold_scan_loops(ops: Vec<Op>) -> Vec<Op> {
    let mut out = Vec::with_capacity(ops.len());
    let mut i = 0;

    while i < ops.len() {
        let stride = match (ops.get(i), ops.get(i + 1), ops.get(i + 2)) {
            (
                Some(Op::JumpIfZero { .. }),
                Some(Op::MoveRight(n)),
                Some(Op::JumpIfNonZero { .. }),
            ) => Some(*n as i32),
            (
                Some(Op::JumpIfZero { .. }),
                Some(Op::MoveLeft(n)),
                Some(Op::JumpIfNonZero { .. }),
            ) => Some(-(*n as i32)),
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

/// Recomputes every jump target from scratch by walking the op list and
/// matching brackets again. Needed because folding changes instruction
/// indices, so the targets baked in during parsing no longer point at the
/// right place.
fn resolve_jumps(ops: Vec<Op>) -> Vec<Op> {
    let mut resolved = ops;
    let mut open_brackets: Vec<usize> = Vec::new();

    for i in 0..resolved.len() {
        match resolved[i] {
            Op::JumpIfZero { .. } => open_brackets.push(i),
            Op::JumpIfNonZero { .. } => {
                // Parser already guaranteed brackets balance, so this always
                // has a match.
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

    #[test]
    fn folds_repeated_add() {
        let ops = vec![Op::Add(1), Op::Add(1), Op::Add(1)];
        let folded = fold_runs(&ops);
        assert_eq!(folded, vec![Op::Add(3)]);
    }

    #[test]
    fn splits_runs_over_255() {
        let ops = vec![Op::Add(1); 300];
        let folded = fold_runs(&ops);
        assert_eq!(folded, vec![Op::Add(255), Op::Add(45)]);
    }

    #[test]
    fn detects_zero_loop() {
        let ops = vec![
            Op::JumpIfZero { target: 2 },
            Op::Sub(1),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_zero_loops(ops);
        assert_eq!(folded, vec![Op::Zero]);
    }

    #[test]
    fn resolves_jumps_after_folding_changes_indices() {
        // +++[-] should fold to [Add(3), Zero] -- no brackets left to resolve,
        // but a loop that survives folding needs its target fixed up.
        let ops = vec![
            Op::Add(1),
            Op::JumpIfZero { target: 0 }, // placeholder, wrong on purpose
            Op::Output,
            Op::JumpIfNonZero { target: 0 }, // placeholder, wrong on purpose
        ];
        let resolved = resolve_jumps(ops);
        assert_eq!(resolved[1], Op::JumpIfZero { target: 3 });
        assert_eq!(resolved[3], Op::JumpIfNonZero { target: 1 });
    }

    #[test]
    fn folds_simple_offset_add() {
        // [->+<]  (move value from cell 0 into cell 1, zero cell 0)
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(1),
            Op::MoveRight(1),
            Op::Add(1),
            Op::MoveLeft(1),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops);
        assert_eq!(
            folded,
            vec![
                Op::MulAdd {
                    offset: 1,
                    factor: 1
                },
                Op::Zero,
            ]
        );
    }

    #[test]
    fn folds_offset_add_with_multiplier() {
        // [->++<]  (double the value into cell 1)
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(1),
            Op::MoveRight(1),
            Op::Add(2),
            Op::MoveLeft(1),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops);
        assert_eq!(
            folded,
            vec![
                Op::MulAdd {
                    offset: 1,
                    factor: 2
                },
                Op::Zero,
            ]
        );
    }

    #[test]
    fn folds_offset_add_spreading_to_multiple_cells() {
        // [->+>+<<]  (copy value to both cell 1 and cell 2)
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(1),
            Op::MoveRight(1),
            Op::Add(1),
            Op::MoveRight(1),
            Op::Add(1),
            Op::MoveLeft(2),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops);
        assert_eq!(
            folded,
            vec![
                Op::MulAdd {
                    offset: 1,
                    factor: 1
                },
                Op::MulAdd {
                    offset: 2,
                    factor: 1
                },
                Op::Zero,
            ]
        );
    }

    #[test]
    fn folds_offset_add_to_the_left() {
        // [-<+>]  (move value into cell -1)
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(1),
            Op::MoveLeft(1),
            Op::Add(1),
            Op::MoveRight(1),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops);
        assert_eq!(
            folded,
            vec![
                Op::MulAdd {
                    offset: -1,
                    factor: 1
                },
                Op::Zero,
            ]
        );
    }

    #[test]
    fn does_not_fold_loop_that_does_not_return_to_start() {
        // [->+]  never comes back, so "offset" would be meaningless
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(1),
            Op::MoveRight(1),
            Op::Add(1),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops.clone());
        assert_eq!(folded, ops);
    }

    #[test]
    fn does_not_fold_loop_with_output_inside() {
        // [->+<.]  has a side effect that must fire once per real iteration
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(1),
            Op::MoveRight(1),
            Op::Add(1),
            Op::MoveLeft(1),
            Op::Output,
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops.clone());
        assert_eq!(folded, ops);
    }

    #[test]
    fn does_not_fold_loop_with_input_inside() {
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(1),
            Op::Input,
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops.clone());
        assert_eq!(folded, ops);
    }

    #[test]
    fn does_not_fold_loop_with_wrong_step_size() {
        // [--->+<]  decrements by 3, so it doesn't run N times for cell
        // value N -- must stay a real loop
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(3),
            Op::MoveRight(1),
            Op::Add(1),
            Op::MoveLeft(1),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops.clone());
        assert_eq!(folded, ops);
    }

    #[test]
    fn does_not_fold_loop_with_subtract_on_other_cell() {
        // [->-<]  decrements the other cell instead of adding -- not the
        // "spread value" pattern this pass targets
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(1),
            Op::MoveRight(1),
            Op::Sub(1),
            Op::MoveLeft(1),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops.clone());
        assert_eq!(folded, ops);
    }

    #[test]
    fn does_not_fold_loop_with_nested_jump() {
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(1),
            Op::JumpIfZero { target: 0 },
            Op::JumpIfNonZero { target: 0 },
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops.clone());
        assert_eq!(folded, ops);
    }

    #[test]
    fn folds_simple_scan_right() {
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::MoveRight(1),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_scan_loops(ops);
        assert_eq!(folded, vec![Op::Scan { stride: 1 }]);
    }

    #[test]
    fn folds_scan_left_with_stride() {
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::MoveLeft(3),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_scan_loops(ops);
        assert_eq!(folded, vec![Op::Scan { stride: -3 }]);
    }

    #[test]
    fn does_not_scan_fold_loop_with_more_than_move_in_body() {
        // [>+]  has an Add in the body, so it's not a pure scan
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::MoveRight(1),
            Op::Add(1),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_scan_loops(ops.clone());
        assert_eq!(folded, ops);
    }

    #[test]
    fn full_pipeline_folds_offset_add_and_resolves_surrounding_jumps() {
        // A zero loop, then an offset-add loop, wrapped in an outer
        // JumpIfZero/JumpIfNonZero pair, to check that resolve_jumps still
        // produces correct targets once earlier passes have shrunk things
        // by different amounts.
        let ops = vec![
            Op::JumpIfZero { target: 0 }, // outer: index 0
            Op::JumpIfZero { target: 0 }, // zero-loop start: index 1
            Op::Sub(1),                   // index 2
            Op::JumpIfNonZero { target: 0 }, // zero-loop end: index 3
            Op::JumpIfZero { target: 0 }, // offset-add start: index 4
            Op::Sub(1),                   // index 5
            Op::MoveRight(1),             // index 6
            Op::Add(1),                   // index 7
            Op::MoveLeft(1),              // index 8
            Op::JumpIfNonZero { target: 0 }, // offset-add end: index 9
            Op::JumpIfNonZero { target: 0 }, // outer end: index 10
        ];
        let result = optimize(ops);
        // expected shape after all folds: [JumpIfZero, Zero, MulAdd, Zero, JumpIfNonZero]
        assert_eq!(
            result,
            vec![
                Op::JumpIfZero { target: 4 },
                Op::Zero,
                Op::MulAdd {
                    offset: 1,
                    factor: 1
                },
                Op::Zero,
                Op::JumpIfNonZero { target: 0 },
            ]
        );
    }
}
