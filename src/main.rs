use clap::Parser;

fn main() -> std::process::ExitCode {
    let args = gcv::cli::Args::parse();
    match gcv::cli::run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
