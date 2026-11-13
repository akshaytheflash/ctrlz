mod cli;
mod ignore;
mod manifest;
mod snapshot;
mod store;
mod util;

fn main() {
    if let Err(error) = cli::run() {
        eprintln!("ctrlz: {error:#}");
        std::process::exit(1);
    }
}
