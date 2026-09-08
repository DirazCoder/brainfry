mod op;
mod parser;
mod optimize;

use op::Op;

fn run(ops: &[Op], label: &str) {
    let mut tape = vec![0u8; 30000];
    let mut ptr: usize = 15000;
    let mut out: Vec<u8> = Vec::new();
    let mut ip: usize = 0;
    let mut steps: u64 = 0;

    while ip < ops.len() {
        steps += 1;
        if steps > 10_000_000 {
            println!("[{label}] too many steps, aborting");
            break;
        }
        match ops[ip] {
            Op::Add(n) => { tape[ptr] = tape[ptr].wrapping_add(n); ip += 1; }
            Op::Sub(n) => { tape[ptr] = tape[ptr].wrapping_sub(n); ip += 1; }
            Op::MoveRight(n) => { ptr += n as usize; ip += 1; }
            Op::MoveLeft(n) => { ptr -= n as usize; ip += 1; }
            Op::Output => { out.push(tape[ptr]); ip += 1; }
            Op::Input => { ip += 1; }
            Op::JumpIfZero { target } => {
                if tape[ptr] == 0 { ip = target as usize + 1; } else { ip += 1; }
            }
            Op::JumpIfNonZero { target } => {
                if tape[ptr] != 0 { ip = target as usize + 1; } else { ip += 1; }
            }
            Op::Zero => { tape[ptr] = 0; ip += 1; }
            Op::MulAdd { offset, factor } => {
                // fixed: no longer zeroes tape[ptr] -- that's the trailing
                // Zero op's job, since a group of MulAdds can share one
                // source cell.
                let target = (ptr as i64 + offset as i64) as usize;
                tape[target] = tape[target].wrapping_add(tape[ptr].wrapping_mul(factor));
                ip += 1;
            }
            Op::Scan { stride } => {
                while tape[ptr] != 0 {
                    ptr = (ptr as i64 + stride as i64) as usize;
                }
                ip += 1;
            }
        }
    }

    println!("[{label}] output: {:?}", String::from_utf8_lossy(&out));
}

fn main() {
    let src = "++++++++++[>+++++++>++++++++++>+++>+<<<<-]>++.>+.+++++++..+++.>++.<<+++++++++++++++.>.+++.------.--------.>+.>.";
    let ops = parser::parse(src).unwrap();
    println!("raw op count: {}", ops.len());
    run(&ops, "raw (no optimize)");

    let optimized = optimize::optimize(ops.clone());
    println!("optimized op count: {}", optimized.len());
    for (i, op) in optimized.iter().enumerate() {
        println!("{i}: {:?}", op);
    }
    run(&optimized, "optimized");
}