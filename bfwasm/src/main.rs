fn main() -> std::process::ExitCode {
    bfwasm::cli::run(std::env::args().collect())
}
