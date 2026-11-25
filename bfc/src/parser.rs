use bfformat::Op;

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    /// 1-based line number, for error messages that point somewhere useful
    /// in the source instead of just saying "invalid program".
    pub line: usize,
}

/// Turns source text into a flat op list with unresolved jump targets
/// (placeholder 0, patched below). Every `+`/`-`/`>`/`<` becomes exactly one
/// op here — folding happens later in the optimizer, kept separate so this
/// pass only has to worry about syntax, not performance.
pub fn parse(source: &str) -> Result<Vec<Op>, ParseError> {
    let mut ops = Vec::new();
    let mut open_brackets: Vec<usize> = Vec::new();
    let mut line = 1;

    for ch in source.chars() {
        match ch {
            '\n' => line += 1,
            '+' => ops.push(Op::Add(1)),
            '-' => ops.push(Op::Sub(1)),
            '>' => ops.push(Op::MoveRight(1)),
            '<' => ops.push(Op::MoveLeft(1)),
            '.' => ops.push(Op::Output),
            ',' => ops.push(Op::Input),
            '[' => {
                open_brackets.push(ops.len());
                ops.push(Op::JumpIfZero { target: 0 });
            }
            ']' => {
                let open_index = open_brackets.pop().ok_or_else(|| ParseError {
                    message: "unmatched ']'".to_string(),
                    line,
                })?;

                ops.push(Op::JumpIfNonZero {
                    target: open_index as u32,
                });

                let close_index = ops.len() - 1;
                ops[open_index] = Op::JumpIfZero {
                    target: close_index as u32,
                };
            }
            // Anything else is a comment per the BF convention — non-command
            // characters are just ignored, not an error.
            _ => {}
        }
    }

    if let Some(open_index) = open_brackets.first() {
        return Err(ParseError {
            message: format!("unmatched '[' (instruction #{open_index})"),
            line,
        });
    }

    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_loop() {
        let ops = parse("+[-]").unwrap();
        assert_eq!(ops.len(), 4);
        assert!(matches!(ops[0], Op::Add(1)));
        assert!(matches!(ops[1], Op::JumpIfZero { target: 3 }));
        assert!(matches!(ops[2], Op::Sub(1)));
        assert!(matches!(ops[3], Op::JumpIfNonZero { target: 1 }));
    }

    #[test]
    fn rejects_unmatched_open() {
        assert!(parse("[+").is_err());
    }

    #[test]
    fn rejects_unmatched_close() {
        assert!(parse("+]").is_err());
    }

    #[test]
    fn ignores_comment_characters() {
        let ops = parse("+ hello world -").unwrap();
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn newlines_advance_the_reported_line() {
        // Error positions are 1-based and line counting is the only thing
        // '\n' does; bfc prints `{line}: {message}`, so a wrong count points
        // users at the wrong place in their source.
        let err = parse("\n\n+[").unwrap_err();
        assert_eq!(err.line, 3);
        assert!(err.message.contains("unmatched '['"));
    }

    #[test]
    fn unmatched_close_reports_its_own_line() {
        let err = parse("+\n]").unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.message.contains("unmatched ']'"));
    }

    #[test]
    fn unmatched_open_reports_the_first_open_bracket() {
        // With several unclosed brackets, the first one's instruction index
        // is what the message should point at.
        let err = parse("[[+").unwrap_err();
        assert_eq!(err.line, 1);
        assert!(err.message.contains("#0"));
    }
}
