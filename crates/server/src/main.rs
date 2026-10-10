#![forbid(unsafe_code)]
mod artifact_worker;
mod candidate_worker;
mod definition_admin;
mod execution_worker;
mod operator;
mod reconciliation_admin;
mod runtime_admin;
mod volume_worker;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match operator::run(std::env::args().skip(1).collect()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", error.message);
            std::process::ExitCode::from(error.exit)
        }
    }
}
