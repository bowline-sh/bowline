#![deny(unsafe_code)]

mod daemon;

fn main() -> std::process::ExitCode {
    daemon::entrypoint()
}
