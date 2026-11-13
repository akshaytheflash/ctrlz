mod cli;
mod diff;
mod doctor;
mod hooks;
mod ignore;
mod manifest;
mod prune;
mod restore;
mod snapshot;
mod store;
mod util;
mod watch;

fn main() {
    if let Err(error) = cli::run() {
        eprintln!("ctrlz: {error:#}");
        std::process::exit(1);
    }
}
