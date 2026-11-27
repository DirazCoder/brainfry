use std::fs;
use std::process::Command;

fn temp_dir() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("brainfry-cli-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn help_is_available_from_the_binary_entry_point() {
    let output = Command::new(env!("CARGO_BIN_EXE_brainfry"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("bfnative"));
}

#[test]
fn compiler_and_runner_share_files_through_the_dispatcher() {
    let dir = temp_dir();
    let source = dir.join("program.bf");
    let bytecode = dir.join("program.bfry");
    fs::write(&source, "+++.".as_bytes()).unwrap();

    let compiled = Command::new(env!("CARGO_BIN_EXE_brainfry"))
        .args(["bfc"])
        .arg(&source)
        .arg(&bytecode)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let executed = Command::new(env!("CARGO_BIN_EXE_brainfry"))
        .args(["bfrun"])
        .arg(&bytecode)
        .output()
        .unwrap();
    assert!(
        executed.status.success(),
        "{}",
        String::from_utf8_lossy(&executed.stderr)
    );
    assert_eq!(executed.stdout, [3]);

    fs::remove_dir_all(dir).unwrap();
}
