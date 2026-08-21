pub mod actions;
pub mod coordinator;
pub mod index;
pub mod model;
pub mod projection;
pub mod query;
pub mod reconciliation;
pub mod scanner;
pub mod scheduler;
pub mod volume;

#[cfg(target_os = "macos")]
pub mod fsevents;

#[cfg(target_os = "macos")]
mod macos;

pub fn run() {
    #[cfg(target_os = "macos")]
    macos::run();

    #[cfg(not(target_os = "macos"))]
    eprintln!("Everyfile requires macOS 15 or later on Apple Silicon.");
}
