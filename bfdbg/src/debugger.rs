//! The engine behind `bfdbg`: a single-op-at-a-time execution core with
//! breakpoints, watchpoints, time-travel, and live tape editing — the
//! "make a Brainfuck program observable" layer.
//!
//! ## Why its own engine (and why the *raw* op list)
//!
//! `bfinterp`'s `run` loop and `bfrun`'s VM both execute to completion;
//! a debugger needs to stop between ops, so neither can be reused. This
//! engine re-implements their exact semantics — `bfinterp`'s, since the
//! raw source is what's being debugged — one op per `step()` call: 8-bit
//! wrapping cells, tape growing to exactly the cell landed on, `,` on an
//! empty input queue leaving the cell untouched, and left-of-cell-0 as a
//! clean error with the shared wording.
//!
//! It deliberately debugs the **unoptimized** op list. The parser emits
//! one op per `+ - > < . , [ ]` character, so op index N maps to an
//! exact line and column in the `.bf` source — that correspondence is
//! the entire value of a source view. The optimizer's folded forms
//! (`Add(200)`, `Zero`, `MulAdd`, `Scan`) correspond to *no single
//! character*, so stepping them can't be shown against the source; a
//! debugger that steps invisible instructions is a tracer, not a
//! debugger. (A `.bfry` bytecode file also can't be loaded for the same
//! reason — and because parse errors, which a debugger should surface,
//! happen before bytecode exists.)
//!
//! ## Time travel
//!
//! Every executed op appends one tiny undo record —
//! `(op_index, old_ptr, old_cell_value)` — before it runs. Every op in
//! the raw list mutates at most one cell (at the pre-op pointer) plus the
//! pointer itself, so that triple inverts any op exactly: restore the
//! cell, restore the pointer, point `pc` back at the op. `back` replays
//! records in reverse; rewinding out of a whole loop is just N backs.
//! A *failed* op records its (unchanged) state too, so a runtime error
//! can be rewound, patched with `set`, and re-run — the workflow this
//! whole tool exists for. The log is bounded (default 1 Mi steps ≈
//! 24 MB, `--undo-limit` to tune, `0` off) because a program like
//! mandelbrot executes ~120M ops and an unbounded log would OOM the
//! machine long before the program finished.
//!
//! ## I/O model
//!
//! Program output (`.`) is written to a caller-supplied stdout stream
//! byte-for-byte; debugger messages go to stderr, so piped sessions keep
//! the program's stdout clean. Program input (`,`) drains a queue fed
//! by `feed <byte>` or preloaded from `--stdin <file>`; an empty queue
//! is EOF and leaves the cell untouched.

use bfformat::Op;
use std::collections::{BTreeSet, VecDeque};
use std::io::Write;

/// Classic Brainfuck starting tape size, matching `bfrun`/`bfinterp`.
pub const INITIAL_TAPE_SIZE: usize = 30_000;

/// Default undo-history capacity: 1 Mi steps is ~24 MB of records, deep
/// enough to rewind out of any realistic mistake without OOM-ing on a
/// long-running program.
pub const DEFAULT_UNDO_LIMIT: usize = 1 << 20;

// ---- source map ----------------------------------------------------------

/// Where one parsed op sits in the original source: 1-based line and
/// column. Built by [`build_srcmap`] with the same character walk the
/// parser uses, so `srcmap[i]` is the position of the exact source
/// character that produced `ops[i]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

fn build_srcmap(source: &str) -> Vec<Pos> {
    let mut map = Vec::new();
    let (mut line, mut col) = (1usize, 1usize);
    for ch in source.chars() {
        match ch {
            '\n' => {
                line += 1;
                col = 1;
            }
            '+' | '-' | '>' | '<' | '.' | ',' | '[' | ']' => {
                map.push(Pos { line, col });
                col += 1;
            }
            _ => {
                col += 1; // comment character — same walk the parser does
            }
        }
    }
    map
}

// ---- undo -----------------------------------------------------------------

/// One executed op's pre-state. `(op_index, old_ptr, old_cell_value)`
/// plus whether the op actually executed: enough to invert any raw op,
/// since raw ops touch at most one cell (at the pre-op pointer) and the
/// pointer. A *failed* op still records (so `back` can rewind out of an
/// error and retry), but `counted` marks that it never incremented the
/// step counter, so rewinding it must not decrement one either.
#[derive(Debug, Clone, Copy)]
struct UndoRecord {
    pc: usize,
    ptr: usize,
    cell: u8,
    counted: bool,
}

// ---- stops ----------------------------------------------------------------

/// Why execution stopped. `Ran` is "the op executed and nothing fired"
/// — what a plain `step` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    Ran,
    Break(usize),
    WatchPtr(usize),
    WatchCell { cell: usize, from: u8, to: u8 },
    Errored(String),
    Finished,
}

// ---- commands -------------------------------------------------------------

/// Where to stop: an op index, or a source line (with optional column).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreakSpec {
    Op(usize),
    Line(usize, Option<usize>),
}

/// What to watch: the pointer reaching a cell, or a cell's value changing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchSpec {
    Ptr(usize),
    Cell(usize),
}

/// Which watchpoint `unwatch` targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Ptr,
    Cell,
}

/// One parsed REPL command. Parsing lives here (not in `main.rs`) so
/// the whole command surface is unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run,
    Step(usize),
    Back(usize),
    Tape(Option<usize>, usize),
    List(usize),
    Break(Option<BreakSpec>),
    Clear(BreakSpec),
    Watch(Option<WatchSpec>),
    Unwatch(Which),
    SetCell(usize, u8),
    SetPtr(usize),
    Feed(Vec<u8>),
    Info,
    Help,
    Quit,
}

/// Parses one command line. Specs: breakpoints take `42` (op index) or
/// `@3` / `@3:5` (line[:col]); watchpoints take `ptr <n>` or `cell <n>`.
pub fn parse_command(line: &str) -> Result<Command, String> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let Some(head) = tokens.first().copied() else {
        return Err("empty command".to_string());
    };
    let num = |t: &str, what: &str| -> Result<usize, String> {
        t.parse::<usize>()
            .map_err(|_| format!("{what} must be a non-negative integer, got {t:?}"))
    };

    match head {
        "run" | "r" | "continue" | "c" => Ok(Command::Run),
        "step" | "s" => Ok(Command::Step(match tokens.get(1) {
            Some(t) => num(t, "step count")?,
            None => 1,
        })),
        "back" | "b" => Ok(Command::Back(match tokens.get(1) {
            Some(t) => num(t, "back count")?,
            None => 1,
        })),
        "tape" | "t" => {
            // tape [center] [count] — a lone number is the center cell.
            let center = match tokens.get(1) {
                Some(t) => Some(num(t, "tape center")?),
                None => None,
            };
            let count = match tokens.get(2) {
                Some(t) => {
                    let n = num(t, "tape count")?;
                    if n == 0 {
                        return Err("tape count must be at least 1".to_string());
                    }
                    n
                }
                None => 16,
            };
            Ok(Command::Tape(center, count))
        }
        "list" | "l" => Ok(Command::List(match tokens.get(1) {
            Some(t) => num(t, "list context lines")?,
            None => 2,
        })),
        "break" | "br" => match tokens.get(1) {
            None => Ok(Command::Break(None)),
            Some(t) => Ok(Command::Break(Some(parse_break_spec(t)?))),
        },
        "clear" => {
            let t = tokens.get(1).ok_or("clear needs a spec: 42 | @3 | @3:5")?;
            Ok(Command::Clear(parse_break_spec(t)?))
        }
        "watch" | "w" => match tokens.get(1) {
            None => Ok(Command::Watch(None)),
            Some(&kind) => {
                let t = tokens
                    .get(2)
                    .ok_or(format!("watch needs a cell number: watch {kind} 42"))?;
                let n = num(t, "watch target")?;
                match kind {
                    "ptr" => Ok(Command::Watch(Some(WatchSpec::Ptr(n)))),
                    "cell" => Ok(Command::Watch(Some(WatchSpec::Cell(n)))),
                    other => Err(format!("watch what? {other:?} — use \"ptr <n>\" or \"cell <n>\"")),
                }
            }
        },
        "unwatch" => {
            let t = tokens.get(1).ok_or("unwatch needs: unwatch ptr | unwatch cell")?;
            match *t {
                "ptr" => Ok(Command::Unwatch(Which::Ptr)),
                "cell" => Ok(Command::Unwatch(Which::Cell)),
                other => Err(format!("unwatch what? {other:?} — use \"unwatch ptr\" or \"unwatch cell\"")),
            }
        }
        "set" => Ok(parse_set(&tokens[1..])?),
        "feed" => {
            let mut bytes = Vec::new();
            for t in &tokens[1..] {
                let v = t.parse::<u8>().map_err(|_| {
                    format!("feed values must be 0..=255, got {t:?}")
                })?;
                bytes.push(v);
            }
            if bytes.is_empty() {
                return Err("feed needs byte values: feed 65 10".to_string());
            }
            Ok(Command::Feed(bytes))
        }
        "info" | "i" => Ok(Command::Info),
        "help" | "?" | "h" => Ok(Command::Help),
        "quit" | "q" | "exit" => Ok(Command::Quit),
        other => Err(format!("unknown command: {other:?} (try `help`)")),
    }
}

fn parse_break_spec(spec: &str) -> Result<BreakSpec, String> {
    if let Some(rest) = spec.strip_prefix('@') {
        let mut parts = rest.splitn(2, ':');
        let line = parts.next().unwrap_or("");
        let line = line.parse::<usize>().map_err(|_| {
            format!("@line must be a positive integer, got {spec:?}")
        })?;
        if line == 0 {
            return Err("lines are 1-based — @0 is not a line".to_string());
        }
        let col = match parts.next() {
            Some(c) => {
                let c = c.parse::<usize>().map_err(|_| {
                    format!("@line:col must be positive integers, got {spec:?}")
                })?;
                if c == 0 {
                    return Err("columns are 1-based — :0 is not a column".to_string());
                }
                Some(c)
            }
            None => None,
        };
        Ok(BreakSpec::Line(line, col))
    } else {
        let op = spec.parse::<usize>().map_err(|_| {
            format!("bad breakpoint spec {spec:?} — use an op index (42) or @line[:col]")
        })?;
        Ok(BreakSpec::Op(op))
    }
}

/// `set cell[3] = 65` / `set cell 3 = 65`, or `set ptr = 3`.
fn parse_set(tokens: &[&str]) -> Result<Command, String> {
    let usage = "usage: set cell[<n>] = <v> | set ptr = <n>".to_string();
    let Some(first) = tokens.first().copied() else {
        return Err(usage);
    };

    // Bracketed form first: `set cell[3] = 65`.
    if let Some(idx) = bracketed_cell(first) {
        if tokens.get(1).copied() != Some("=") {
            return Err(usage);
        }
        let t = tokens.get(2).copied().ok_or_else(|| usage.clone())?;
        let v = t
            .parse::<u8>()
            .map_err(|_| format!("cell values are 0..=255, got {t:?}"))?;
        return Ok(Command::SetCell(idx, v));
    }

    match first {
        "ptr" => {
            if tokens.get(1).copied() != Some("=") {
                return Err(usage);
            }
            let t = tokens.get(2).copied().ok_or_else(|| usage.clone())?;
            let n = t
                .parse::<usize>()
                .map_err(|_| format!("ptr must be a non-negative integer, got {t:?}"))?;
            Ok(Command::SetPtr(n))
        }
        "cell" => {
            let idx_t = tokens.get(1).copied().ok_or_else(|| usage.clone())?;
            let idx = idx_t
                .parse::<usize>()
                .map_err(|_| format!("cell index must be a non-negative integer, got {idx_t:?}"))?;
            if tokens.get(2).copied() != Some("=") {
                return Err(usage);
            }
            let t = tokens.get(3).copied().ok_or_else(|| usage.clone())?;
            let v = t
                .parse::<u8>()
                .map_err(|_| format!("cell values are 0..=255, got {t:?}"))?;
            Ok(Command::SetCell(idx, v))
        }
        _ => Err(usage),
    }
}

/// `cell[3]` -> Some(3).
fn bracketed_cell(token: &str) -> Option<usize> {
    let rest = token.strip_prefix("cell[")?;
    let idx = rest.strip_suffix(']')?;
    idx.parse().ok()
}

// ---- the debugger ---------------------------------------------------------

pub struct Debugger {
    ops: Vec<Op>,
    srcmap: Vec<Pos>,
    source_lines: Vec<String>,
    tape: Vec<u8>,
    ptr: usize,
    pc: usize,
    steps: u64,
    undo: VecDeque<UndoRecord>,
    undo_limit: usize,
    undo_truncated: bool,
    breaks: BTreeSet<usize>,
    watch_ptr: Option<usize>,
    watch_cell: Option<usize>,
    input: VecDeque<u8>,
    finished: bool,
    last_error: Option<String>,
    /// The op index `run` last stopped on for a breakpoint; resuming from
    /// it must not re-trigger the same breakpoint before executing.
    resume_break: Option<usize>,
}

impl Debugger {
    pub fn new(ops: Vec<Op>, source: String, undo_limit: usize) -> Self {
        let srcmap = build_srcmap(&source);
        let source_lines = source.split('\n').map(str::to_string).collect();
        Debugger {
            srcmap,
            source_lines,
            tape: vec![0u8; INITIAL_TAPE_SIZE],
            ops,
            ptr: 0,
            pc: 0,
            steps: 0,
            undo: VecDeque::new(),
            undo_limit,
            undo_truncated: false,
            breaks: BTreeSet::new(),
            watch_ptr: None,
            watch_cell: None,
            input: VecDeque::new(),
            finished: false,
            last_error: None,
            resume_break: None,
        }
    }

    // ---- state queries ----

    pub fn op_count(&self) -> usize {
        self.ops.len()
    }
    pub fn pc(&self) -> usize {
        self.pc
    }
    pub fn steps(&self) -> u64 {
        self.steps
    }
    pub fn tape_len(&self) -> usize {
        self.tape.len()
    }
    pub fn is_finished(&self) -> bool {
        self.finished
    }
    /// Whether the undo log is on at all (`--undo-limit 0` disables it).
    pub fn undo_enabled(&self) -> bool {
        self.undo_limit != 0
    }
    /// Value of a cell, zero for anything past the current tape end —
    /// cells are conceptually an infinite field of zeros.
    pub fn cell_at(&self, n: usize) -> u8 {
        self.tape.get(n).copied().unwrap_or(0)
    }

    /// Label like `Add(3)` for the op at `i` (or the next op).
    pub fn op_label(&self, i: usize) -> String {
        op_label(self.ops.get(i))
    }

    /// Position of op `i` in the source, if it exists.
    pub fn pos_of(&self, i: usize) -> Option<Pos> {
        self.srcmap.get(i).copied()
    }

    // ---- breakpoints & watches ----

    pub fn add_break(&mut self, idx: usize) {
        self.breaks.insert(idx);
    }
    pub fn clear_break(&mut self, idx: usize) -> bool {
        self.breaks.remove(&idx)
    }
    pub fn breaks(&self) -> Vec<usize> {
        self.breaks.iter().copied().collect()
    }

    pub fn set_watch_ptr(&mut self, n: usize) {
        self.watch_ptr = Some(n);
    }
    pub fn set_watch_cell(&mut self, n: usize) {
        self.watch_cell = Some(n);
    }
    pub fn clear_watch_ptr(&mut self) -> bool {
        self.watch_ptr.take().is_some()
    }
    pub fn clear_watch_cell(&mut self) -> bool {
        self.watch_cell.take().is_some()
    }
    pub fn watch_ptr_target(&self) -> Option<usize> {
        self.watch_ptr
    }
    pub fn watch_cell_target(&self) -> Option<usize> {
        self.watch_cell
    }

    /// Resolves a [`BreakSpec`] against the source map: op index bounds,
    /// or the first op on the given line (and column, if given).
    pub fn resolve_break(&self, spec: &BreakSpec) -> Result<usize, String> {
        match spec {
            BreakSpec::Op(i) => {
                if *i < self.ops.len() {
                    Ok(*i)
                } else {
                    Err(format!(
                        "program has {} ops — there is no op {}",
                        self.ops.len(),
                        i
                    ))
                }
            }
            BreakSpec::Line(line, col) => {
                let wanted = *col;
                for (i, pos) in self.srcmap.iter().enumerate() {
                    if pos.line == *line && wanted.map_or(true, |c| pos.col == c) {
                        return Ok(i);
                    }
                }
                match wanted {
                    Some(c) => Err(format!("no instruction at line {line}, col {c}")),
                    None => Err(format!("no instruction on line {line}")),
                }
            }
        }
    }

    // ---- input & editing ----

    /// Appends bytes to the program's `,` queue; returns the queue length.
    pub fn feed(&mut self, bytes: Vec<u8>) -> usize {
        self.input.extend(bytes);
        self.input.len()
    }

    /// Preloads the `,` queue from a file's bytes.
    pub fn feed_stdin(&mut self, bytes: &[u8]) -> usize {
        self.feed(bytes.to_vec())
    }

    pub fn set_cell(&mut self, idx: usize, val: u8) -> Result<(), String> {
        if idx >= self.tape.len() {
            self.grow_to_at_least(idx)?;
        }
        self.tape[idx] = val;
        self.resume_break = None; // state changed: re-arm breakpoints
        Ok(())
    }

    pub fn set_ptr(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.tape.len() {
            self.grow_to_at_least(idx)?;
        }
        self.ptr = idx;
        self.resume_break = None;
        Ok(())
    }

    // ---- execution ----

    /// Executes one op. Breakpoints are *not* consulted — `step` steps
    /// through them (gdb semantics); only `run` stops at them.
    pub fn step(&mut self, prog_out: &mut dyn Write) -> Stop {
        self.resume_break = None;
        if self.pc >= self.ops.len() {
            self.finished = true;
            return Stop::Finished;
        }
        let pre = self.watch_pre();
        if let Err(msg) = self.execute_one(prog_out) {
            self.last_error = Some(msg.clone());
            return Stop::Errored(msg);
        }
        if let Some(stop) = self.check_watches(pre) {
            return stop;
        }
        if self.pc >= self.ops.len() {
            self.finished = true;
            return Stop::Finished;
        }
        Stop::Ran
    }

    /// Runs until a stop: breakpoint, watchpoint, runtime error, or end.
    /// Resuming from the breakpoint that just stopped us executes past it
    /// first instead of immediately re-triggering.
    pub fn run(&mut self, prog_out: &mut dyn Write) -> Stop {
        if self.pc >= self.ops.len() {
            self.finished = true;
            return Stop::Finished;
        }
        let mut skip = self.resume_break.take();
        loop {
            if self.pc >= self.ops.len() {
                self.finished = true;
                return Stop::Finished;
            }
            if skip != Some(self.pc) && self.breaks.contains(&self.pc) {
                self.resume_break = Some(self.pc);
                return Stop::Break(self.pc);
            }
            let pre = self.watch_pre();
            if let Err(msg) = self.execute_one(prog_out) {
                self.last_error = Some(msg.clone());
                return Stop::Errored(msg);
            }
            if let Some(stop) = self.check_watches(pre) {
                return stop;
            }
            skip = None; // only the first op may skip the breakpoint
        }
    }

    /// Rewinds up to `n` ops through the undo log; returns how many
    /// actually rewound. Clears any error/finished state.
    pub fn back(&mut self, n: usize) -> usize {
        self.resume_break = None;
        let mut done = 0;
        for _ in 0..n {
            let Some(rec) = self.undo.pop_back() else {
                break;
            };
            self.pc = rec.pc;
            self.ptr = rec.ptr;
            self.tape[rec.ptr] = rec.cell;
            if rec.counted {
                self.steps = self.steps.saturating_sub(1);
            }
            self.finished = false;
            self.last_error = None;
            done += 1;
        }
        done
    }

    /// Pushes one undo record, enforcing the capacity limit.
    fn undo_push(&mut self, rec: UndoRecord) {
        if self.undo_limit == 0 {
            return;
        }
        self.undo.push_back(rec);
        while self.undo.len() > self.undo_limit {
            self.undo.pop_front();
            self.undo_truncated = true;
        }
    }

    /// The pre-op snapshot watchpoints compare against.
    fn watch_pre(&self) -> u8 {
        self.watch_cell.map(|n| self.cell_at(n)).unwrap_or(0)
    }

    fn check_watches(&self, pre: u8) -> Option<Stop> {
        if let Some(n) = self.watch_ptr {
            if self.ptr == n {
                return Some(Stop::WatchPtr(n));
            }
        }
        if let Some(n) = self.watch_cell {
            let now = self.cell_at(n);
            if now != pre {
                return Some(Stop::WatchCell {
                    cell: n,
                    from: pre,
                    to: now,
                });
            }
        }
        None
    }

    /// Executes exactly one op with `bfinterp`'s semantics, after pushing
    /// the undo record. A failing op leaves state untouched (underflow is
    /// checked before the move, growth before the write) — the record
    /// still stands with `counted = false`, so `back` can rewind a failed
    /// attempt and `run` re-executes it, which is how "edit the tape and
    /// retry" works.
    fn execute_one(&mut self, prog_out: &mut dyn Write) -> Result<(), String> {
        let (pc, ptr, cell) = (self.pc, self.ptr, self.tape[self.ptr]);
        let op = self.ops[pc];
        let result = self.perform(op, prog_out);
        let counted = result.is_ok();
        self.undo_push(UndoRecord { pc, ptr, cell, counted });
        if counted {
            self.pc += 1;
            self.steps += 1;
        }
        result
    }

    /// Performs one op; the caller owns pc advancement and bookkeeping.
    fn perform(&mut self, op: Op, prog_out: &mut dyn Write) -> Result<(), String> {
        match op {
            Op::Add(n) => self.tape[self.ptr] = self.tape[self.ptr].wrapping_add(n),
            Op::Sub(n) => self.tape[self.ptr] = self.tape[self.ptr].wrapping_sub(n),
            Op::MoveRight(n) => {
                self.ptr += n as usize;
                if self.ptr >= self.tape.len() {
                    self.grow_to_at_least(self.ptr)?;
                }
            }
            Op::MoveLeft(n) => {
                self.ptr = self
                    .ptr
                    .checked_sub(n as usize)
                    .ok_or_else(|| "pointer moved left of cell 0".to_string())?;
            }
            Op::Output => {
                prog_out
                    .write_all(&[self.tape[self.ptr]])
                    .map_err(|e| format!("I/O error: {e}"))?;
                prog_out
                    .flush()
                    .map_err(|e| format!("I/O error: {e}"))?;
            }
            Op::Input => {
                // Empty queue is EOF: the cell is left untouched, the
                // shared convention.
                if let Some(byte) = self.input.pop_front() {
                    self.tape[self.ptr] = byte;
                }
            }
            Op::JumpIfZero { target } => {
                if self.tape[self.ptr] == 0 {
                    self.pc = target as usize;
                }
            }
            Op::JumpIfNonZero { target } => {
                if self.tape[self.ptr] != 0 {
                    self.pc = target as usize;
                }
            }
            Op::Zero => self.tape[self.ptr] = 0,
            Op::MulAdd { offset, factor } => {
                // Only reachable if someone constructs a Debugger over
                // optimized ops; kept identical to bfinterp for that case.
                let value = self.tape[self.ptr];
                if value != 0 {
                    let target = self.offset_cell(offset)?;
                    if target >= self.tape.len() {
                        self.grow_to_at_least(target)?;
                    }
                    self.tape[target] = self.tape[target]
                        .wrapping_add(value.wrapping_mul(factor));
                }
            }
            Op::Scan { stride } => {
                while self.tape[self.ptr] != 0 {
                    let target = self.offset_cell(stride)?;
                    if target >= self.tape.len() {
                        self.grow_to_at_least(target)?;
                    }
                    self.ptr = target;
                }
            }
        }
        Ok(())
    }

    fn offset_cell(&self, offset: i32) -> Result<usize, String> {
        let new_cell = self.ptr as i64 + offset as i64;
        if new_cell < 0 {
            Err("pointer moved left of cell 0".to_string())
        } else {
            Ok(new_cell as usize)
        }
    }

    /// `bfinterp`'s grow policy: grow to exactly cover the index, and
    /// fail cleanly instead of aborting on absurd sizes.
    fn grow_to_at_least(&mut self, index: usize) -> Result<(), String> {
        debug_assert!(index >= self.tape.len());
        let additional = index + 1 - self.tape.len();
        self.tape
            .try_reserve(additional)
            .map_err(|_| "failed to grow tape".to_string())?;
        self.tape.resize(index + 1, 0);
        Ok(())
    }

    // ---- views ----

    /// A window of cells around `center` (default: the pointer) with
    /// decimal, hex, and ASCII columns; the pointer row is marked `>`.
    pub fn tape_view(&self, center: Option<usize>, count: usize) -> String {
        let center = center.unwrap_or(self.ptr);
        if center >= self.tape.len() {
            return format!(
                "cell {center} is past the end of the tape ({} cells) — ptr is at {}\n",
                self.tape.len(),
                self.ptr
            );
        }
        let start = center.saturating_sub(count / 2).min(self.tape.len() - 1);
        let end = (start + count).min(self.tape.len());
        let mut s = format!(
            "tape: {} cells, ptr at {} (showing {}..{})\n",
            self.tape.len(),
            self.ptr,
            start,
            end.saturating_sub(1)
        );
        s.push_str(&format!(
            "      {:>8}  {:>3}  {:>4}  chr\n",
            "idx", "dec", "hex"
        ));
        for i in start..end {
            let v = self.tape[i];
            let marker = if i == self.ptr { '>' } else { ' ' };
            let chr = if (32..=126).contains(&v) {
                (v as char).to_string()
            } else {
                ".".to_string()
            };
            s.push_str(&format!(
                "{marker} {:>8}  {:>3}  {:>4x}  {}\n",
                i, v, v, chr
            ));
        }
        s
    }

    /// The source window around the next op, with a caret marking it.
    /// `context` lines are shown above and below its line.
    pub fn source_view(&self, context: usize) -> String {
        if self.pc >= self.ops.len() {
            return format!(
                "program finished after {} steps — no next instruction\n",
                self.steps
            );
        }
        let pos = self.srcmap[self.pc];
        let mut s = format!(
            "next: op {}/{} — line {}, col {} — {}\n",
            self.pc + 1,
            self.ops.len(),
            pos.line,
            pos.col,
            self.op_label(self.pc)
        );
        if self.last_error.is_some() {
            s.push_str("note: this op failed last time — `back` rewinds it, `set` patches state\n");
        }
        let ln = pos.line - 1; // 0-based line index
        let first = ln.saturating_sub(context);
        let last = (ln + context).min(self.source_lines.len().saturating_sub(1));
        let numw = self.source_lines.len().to_string().len().max(3);
        for i in first..=last {
            s.push_str(&format!("{:>numw$} | {}\n", i + 1, self.source_lines[i]));
            if i == ln {
                s.push_str(&format!(
                    "{:>numw$} | {}^\n",
                    "",
                    " ".repeat(pos.col - 1)
                ));
            }
        }
        s
    }

    /// The status block `info` prints.
    pub fn info(&self) -> String {
        let mut s = String::new();
        let next = if self.pc < self.ops.len() {
            let pos = self.srcmap[self.pc];
            format!(
                "op {}/{} — line {}, col {} — {}",
                self.pc + 1,
                self.ops.len(),
                pos.line,
                pos.col,
                self.op_label(self.pc)
            )
        } else {
            format!("past the last op ({} total)", self.ops.len())
        };
        s.push_str(&format!("next:    {}\n", next));
        s.push_str(&format!(
            "machine: ptr {}, tape {} cells, {} steps executed\n",
            self.ptr,
            self.tape.len(),
            self.steps
        ));
        let undo = if self.undo_limit == 0 {
            "disabled (--undo-limit 0)".to_string()
        } else {
            format!(
                "{} entries (limit {})",
                self.undo.len(),
                self.undo_limit
            )
        };
        s.push_str(&format!("undo:    {}\n", undo));
        if self.undo_truncated {
            s.push_str("note:    undo history hit its limit — oldest steps were dropped\n");
        }
        let state = if let Some(err) = &self.last_error {
            format!("errored — runtime error: {err}")
        } else if self.finished {
            "finished".to_string()
        } else if self.steps == 0 {
            "at the start (nothing executed yet)".to_string()
        } else {
            "paused".to_string()
        };
        s.push_str(&format!("state:   {}\n", state));
        s
    }
}

/// One-line op label for display.
fn op_label(op: Option<&Op>) -> String {
    match op {
        Some(Op::Add(n)) => format!("Add({n})"),
        Some(Op::Sub(n)) => format!("Sub({n})"),
        Some(Op::MoveRight(n)) => format!("MoveRight({n})"),
        Some(Op::MoveLeft(n)) => format!("MoveLeft({n})"),
        Some(Op::Output) => "Output".to_string(),
        Some(Op::Input) => "Input".to_string(),
        Some(Op::JumpIfZero { .. }) => "JumpIfZero".to_string(),
        Some(Op::JumpIfNonZero { .. }) => "JumpIfNonZero".to_string(),
        Some(Op::Zero) => "Zero".to_string(),
        Some(Op::MulAdd { offset, factor }) => format!("MulAdd({offset}, {factor})"),
        Some(Op::Scan { stride }) => format!("Scan({stride})"),
        None => "(no op)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bfc::parser;

    fn dbg_for(source: &str) -> Debugger {
        let ops = parser::parse(source).unwrap();
        Debugger::new(ops, source.to_string(), DEFAULT_UNDO_LIMIT)
    }

    // ---- source map ----

    #[test]
    fn srcmap_maps_ops_to_source_positions() {
        // Line 1 is a comment with no BF characters; line 2 has the ops.
        let d = dbg_for("x\n++.\n");
        assert_eq!(d.op_count(), 3);
        assert_eq!(d.pos_of(0), Some(Pos { line: 2, col: 1 }));
        assert_eq!(d.pos_of(1), Some(Pos { line: 2, col: 2 }));
        assert_eq!(d.pos_of(2), Some(Pos { line: 2, col: 3 }));
        assert_eq!(d.pos_of(3), None);
    }

    #[test]
    fn srcmap_follows_the_parser_on_comments_and_newlines() {
        // Non-command characters are comments — `~` is not a BF op, so
        // only the line-2 moves count, exactly as the parser walks them.
        let d = dbg_for("a~b\n>>>c\n");
        assert_eq!(d.op_count(), 3);
        assert_eq!(d.pos_of(0), Some(Pos { line: 2, col: 1 }));
        assert_eq!(d.pos_of(2), Some(Pos { line: 2, col: 3 }));
    }

    // ---- execution ----

    #[test]
    fn steps_execute_and_output_bytes() {
        let mut d = dbg_for("+.");
        let mut out = Vec::new();
        assert_eq!(d.step(&mut out), Stop::Ran);
        assert_eq!(d.cell_at(0), 1);
        assert_eq!(d.step(&mut out), Stop::Finished);
        assert_eq!(out, vec![1u8]);
        assert_eq!(d.steps(), 2);
    }

    #[test]
    fn wraparound_is_bfinterp_semantics() {
        let mut d = dbg_for(&"-".repeat(1));
        let mut out = Vec::new();
        d.step(&mut out);
        assert_eq!(d.cell_at(0), 255);
    }

    #[test]
    fn tape_grows_to_exactly_the_cell_landed_on() {
        let source = ">".repeat(30_000);
        let mut d = dbg_for(&source);
        let mut out = Vec::new();
        for _ in 0..30_000 {
            d.step(&mut out);
        }
        assert_eq!(d.tape_len(), 30_001);
        // The new cell is zero and writable.
        assert_eq!(d.cell_at(30_000), 0);
    }

    #[test]
    fn move_left_underflow_is_a_clean_error_that_leaves_state_alone() {
        let mut d = dbg_for("<");
        let mut out = Vec::new();
        assert_eq!(d.step(&mut out), Stop::Errored("pointer moved left of cell 0".into()));
        assert_eq!(d.pc(), 0); // pc still points at the failed op
        assert_eq!(d.steps(), 0);
    }

    #[test]
    fn jumps_follow_bracket_pairing_with_the_shared_off_by_one() {
        // +[-] run to completion: Add, JZ(not taken), Sub, JNZ(not taken).
        let mut d = dbg_for("+[-]");
        let mut out = Vec::new();
        assert_eq!(d.run(&mut out), Stop::Finished);
        assert_eq!(d.cell_at(0), 0);
        assert_eq!(d.steps(), 4);
    }

    // ---- time travel ----

    #[test]
    fn back_rewinds_one_op_and_restores_the_cell() {
        let mut d = dbg_for("++");
        let mut out = Vec::new();
        d.run(&mut out);
        assert_eq!(d.cell_at(0), 2);
        assert_eq!(d.back(1), 1);
        assert_eq!(d.cell_at(0), 1);
        assert_eq!(d.pc(), 1);
        assert_eq!(d.steps(), 1);
    }

    #[test]
    fn back_restores_pointer_and_position() {
        let mut d = dbg_for(">+<");
        let mut out = Vec::new();
        d.run(&mut out);
        // After: ptr 0, tape[1] = 1.
        assert_eq!(d.back(1), 1);
        assert_eq!(d.pc(), 2); // the `<` op again
        // One more back lands before the Add, restoring tape[1] = 0.
        assert_eq!(d.back(1), 1);
        assert_eq!(d.cell_at(1), 0);
        // A third back rewinds the MoveRight itself.
        assert_eq!(d.back(1), 1);
        assert_eq!(d.pc(), 0);
        // Nothing left.
        assert_eq!(d.back(1), 0);
    }

    #[test]
    fn back_rewinds_an_entire_loop_run() {
        // ++[-]: 7 ops execute (two Adds, then the loop body twice).
        let mut d = dbg_for("++[-]");
        let mut out = Vec::new();
        d.run(&mut out);
        assert_eq!(d.steps(), 7);
        assert_eq!(d.back(7), 7);
        assert_eq!(d.pc(), 0);
        assert_eq!(d.cell_at(0), 0);
        assert_eq!(d.steps(), 0);
        assert!(!d.is_finished());
    }

    #[test]
    fn back_out_of_an_error_lets_a_patched_state_retry() {
        // The flagship workflow: underflow -> rewind -> patch -> succeed.
        let mut d = dbg_for("<");
        let mut out = Vec::new();
        assert!(matches!(d.run(&mut out), Stop::Errored(_)));
        assert_eq!(d.back(1), 1); // clears the error state
        d.set_ptr(2).unwrap(); // now `<` is legal
        assert_eq!(d.run(&mut out), Stop::Finished);
        assert_eq!(d.steps(), 1);
    }

    #[test]
    fn undo_limit_zero_disables_back_entirely() {
        let ops = parser::parse("+").unwrap();
        let mut d = Debugger::new(ops, "+".into(), 0);
        let mut out = Vec::new();
        d.step(&mut out);
        assert!(!d.undo_enabled());
        assert_eq!(d.back(1), 0);
    }

    #[test]
    fn undo_limit_drops_the_oldest_entries() {
        let ops = parser::parse("+++++").unwrap();
        let mut d = Debugger::new(ops, "+++++".into(), 3);
        let mut out = Vec::new();
        d.run(&mut out);
        assert_eq!(d.steps(), 5);
        // Only the last 3 survive, so only 3 can rewind.
        assert_eq!(d.back(10), 3);
        assert_eq!(d.cell_at(0), 2);
    }

    // ---- breakpoints ----

    #[test]
    fn breakpoints_stop_run_before_the_op_executes() {
        let mut d = dbg_for("+++.");
        d.add_break(2);
        let mut out = Vec::new();
        assert_eq!(d.run(&mut out), Stop::Break(2));
        assert_eq!(d.cell_at(0), 2); // two Adds ran, the third didn't
        // `step` walks straight through the breakpoint.
        assert_eq!(d.step(&mut out), Stop::Ran);
        assert_eq!(d.cell_at(0), 3);
        assert_eq!(d.run(&mut out), Stop::Finished);
        assert_eq!(out, vec![3u8]);
    }

    #[test]
    fn resuming_from_a_breakpoint_does_not_retrigger_it() {
        let mut d = dbg_for("+[>+<-]");
        d.add_break(1); // the loop head
        let mut out = Vec::new();
        assert_eq!(d.run(&mut out), Stop::Break(1));
        // Resume: the whole loop runs to completion without stopping at
        // op 1 again — one pass, cell 0 counts down to 0.
        assert_eq!(d.run(&mut out), Stop::Finished);
    }

    #[test]
    fn a_loop_revisit_retriggers_the_breakpoint() {
        // Two iterations (cell 0 counts 2 down to 0), so op 3 — the
        // MoveRight, hit every iteration — triggers on both passes.
        let mut d = dbg_for("++[>+<-]");
        d.add_break(3);
        let mut out = Vec::new();
        assert_eq!(d.run(&mut out), Stop::Break(3));
        assert_eq!(d.run(&mut out), Stop::Break(3)); // looped back to it
        assert_eq!(d.run(&mut out), Stop::Finished);   // one more pass, done
    }

    // ---- watchpoints ----

    #[test]
    fn watch_ptr_stops_when_the_pointer_reaches_the_cell() {
        let mut d = dbg_for(">>>");
        d.set_watch_ptr(2);
        let mut out = Vec::new();
        assert_eq!(d.run(&mut out), Stop::WatchPtr(2));
        assert_eq!(d.steps(), 2);
    }

    #[test]
    fn watch_cell_fires_only_on_a_real_change() {
        let mut d = dbg_for("+>+");
        d.set_watch_cell(1);
        let mut out = Vec::new();
        // Two ops that don't touch cell 1 run through; the third fires.
        assert_eq!(d.run(&mut out), Stop::WatchCell { cell: 1, from: 0, to: 1 });
        assert_eq!(d.steps(), 3);
    }

    #[test]
    fn watch_cell_beyond_the_tape_defaults_to_zero_then_grows() {
        // Cell 30000 starts conceptually-zero (past the tape end); the
        // move grows the tape without changing its value (no trigger),
        // then `+` writes it and the watch fires 0 -> 1.
        let source = format!("{}+", ">".repeat(30_000));
        let mut d = dbg_for(&source);
        d.set_watch_cell(30_000);
        let mut out = Vec::new();
        assert_eq!(
            d.run(&mut out),
            Stop::WatchCell {
                cell: 30_000,
                from: 0,
                to: 1
            }
        );
        assert_eq!(d.tape_len(), 30_001);
    }

    #[test]
    fn watch_cell_on_a_untouched_cell_never_fires() {
        let mut d = dbg_for("+++.");
        d.set_watch_cell(5);
        let mut out = Vec::new();
        assert_eq!(d.run(&mut out), Stop::Finished);
        assert_eq!(out, vec![3u8]);
    }

    // ---- input ----

    #[test]
    fn input_reads_the_fed_queue_and_eof_leaves_the_cell() {
        let mut d = dbg_for(",.");
        let mut out = Vec::new();
        // Empty queue: EOF, cell stays 0.
        assert_eq!(d.run(&mut out), Stop::Finished);
        assert_eq!(out, vec![0u8]);

        let mut d = dbg_for(",.");
        d.feed(vec![65]);
        let mut out = Vec::new();
        assert_eq!(d.run(&mut out), Stop::Finished);
        assert_eq!(out, vec![65u8]);
    }

    // ---- resolve & views ----

    #[test]
    fn resolve_break_handles_ops_lines_and_columns() {
        let d = dbg_for("++\n--");
        assert_eq!(d.resolve_break(&BreakSpec::Op(2)), Ok(2));
        assert_eq!(d.resolve_break(&BreakSpec::Line(2, None)), Ok(2));
        assert_eq!(d.resolve_break(&BreakSpec::Line(2, Some(2))), Ok(3));
        assert!(d.resolve_break(&BreakSpec::Op(99)).is_err());
        assert!(d.resolve_break(&BreakSpec::Line(9, None)).is_err());
    }

    #[test]
    fn tape_view_shows_dec_hex_ascii_and_marks_ptr() {
        let mut d = dbg_for("+");
        let mut out = Vec::new();
        d.step(&mut out);
        d.set_cell(2, 65).unwrap();
        let view = d.tape_view(Some(0), 4);
        assert!(view.contains("tape:"), "{view}");
        assert!(view.contains("idx"), "{view}");
        // The ptr row is marked; cell 0 holds 1 (dec/hex/chr columns).
        assert!(view.contains("> "), "{view}");
        assert!(view.contains("1"), "{view}");
        // Cell 2 holds 65 -> hex 41, ASCII 'A'.
        assert!(view.contains("41"), "{view}");
        assert!(view.contains("A"), "{view}");
    }

    #[test]
    fn tape_view_beyond_the_tape_says_so() {
        let d = dbg_for("+");
        let view = d.tape_view(Some(50_000), 4);
        assert!(view.contains("past the end of the tape"), "{view}");
    }

    #[test]
    fn source_view_carets_the_next_op() {
        let d = dbg_for("++\n++\n");
        let view = d.source_view(1);
        assert!(view.contains("next: op 1/4 — line 1, col 1 — Add(1)"), "{view}");
        assert!(view.contains("  1 | ++"), "{view}");
        assert!(view.contains("  2 | ++"), "{view}");
        assert!(view.contains(" | ^"), "{view}");
    }

    #[test]
    fn source_view_when_finished_says_so() {
        let mut d = dbg_for("+");
        let mut out = Vec::new();
        d.run(&mut out);
        let view = d.source_view(1);
        assert!(view.contains("program finished"), "{view}");
    }

    #[test]
    fn info_reports_machine_state_and_progress() {
        let mut d = dbg_for("++.");
        let mut out = Vec::new();
        let info = d.info();
        assert!(info.contains("at the start"), "{info}");
        d.step(&mut out);
        let info = d.info();
        assert!(info.contains("op 2/3"), "{info}");
        assert!(info.contains("1 steps executed"), "{info}");
        assert!(info.contains("paused"), "{info}");
        assert!(info.contains("limit 1048576"), "{info}");
    }

    // ---- command parsing ----

    #[test]
    fn parse_command_covers_the_whole_surface() {
        use Command::*;
        assert_eq!(parse_command("run"), Ok(Run));
        assert_eq!(parse_command("c"), Ok(Run));
        assert_eq!(parse_command("s"), Ok(Step(1)));
        assert_eq!(parse_command("step 3"), Ok(Step(3)));
        assert_eq!(parse_command("b"), Ok(Back(1)));
        assert_eq!(parse_command("back 2"), Ok(Back(2)));
        assert_eq!(parse_command("t"), Ok(Tape(None, 16)));
        assert_eq!(parse_command("tape 5"), Ok(Tape(Some(5), 16)));
        assert_eq!(parse_command("tape 5 8"), Ok(Tape(Some(5), 8)));
        assert_eq!(parse_command("l"), Ok(List(2)));
        assert_eq!(parse_command("list 4"), Ok(List(4)));
        assert_eq!(parse_command("break"), Ok(Break(None)));
        assert_eq!(
            parse_command("break 42"),
            Ok(Break(Some(BreakSpec::Op(42))))
        );
        assert_eq!(
            parse_command("break @3"),
            Ok(Break(Some(BreakSpec::Line(3, None))))
        );
        assert_eq!(
            parse_command("break @3:5"),
            Ok(Break(Some(BreakSpec::Line(3, Some(5)))))
        );
        assert_eq!(
            parse_command("watch ptr 5"),
            Ok(Watch(Some(WatchSpec::Ptr(5))))
        );
        assert_eq!(
            parse_command("watch cell 10"),
            Ok(Watch(Some(WatchSpec::Cell(10))))
        );
        assert_eq!(parse_command("watch"), Ok(Watch(None)));
        assert_eq!(parse_command("unwatch ptr"), Ok(Unwatch(Which::Ptr)));
        assert_eq!(parse_command("set cell[3] = 65"), Ok(SetCell(3, 65)));
        assert_eq!(parse_command("set cell 3 = 65"), Ok(SetCell(3, 65)));
        assert_eq!(parse_command("set ptr = 4"), Ok(SetPtr(4)));
        assert_eq!(parse_command("feed 65 10"), Ok(Feed(vec![65, 10])));
        assert_eq!(parse_command("info"), Ok(Info));
        assert_eq!(parse_command("?"), Ok(Help));
        assert_eq!(parse_command("q"), Ok(Quit));
    }

    #[test]
    fn parse_command_rejects_the_bad_shapes() {
        assert!(parse_command("feed 300").is_err()); // not a byte
        assert!(parse_command("tape 5 0").is_err()); // count 0
        assert!(parse_command("break @0").is_err()); // lines are 1-based
        assert!(parse_command("break @3:x").is_err());
        assert!(parse_command("set cell[3] 65").is_err()); // missing =
        assert!(parse_command("set cell[3] = 300").is_err()); // not a byte
        assert!(parse_command("watch foo 3").is_err());
        assert!(parse_command("frobnicate").is_err());
        assert!(parse_command("clear").is_err());
    }
}
