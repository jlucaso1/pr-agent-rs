use tracing_subscriber::EnvFilter;

mod ai;
mod cli;
mod config;
mod error;
mod git;
mod output;
mod processing;
mod server;
mod template;
mod tools;
mod util;

#[cfg(test)]
mod testing;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    if let Err(e) = cli::run().await {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
