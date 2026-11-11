use clap::Parser;
use console::style;
use sqlx_cli::Opt;
use std::process;

// cargo invokes this binary as `cargo-sqlx sqlx <args>`
// so the parser below is defined with that in mind
#[derive(Parser, Debug)]
#[clap(bin_name = "cargo")]
enum Cli {
    Sqlx(Opt),
}

#[tokio::main]
async fn main() {
    sqlx_cli::maybe_apply_dotenv();
    sqlx_cli::init_logging();

    sqlx::any::install_default_drivers();

    let Cli::Sqlx(opt) = Cli::parse();

    if let Err(error) = sqlx_cli::run(opt).await {
        println!("{} {}", style("error:").bold().red(), error);
        process::exit(1);
    }
}
