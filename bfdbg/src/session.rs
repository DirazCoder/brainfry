//! One layer above the engine: runs parsed [`Command`]s against a
//! [`Debugger`] and renders every human-facing message. The REPL in
//! `main.rs` is a thin read/parse/dispatch loop on top of this, so the
//! entire command surface — output wording included — is unit-testable
//! by driving a `Debugger` plus `Vec<u8>` writers, and the e2e tests
//! drive the real binary.
//!
//! Stream discipline: the *program's* output (`.` bytes) goes to
//! `prog_out` (stdout in the real REPL); every debugger message goes to
//! `dbg_out` (stderr). A piped session therefore captures the program's
//! stdout byte-clean on fd 1 and the full debugger transcript on fd 2.

use crate::debugger::{Command, Debugger, Stop, WatchSpec, Which};
use std::io::Write;

/// The `help` text, one screenful, mirroring the usage block.
pub const HELP: &str = "\
commands:
  run | continue          resume until breakpoint, watchpoint, error, or end
  step [n]                execute n ops (default 1); steps through breakpoints
  back [n]                time-travel: rewind n ops through the undo log
  tape [center] [count]   show cells around `center` (default: ptr) — dec/hex/ASCII
  list [ctx]              show source window; ctx lines around the current one
  break [spec]            breakpoint: op index (42) or @line or @line:col
  clear <spec>            remove a breakpoint
  watch [spec]            watchpoint: `ptr <n>` (stop when ptr reaches it) or
                          `cell <n>` (stop when its value changes)
  unwatch ptr | cell      remove a watchpoint
  set cell[<n>] = <v>     overwrite a cell (grows the tape if needed)
  set ptr = <n>           move the pointer (grows the tape if needed)
  feed <v> ...            queue input byte(s) (0..=255) for the program's `,`
  info                    pc, next op, ptr, steps, undo, state
  quit                    exit the debugger
program output goes to stdout, debugger messages to stderr; `,` reads the
`feed` queue and EOF (empty queue) leaves the cell untouched.";

/// Writes one debugger message (already newline-terminated by the
/// caller) and flushes, so interactive sessions see output immediately.
/// Write failures on the debugger's own stderr are ignored — there's no
/// useful recovery once the transcript stream is gone, and the program's
/// stdout errors are the ones that matter semantically.
fn say(out: &mut dyn Write, s: &str) {
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// Executes one command against the debugger. Returns `true` when the
/// REPL should quit.
pub fn execute_command(
    dbg: &mut Debugger,
    cmd: &Command,
    prog_out: &mut dyn Write,
    dbg_out: &mut dyn Write,
) -> bool {
    match cmd {
        Command::Help => say(dbg_out, &format!("{HELP}\n")),
        Command::Info => say(dbg_out, &dbg.info()),
        Command::Quit => return true,

        Command::Run => {
            if dbg.is_finished() {
                say(
                    dbg_out,
                    "program already finished — `back` rewinds, then `run` resumes\n",
                );
            } else {
                let stop = dbg.run(prog_out);
                say(dbg_out, &format!("{}\n", stop_line(dbg, &stop)));
            }
        }

        Command::Step(n) => {
            if dbg.is_finished() {
                say(
                    dbg_out,
                    "program already finished — `back` rewinds, then `step` resumes\n",
                );
            } else {
                let mut done = 0;
                let mut stop = Stop::Ran;
                for _ in 0..*n {
                    if dbg.is_finished() {
                        stop = Stop::Finished;
                        break;
                    }
                    stop = dbg.step(prog_out);
                    done += 1;
                    if !matches!(stop, Stop::Ran) {
                        break;
                    }
                }
                say(dbg_out, &format!("stepped {done} op(s) — now {}\n", position_line(dbg)));
                if !matches!(stop, Stop::Ran) {
                    say(dbg_out, &format!("{}\n", stop_line(dbg, &stop)));
                }
            }
        }

        Command::Back(n) => {
            if !dbg.undo_enabled() {
                say(dbg_out, "undo log disabled (--undo-limit 0) — `back` is unavailable\n");
            } else {
                let done = dbg.back(*n);
                if done == 0 {
                    say(
                        dbg_out,
                        "nothing to rewind — this is the earliest state the undo log \
                         remembers\n",
                    );
                } else {
                    say(dbg_out, &format!("stepped back {done} op(s) — now {}\n", position_line(dbg)));
                }
            }
        }

        Command::Tape(center, count) => say(dbg_out, &dbg.tape_view(*center, *count)),
        Command::List(ctx) => say(dbg_out, &dbg.source_view(*ctx)),

        Command::Break(None) => {
            let b = dbg.breaks();
            if b.is_empty() {
                say(dbg_out, "no breakpoints\n");
            } else {
                let mut s = String::from("breakpoints:\n");
                for i in b {
                    s.push_str(&format!("  {} ({})\n", op_number(dbg, i), op_pos(dbg, i)));
                }
                say(dbg_out, &s);
            }
        }
        Command::Break(Some(spec)) => match dbg.resolve_break(spec) {
            Err(e) => say(dbg_out, &format!("error: {e}\n")),
            Ok(i) => {
                dbg.add_break(i);
                say(
                    dbg_out,
                    &format!("breakpoint at {} ({})\n", op_number(dbg, i), op_pos(dbg, i)),
                );
            }
        },
        Command::Clear(spec) => match dbg.resolve_break(spec) {
            Err(e) => say(dbg_out, &format!("error: {e}\n")),
            Ok(i) => {
                if dbg.clear_break(i) {
                    say(dbg_out, &format!("cleared breakpoint at {}\n", op_number(dbg, i)));
                } else {
                    say(dbg_out, &format!("no breakpoint at {}\n", op_number(dbg, i)));
                }
            }
        },

        Command::Watch(None) => {
            let (p, c) = (dbg.watch_ptr_target(), dbg.watch_cell_target());
            if p.is_none() && c.is_none() {
                say(dbg_out, "no watchpoints\n");
            } else {
                let mut s = String::from("watchpoints:\n");
                if let Some(n) = p {
                    s.push_str(&format!("  ptr == {n}\n"));
                }
                if let Some(n) = c {
                    s.push_str(&format!("  cell {n} (value changes)\n"));
                }
                say(dbg_out, &s);
            }
        }
        Command::Watch(Some(WatchSpec::Ptr(n))) => {
            dbg.set_watch_ptr(*n);
            say(
                dbg_out,
                &format!("watching ptr == {n} — stops when the pointer reaches cell {n}\n"),
            );
        }
        Command::Watch(Some(WatchSpec::Cell(n))) => {
            dbg.set_watch_cell(*n);
            say(dbg_out, &format!("watching cell {n} for value changes\n"));
        }
        Command::Unwatch(Which::Ptr) => {
            if dbg.clear_watch_ptr() {
                say(dbg_out, "ptr watchpoint cleared\n");
            } else {
                say(dbg_out, "no ptr watchpoint was set\n");
            }
        }
        Command::Unwatch(Which::Cell) => {
            if dbg.clear_watch_cell() {
                say(dbg_out, "cell watchpoint cleared\n");
            } else {
                say(dbg_out, "no cell watchpoint was set\n");
            }
        }

        Command::SetCell(idx, val) => {
            let len_before = dbg.tape_len();
            match dbg.set_cell(*idx, *val) {
                Ok(()) => {
                    let grew = if dbg.tape_len() != len_before {
                        format!(" (tape grew to {} cells)", dbg.tape_len())
                    } else {
                        String::new()
                    };
                    say(dbg_out, &format!("cell {idx} = {val}{grew}\n"));
                }
                Err(e) => say(dbg_out, &format!("error: {e}\n")),
            }
        }
        Command::SetPtr(idx) => {
            let len_before = dbg.tape_len();
            match dbg.set_ptr(*idx) {
                Ok(()) => {
                    let grew = if dbg.tape_len() != len_before {
                        format!(" (tape grew to {} cells)", dbg.tape_len())
                    } else {
                        String::new()
                    };
                    say(dbg_out, &format!("ptr = {idx}{grew}\n"));
                }
                Err(e) => say(dbg_out, &format!("error: {e}\n")),
            }
        }

        Command::Feed(bytes) => {
            let queued = dbg.feed(bytes.clone());
            say(
                dbg_out,
                &format!("queued {} byte(s) — input queue: {queued}\n", bytes.len()),
            );
        }
    }
    false
}

/// `op 5/16` — 1-based op numbering to match how `list` and breakpoints
/// refer to positions.
fn op_number(dbg: &Debugger, i: usize) -> String {
    format!("op {}/{}", i + 1, dbg.op_count())
}

/// `line 1, col 5` for an op index, or `--` when past the end.
fn op_pos(dbg: &Debugger, i: usize) -> String {
    match dbg.pos_of(i) {
        Some(pos) => format!("line {}, col {}", pos.line, pos.col),
        None => "--".to_string(),
    }
}

/// Where execution sits right now: op, source position, op label.
fn position_line(dbg: &Debugger) -> String {
    if dbg.pc() >= dbg.op_count() {
        format!("past the last op ({} ops total)", dbg.op_count())
    } else {
        format!(
            "{} — {} — {}",
            op_number(dbg, dbg.pc()),
            op_pos(dbg, dbg.pc()),
            dbg.op_label(dbg.pc())
        )
    }
}

/// Renders why execution stopped, in the vocabulary the docs use.
fn stop_line(dbg: &Debugger, stop: &Stop) -> String {
    match stop {
        Stop::Ran => "ran one op".to_string(),
        Stop::Break(i) => format!(
            "stopped — breakpoint at {} ({})",
            op_number(dbg, *i),
            op_pos(dbg, *i)
        ),
        Stop::WatchPtr(n) => format!("stopped — watchpoint: ptr == {n}"),
        Stop::WatchCell { cell, from, to } => {
            format!("stopped — watchpoint: cell {cell} changed {from} -> {to}")
        }
        Stop::Errored(msg) => format!(
            "runtime error: {msg} (failed at {})\n\
             hint: rewind it with `back`, patch the tape with `set cell[..] = ..`, \
             then `run` to retry",
            position_line(dbg)
        ),
        Stop::Finished => format!("program finished — {} steps executed", dbg.steps()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debugger::{parse_command, Debugger, DEFAULT_UNDO_LIMIT};
    use bfc::parser;
    use std::io::Write;

    /// Runs a whole command script against a fresh debugger and returns
    /// (program stdout, debugger stderr). Parse errors render exactly the
    /// way the real REPL renders them ("error: ..."), so bad-command
    /// tests flow through the same path as the binary.
    fn script(source: &str, commands: &[&str]) -> (Vec<u8>, String) {
        let ops = parser::parse(source).unwrap();
        let mut dbg = Debugger::new(ops, source.to_string(), DEFAULT_UNDO_LIMIT);
        let mut prog = Vec::new();
        let mut dbg_out = Vec::new();
        for line in commands {
            match parse_command(line) {
                Ok(cmd) => {
                    if execute_command(&mut dbg, &cmd, &mut prog, &mut dbg_out) {
                        break;
                    }
                }
                Err(msg) => {
                    let _ = writeln!(dbg_out, "error: {msg}");
                }
            }
        }
        (prog, String::from_utf8_lossy(&dbg_out).into_owned())
    }

    #[test]
    fn run_finishes_and_splits_the_streams() {
        let (prog, dbg) = script("+.", &["run", "info"]);
        assert_eq!(prog, vec![1u8], "program bytes on the program stream");
        assert!(dbg.contains("program finished — 2 steps executed"), "{dbg}");
        assert!(dbg.contains("state:   finished"), "{dbg}");
        // info after the end shows the machine, not a position.
        assert!(dbg.contains("past the last op"), "{dbg}");
    }

    #[test]
    fn the_error_back_patch_retry_workflow_renders() {
        // The reason this tool exists, end to end: hit the underflow,
        // rewind, patch, retry, finish.
        let (prog, dbg) = script(
            "<",
            &["run", "back", "set ptr = 2", "run", "info"],
        );
        assert!(prog.is_empty());
        assert!(dbg.contains("runtime error: pointer moved left of cell 0"), "{dbg}");
        assert!(dbg.contains("failed at op 1/1"), "{dbg}");
        assert!(dbg.contains("hint: rewind it with `back`"), "{dbg}");
        assert!(dbg.contains("stepped back 1 op(s)"), "{dbg}");
        assert!(dbg.contains("ptr = 2"), "{dbg}");
        assert!(dbg.contains("program finished — 1 steps executed"), "{dbg}");
        // The retry means the final state is a success, not an error.
        assert!(dbg.contains("state:   finished"), "{dbg}");
    }

    #[test]
    fn breakpoints_and_stepping_render_positions() {
        let (_, dbg) = script(
            "+++\n.\n",
            &["break 1", "run", "list", "step", "info"],
        );
        assert!(dbg.contains("breakpoint at op 2/4 (line 1, col 2)"), "{dbg}");
        assert!(dbg.contains("stopped — breakpoint at op 2/4 (line 1, col 2)"), "{dbg}");
        // `list` marks the paused op with a caret.
        assert!(dbg.contains("next: op 2/4 — line 1, col 2 — Add(1)"), "{dbg}");
        assert!(dbg.contains("^"), "{dbg}");
        // `step` walks through the breakpoint by design.
        assert!(dbg.contains("stepped 1 op(s) — now op 3/4 — line 1, col 3 — Add(1)"), "{dbg}");
    }

    #[test]
    fn watchpoints_render_their_trigger() {
        let (_, dbg) = script(">>>", &["watch ptr 2", "run"]);
        assert!(dbg.contains("watching ptr == 2"), "{dbg}");
        assert!(dbg.contains("stopped — watchpoint: ptr == 2"), "{dbg}");

        let (_, dbg) = script("+>+", &["watch cell 1", "run"]);
        assert!(dbg.contains("watching cell 1 for value changes"), "{dbg}");
        assert!(dbg.contains("stopped — watchpoint: cell 1 changed 0 -> 1"), "{dbg}");
    }

    #[test]
    fn tape_list_and_feed_render() {
        let (prog, dbg) = script(
            "x\n++.",
            &["tape", "list", "feed 33", "run"],
        );
        // tape: header plus the ptr marker.
        assert!(dbg.contains("tape: 30000 cells, ptr at 0"), "{dbg}");
        assert!(dbg.contains("> "), "{dbg}");
        // list: line-numbered source with the caret under the first op.
        assert!(dbg.contains("1 | x"), "{dbg}");
        assert!(dbg.contains("2 | ++."), "{dbg}");
        // feed + `,`: none here, but the queue message renders.
        assert!(dbg.contains("queued 1 byte(s) — input queue: 1"), "{dbg}");
        assert_eq!(prog, vec![2u8]);
    }

    #[test]
    fn set_renders_growth_and_bad_specs_error() {
        let (_, dbg) = script("+", &["set cell[40000] = 5", "set ptr = 50000", "info"]);
        assert!(dbg.contains("cell 40000 = 5 (tape grew to 40001 cells)"), "{dbg}");
        assert!(dbg.contains("ptr = 50000 (tape grew to 50001 cells)"), "{dbg}");
        assert!(dbg.contains("machine: ptr 50000, tape 50001 cells"), "{dbg}");

        // Bad commands through the REPL path produce errors, not panics.
        let (_, dbg) = script("+", &["break 99", "break @9", "watch bogus 1", "feed 999"]);
        assert!(dbg.contains("error: program has 1 ops — there is no op 99"), "{dbg}");
        assert!(dbg.contains("error: no instruction on line 9"), "{dbg}");
        assert!(dbg.contains("error: watch what? \"bogus\""), "{dbg}");
        assert!(dbg.contains("error: feed values must be 0..=255, got \"999\""), "{dbg}");
    }

    #[test]
    fn quit_stops_the_session_and_help_lists_commands() {
        let (prog, dbg) = script("+.", &["help", "run", "quit", "run"]);
        assert!(dbg.contains("commands:"), "{dbg}");
        assert!(dbg.contains("back [n]"), "{dbg}");
        assert!(dbg.contains("feed <v> ..."), "{dbg}");
        // The second run never happened: quit ended the script.
        assert_eq!(prog, vec![1u8]);
        assert!(!dbg.contains("program already finished"), "{dbg}");
    }
}
