use bfformat::Op;

/// Runs both optimization passes and returns a final op list with jump
/// targets re-resolved to match the new (shorter) instruction sequence.
pub fn optimize(ops: Vec<Op>) -> Vec<Op> {
    let folded = fold_runs(&ops);
    resolve_jumps(fold_zero_loops(folded))
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
        out.push(if is_add { Op::Add(chunk) } else { Op::Sub(chunk) });
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
                let open_index = open_brackets.pop().expect("unbalanced brackets survived parsing");
                resolved[open_index] = Op::JumpIfZero { target: i as u32 };
                resolved[i] = Op::JumpIfNonZero { target: open_index as u32 };
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
}
