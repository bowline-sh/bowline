use std::io::{self, Write};

use bowline_release_durability::Invocation;

fn main() {
    let output = Invocation::parse(std::env::args_os().skip(1)).execute();
    let exit_code = if output.is_success() { 0 } else { 1 };
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    if serde_json::to_writer(&mut writer, &output).is_err() || writer.write_all(b"\n").is_err() {
        std::process::exit(70);
    }
    std::process::exit(exit_code);
}
