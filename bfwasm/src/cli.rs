//! `bfwasm` — compiles a `.bf` file to a standalone WASI `.wasm` module or
//! a self-contained browser `.js` file. Same front end as every other backend
//! in this workspace
//! (`bfc::parser` + `bfc::optimize`), same "no external toolchain" rule
//! `bfnative` holds itself to: no `wat2wasm`, no `wasm-bindgen`, no
//! Emscripten. The module this writes runs as-is under any
//! WASI-preview1-capable runtime (`wasmtime run`, `wasmer run`, Node's
//! built-in `node:wasi` API, ...).

use crate::{browser, codegen};
use bfc::{optimize, parser};
use std::fs;
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: bfwasm [options] <input.bf>

options:
  --target <target>      output target: wasi (default) or browser
  -o, --output <file>    output path (default extension follows target)
  --split                write a browser .js loader and adjacent .wasm module

Compiles to a standalone WASI .wasm module by default. Use --target browser
to write a self-contained JavaScript file with an inline WASI shim. An output
path ending in .js also selects browser mode unless --target wasi is explicit.
Use --split with a browser target to write the .wasm module beside the loader.

Examples:
  bfwasm program.bf
  wasmtime run program.wasm
  bfwasm program.bf --target browser
  bfwasm program.bf -o program.js";

pub fn run(args: Vec<String>) -> ExitCode {
    if has_help_flag(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let (input, output, target, split) = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let source = match fs::read_to_string(&input) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("couldn't read {input}: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Same parse + optimize pipeline every other backend runs — see
    // bfnative's main.rs for why this is reused rather than reimplemented.
    let raw_ops = match parser::parse(&source) {
        Ok(ops) => ops,
        Err(err) => {
            eprintln!("{input}:{}: {}", err.line, err.message);
            return ExitCode::FAILURE;
        }
    };
    let ops = optimize::optimize(raw_ops);

    let module = codegen::compile(&ops);
    let output_path = match output {
        Some(output) => output,
        None => default_output_path(&input, target),
    };

    if split {
        let (wasm_output_path, wasm_filename) = match split_wasm_path(&output_path) {
            Ok(paths) => paths,
            Err(message) => {
                eprintln!("{message}\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(err) = fs::write(&wasm_output_path, &module) {
            eprintln!("couldn't write {wasm_output_path}: {err}");
            return ExitCode::FAILURE;
        }
        let output_bytes = browser::compile_split(&wasm_filename).into_bytes();
        if let Err(err) = fs::write(&output_path, &output_bytes) {
            eprintln!("couldn't write {output_path}: {err}");
            return ExitCode::FAILURE;
        }
        println!(
            "compiled {input} -> {output_path} and {wasm_output_path} ({} instructions, {} bytes)",
            ops.len(),
            output_bytes.len()
        );
        return ExitCode::SUCCESS;
    }

    let output_bytes = match target {
        Target::Wasi => module,
        Target::Browser => browser::compile(&module).into_bytes(),
    };

    if let Err(err) = fs::write(&output_path, &output_bytes) {
        eprintln!("couldn't write {output_path}: {err}");
        return ExitCode::FAILURE;
    }

    println!(
        "compiled {input} -> {output_path} ({} instructions, {} bytes)",
        ops.len(),
        output_bytes.len()
    );
    ExitCode::SUCCESS
}

fn has_help_flag(args: &[String]) -> bool {
    args.iter()
        .skip(1)
        .any(|arg| arg == "-h" || arg == "--help")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Wasi,
    Browser,
}

fn parse_target(value: &str) -> Result<Target, String> {
    match value {
        "wasi" => Ok(Target::Wasi),
        "browser" => Ok(Target::Browser),
        other => Err(format!("unknown target: {other}")),
    }
}

fn resolve_target(explicit: Option<Target>, output: Option<&str>) -> Target {
    explicit.unwrap_or_else(|| {
        if output.is_some_and(|path| {
            Path::new(path)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("js"))
        }) {
            Target::Browser
        } else {
            Target::Wasi
        }
    })
}

fn parse_args(args: Vec<String>) -> Result<(String, Option<String>, Target, bool), String> {
    let mut input: Option<String> = None;
    let mut output: Option<String> = None;
    let mut target: Option<Target> = None;
    let mut split = false;

    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--target" => {
                let value = match args.next() {
                    Some(value) => value,
                    None => return Err("--target needs a value".into()),
                };
                target = Some(parse_target(&value)?);
            }
            "--split" => split = true,
            "-o" | "--output" => {
                output = Some(args.next().ok_or("--output needs a value")?);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option: {other}"));
            }
            other => {
                if input.is_some() {
                    return Err("more than one input file given".to_string());
                }
                input = Some(other.to_string());
            }
        }
    }

    let input = input.ok_or("no input file given")?;
    let target = resolve_target(target, output.as_deref());
    if split && target != Target::Browser {
        return Err("--split requires the browser target".into());
    }
    Ok((input, output, target, split))
}

fn split_wasm_path(js_output_path: &str) -> Result<(String, String), String> {
    let js_path = Path::new(js_output_path);
    if js_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wasm"))
    {
        return Err("--split cannot use a .wasm loader path".into());
    }
    let wasm_path = js_path.with_extension("wasm");
    let wasm_filename = wasm_path
        .file_name()
        .ok_or("--split output path must include a filename")?
        .to_string_lossy()
        .into_owned();
    Ok((wasm_path.to_string_lossy().into_owned(), wasm_filename))
}

fn default_output_path(input_path: &str, target: Target) -> String {
    Path::new(input_path)
        .with_extension(match target {
            Target::Wasi => "wasm",
            Target::Browser => "js",
        })
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_flag_is_recognized() {
        assert!(has_help_flag(&["bfwasm".into(), "-h".into()]));
        assert!(has_help_flag(&["bfwasm".into(), "--help".into()]));
        assert!(!has_help_flag(&["bfwasm".into(), "program.bf".into()]));
    }

    #[test]
    fn target_names_are_validated() {
        assert_eq!(parse_target("wasi"), Ok(Target::Wasi));
        assert_eq!(parse_target("browser"), Ok(Target::Browser));
        assert_eq!(
            parse_target("unknown"),
            Err("unknown target: unknown".into())
        );
    }

    #[test]
    fn output_extension_selects_browser_unless_target_is_explicit() {
        assert_eq!(resolve_target(None, Some("program.js")), Target::Browser);
        assert_eq!(resolve_target(None, Some("program.JS")), Target::Browser);
        assert_eq!(resolve_target(None, Some("program.wasm")), Target::Wasi);
        assert_eq!(resolve_target(None, Some("program")), Target::Wasi);
        assert_eq!(resolve_target(None, None), Target::Wasi);
        assert_eq!(
            resolve_target(Some(Target::Wasi), Some("program.js")),
            Target::Wasi
        );
        assert_eq!(
            resolve_target(Some(Target::Browser), Some("program.wasm")),
            Target::Browser
        );
    }

    #[test]
    fn target_option_requires_a_value() {
        assert_eq!(
            parse_args(vec![
                "bfwasm".into(),
                "program.bf".into(),
                "--target".into()
            ]),
            Err("--target needs a value".into())
        );
    }

    #[test]
    fn target_option_parses_a_valid_value() {
        assert_eq!(
            parse_args(vec![
                "bfwasm".into(),
                "program.bf".into(),
                "--target".into(),
                "browser".into()
            ]),
            Ok(("program.bf".into(), None, Target::Browser, false))
        );
    }

    #[test]
    fn target_option_rejects_unknown_value() {
        assert_eq!(
            parse_args(vec![
                "bfwasm".into(),
                "program.bf".into(),
                "--target".into(),
                "unknown".into()
            ]),
            Err("unknown target: unknown".into())
        );
    }

    #[test]
    fn split_requires_browser_target_after_extension_inference() {
        assert_eq!(
            parse_args(vec![
                "bfwasm".into(),
                "program.bf".into(),
                "--target".into(),
                "browser".into(),
                "--split".into()
            ]),
            Ok(("program.bf".into(), None, Target::Browser, true))
        );
        assert_eq!(
            parse_args(vec![
                "bfwasm".into(),
                "program.bf".into(),
                "-o".into(),
                "program.js".into(),
                "--split".into()
            ]),
            Ok((
                "program.bf".into(),
                Some("program.js".into()),
                Target::Browser,
                true
            ))
        );
        assert_eq!(
            parse_args(vec![
                "bfwasm".into(),
                "program.bf".into(),
                "--target".into(),
                "wasi".into(),
                "--split".into()
            ]),
            Err("--split requires the browser target".into())
        );
    }

    #[test]
    fn split_wasm_path_uses_loader_stem_and_rejects_collision() {
        assert_eq!(
            split_wasm_path("dist/app.js"),
            Ok(("dist/app.wasm".into(), "app.wasm".into()))
        );
        assert_eq!(
            split_wasm_path("app.WASM"),
            Err("--split cannot use a .wasm loader path".into())
        );
        assert_eq!(
            split_wasm_path(std::path::MAIN_SEPARATOR_STR),
            Err("--split output path must include a filename".into())
        );
    }

    #[test]
    fn default_output_extension_follows_the_target() {
        assert_eq!(
            default_output_path("program.bf", Target::Wasi),
            "program.wasm"
        );
        assert_eq!(
            default_output_path("program.bf", Target::Browser),
            "program.js"
        );
    }
}
