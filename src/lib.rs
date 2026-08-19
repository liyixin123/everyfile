pub mod model;
pub mod scheduler;

#[cfg(target_os = "macos")]
mod macos;

pub fn run() {
    #[cfg(target_os = "macos")]
    macos::run();

    #[cfg(not(target_os = "macos"))]
    eprintln!("Everyfile requires macOS 15 or later on Apple Silicon.");
}
