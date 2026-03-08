use clap::Parser;
use std::error::Error;

use meerkat_lib::runtime;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(short = 'f', long = "file", default_value = "test0.meerkat")]
    input_file: String,

    #[arg(short = 'v', long = "verbose", default_value_t = false)]
    verbose: bool,
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    let log_level = if args.verbose {
        log::LevelFilter::Info
    } else {
        log::LevelFilter::Warn
    };
    env_logger::Builder::from_default_env()
        .filter_level(log_level)
        .init();

    let prog = meerkat_lib::runtime::parser::parser::parse_file(&args.input_file)
        .map_err(|e| format!("Parse error: {}", e))?;

    runtime::run(&prog).await?;

    Ok(())
}
