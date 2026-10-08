mod batch;
mod cli;
mod commands;
mod input;
mod install_prompt;
mod prompt;
mod render;
mod trim;

use clap::Parser;

fn main() {
    // Put the console into its final mode before anything is written to it.
    // On Windows this turns on virtual-terminal processing; everywhere else
    // it is a cached `true`. Doing it here rather than lazily at the first
    // styled write means indicatif -- which draws through the same console --
    // also sees the enabled mode from its very first frame (F163).
    render::ansi_supported();

    // `wild` parses the raw command line so globs work on Windows, where
    // neither cmd.exe nor PowerShell expands them for a native executable.
    let cli = cli::Cli::parse_from(wild::args_os());
    // Contradictions between flags, reported the way clap reports its own
    // usage errors: on stderr, exit 2.
    if let Err(msg) = cli.check() {
        use clap::CommandFactory;
        cli::Cli::command()
            .error(clap::error::ErrorKind::ArgumentConflict, msg)
            .exit();
    }
    let code = match &cli.command {
        None => commands::convert::run(&cli),
        Some(cli::Command::Doctor) => commands::doctor::run(&cli),
        Some(cli::Command::Capabilities { ref format }) => {
            commands::capabilities::run(&cli, format.as_deref())
        }
        Some(cli::Command::Install { backend }) => commands::install::run(&cli, backend),
        Some(cli::Command::Scan { paths }) => commands::scan::run(&cli, paths),
        Some(cli::Command::Update { check }) => commands::update::run(&cli, *check),
    };
    std::process::exit(code);
}
