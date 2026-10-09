#![forbid(unsafe_code)]
mod definition_admin;
mod operator;

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
