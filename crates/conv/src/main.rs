mod batch;
mod cli;
mod commands;
mod input;
mod render;

use clap::Parser;

fn main() {
    // `wild` parses the raw command line so globs work on Windows, where
    // neither cmd.exe nor PowerShell expands them for a native executable.
    let cli = cli::Cli::parse_from(wild::args_os());
    let code = match &cli.command {
        None => commands::convert::run(&cli),
        Some(_) => {
            eprintln!("error: subcommand not implemented yet");
            2
        }
    };
    std::process::exit(code);
}
