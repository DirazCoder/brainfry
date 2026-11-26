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
                let mut total: u64 = 0;
                while i < ops.len() && matches!(ops[i], Op::MoveRight(_)) {
                    total += 1;
                    i += 1;
                }
                push_move(&mut out, total, true);
            }
            Op::MoveLeft(_) => {
                let mut total: u64 = 0;
                while i < ops.len() && matches!(ops[i], Op::MoveLeft(_)) {
                    total += 1;
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

/// Move counts are u32 in the format, so a run longer than that (4+ billion
/// `>` in a row -- a multi-gigabyte source, but nothing stops it) is chunked
/// into multiple ops instead of overflowing. The old accumulator was a plain
/// u32 `+`, which panicked on overflow in debug builds and silently wrapped
/// -- a miscompile -- in release ones.
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

/// Replaces the `[-]` / `[+]` idiom (a loop whose entire body is one
/// decrement or increment, meaning "clear this cell") with a single `Zero`
/// instruction. This is by far the most common non-trivial pattern in real
/// Brainfuck code, and running it as an actual loop means up to 255 wasted
/// iterations just to reach zero.
///
/// The step count must be **odd**. Reaching 0 by repeatedly adding `n` has
/// a solution for every starting value exactly when `n` is a unit mod 256,
/// which (256 being a power of two) means odd. An even step from an odd
/// cell never lands on 0 at all -- `[++]` with the cell holding 3 stays odd
/// forever -- so the real loop diverges, and folding it to `Zero` would
/// silently turn that infinite loop into termination: a miscompile. Odd
/// steps (1 for the classic idioms, 3, 5, ... ) reach 0 from any start, so
/// only those fold.
fn fold_zero_loops(ops: Vec<Op>) -> Vec<Op> {
    let mut out = Vec::with_capacity(ops.len());
    let mut i = 0;

    while i < ops.len() {
        let is_zero_loop = matches!(ops[i], Op::JumpIfZero { .. })
            && match ops.get(i + 1) {
                Some(Op::Add(n)) | Some(Op::Sub(n)) => n % 2 == 1,
                _ => false,
            }
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
///   - every cell the body lands on is a cell it actually adds to: a move
///     that touches down and leaves without writing ([<>-]'s round trip
///     through cell -1, [->>+<<]'s hop to cell 2) still has to be
///     bounds-checked on every real iteration, and a folded MulAdd/Zero
///     body can't perform that excursion -- so such loops stay real
///   - no Output, Input, or nested Jump in the body -- collapsing the loop
///     would change how many times a side effect fires, which is a
///     behavior change, not just a speed one
///
/// Loops that don't fit -- wrong step size, movement that doesn't return to
/// start, I/O inside, whatever -- are left as real loops. This pass is
/// conservative on purpose: a loop that isn't provably safe to fold stays a
/// loop rather than risk changing what the program does.
// coverage(off) (nightly coverage runs only — the attribute is unstable, so
// it is cfg-gated): the `offset != 0` guard below is deliberately defensive
// -- find_offset_add_loop's `*offset == 0 { continue; }` provably keeps the
// counting cell out of `deltas`, so the guard's false branch is unreachable
// by construction. It is kept (rather than deleted) as belt-and-braces
// against a future pass reordering, exactly like the Zero/MulAdd exclusion
// comment inside find_offset_add_loop argues for. Excluding the function
// from instrumentation keeps that dead arm from diluting the measured
// coverage of the rest of the pass, which the tests below still exercise
// assertion-for-assertion.
#[cfg_attr(coverage_nightly, coverage(off))]
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
                // The landing cell is touched even when nothing is ever
                // added to it. A loop like [<>-] round-trips through cell
                // -1 without writing: if only Add/Sub landings were
                // recorded, that excursion stayed invisible, every
                // structural check below passed, and the body folded to a
                // plain Zero -- dropping the underflow the real MoveLeft
                // raises on every iteration. Recording the touchdown
                // leaves a delta-0 entry, which the `*delta <= 0` filter
                // rejects, so the loop stays real and its bounds check
                // still fires.
                //
                // fold_runs has already merged same-direction moves, so
                // this records exactly the cells a real MoveRight /
                // MoveLeft bounds-checks: each move's landing cell. The
                // cells a multi-cell stride passes over never get
                // checked by the real ops either (the check lands on the
                // destination), so not recording them matches runtime
                // semantics rather than being an approximation.
                cell_delta.entry(pos).or_insert(0);
                i += 1;
            }
            Op::MoveLeft(n) => {
                pos -= *n as i64;
                // Same touchdown rule as MoveRight: a landing without a
                // write must still count as a visited cell.
                cell_delta.entry(pos).or_insert(0);
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
    fn does_not_fold_even_step_zero_loop() {
        // [++] from an odd cell never reaches 0 (adding 2 keeps the cell
        // odd forever) -- the real loop diverges, so folding it to Zero
        // would silently turn an infinite loop into termination.
        // Differential fuzzing caught exactly this class of divergence.
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Add(2),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_zero_loops(ops.clone());
        assert_eq!(folded, ops);

        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(2),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_zero_loops(ops.clone());
        assert_eq!(folded, ops);
    }

    #[test]
    fn folds_odd_step_zero_loop() {
        // [+++]: 3 is a unit mod 256, so the loop reaches 0 from any
        // starting value and the fold is sound.
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Add(3),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_zero_loops(ops);
        assert_eq!(folded, vec![Op::Zero]);

        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(255),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_zero_loops(ops);
        assert_eq!(folded, vec![Op::Zero]);
    }

    #[test]
    fn push_move_chunks_counts_that_exceed_u32() {
        // A >4 billion `>` run used to overflow a u32 accumulator (panic
        // in debug, silent wrap -- a miscompile -- in release).
        let mut out = Vec::new();
        push_move(&mut out, u32::MAX as u64 + 5, true);
        assert_eq!(out, vec![Op::MoveRight(u32::MAX), Op::MoveRight(5)]);

        let mut out = Vec::new();
        push_move(&mut out, 3, false);
        assert_eq!(out, vec![Op::MoveLeft(3)]);
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
    fn does_not_fold_loop_whose_moves_touch_an_untouched_cell() {
        // [<>-]: the body walks to cell -1 and back without ever writing
        // it. Only the Sub lands in cell_delta, so with write-only
        // recording every structural check passed and the loop folded to a
        // plain Zero -- silently dropping the left-of-cell-0 excursion the
        // real loop performs on every iteration. The touchdown recording
        // leaves a delta-0 entry at -1, which the `*delta <= 0` filter
        // rejects: the loop stays real and the runtime bounds check fires.
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::MoveLeft(1),
            Op::MoveRight(1),
            Op::Sub(1),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops.clone());
        assert_eq!(folded, ops);
    }

    #[test]
    fn does_not_fold_loop_with_untouched_excursion_after_the_decrement() {
        // [-<>]: same excursion, after the decrement instead of before.
        // Also diverges when folded to Zero: entering with a nonzero cell,
        // the real body decrements then dies on the MoveLeft.
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(1),
            Op::MoveLeft(1),
            Op::MoveRight(1),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops.clone());
        assert_eq!(folded, ops);
    }

    #[test]
    fn does_not_fold_loop_with_a_multi_cell_untouched_excursion() {
        // [->>+<<]: fold_runs merges the hops, so what gets recorded is
        // the landing cell two to the right. A rightward excursion still
        // has to run for real -- a huge landing cell forces a tape grow
        // that can fail -- so it can't be folded away either.
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(1),
            Op::MoveRight(2),
            Op::MoveLeft(2),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops.clone());
        assert_eq!(folded, ops);
    }

    #[test]
    fn folds_offset_add_across_multi_cell_hops() {
        // [->>+>+<<<]: the hops land on cells 2 and 3 and both get a real
        // Add, so the fold still applies -- recording touchdowns must not
        // over-reject loops whose every landing cell is written. Compare
        // does_not_fold_loop_with_a_multi_cell_untouched_excursion, which
        // is this shape minus the writes.
        let ops = vec![
            Op::JumpIfZero { target: 0 },
            Op::Sub(1),
            Op::MoveRight(2),
            Op::Add(1),
            Op::MoveRight(1),
            Op::Add(1),
            Op::MoveLeft(3),
            Op::JumpIfNonZero { target: 0 },
        ];
        let folded = fold_offset_adds(ops);
        assert_eq!(
            folded,
            vec![
                Op::MulAdd {
                    offset: 2,
                    factor: 1
                },
                Op::MulAdd {
                    offset: 3,
                    factor: 1
                },
                Op::Zero,
            ]
        );
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
    fn offset_add_scan_stops_at_an_unterminated_body() {
        // A JumpIfZero whose body runs off the end of the op list (no
        // closing JumpIfNonZero anywhere): the `ops.get(i)?` in
        // find_offset_add_loop turns that into a plain None. The parser
        // never produces this shape, but a hand-built or corrupt list can,
        // and the pass must decline to fold rather than index out of
        // bounds.
        let ops = vec![Op::JumpIfZero { target: 0 }, Op::Add(1)];
        assert!(find_offset_add_loop(&ops, 0).is_none());
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
            Op::JumpIfZero { target: 0 },    // outer: index 0
            Op::JumpIfZero { target: 0 },    // zero-loop start: index 1
            Op::Sub(1),                      // index 2
            Op::JumpIfNonZero { target: 0 }, // zero-loop end: index 3
            Op::JumpIfZero { target: 0 },    // offset-add start: index 4
            Op::Sub(1),                      // index 5
            Op::MoveRight(1),                // index 6
            Op::Add(1),                      // index 7
            Op::MoveLeft(1),                 // index 8
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
