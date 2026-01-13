//! Shell completion generation
//!
//! Generate shell completion scripts for bash, zsh, fish, and powershell.

use clap::CommandFactory;
use clap_complete::{Generator, Shell};

use super::Cli;
use crate::exit_code::ExitCode;

/// Arguments for the completions command
#[derive(clap::Args, Debug)]
pub struct CompletionsArgs {
    /// Shell to generate completions for
    #[arg(value_enum)]
    pub shell: Shell,
}

/// Generate shell completions and print to stdout
pub fn execute(args: CompletionsArgs) -> ExitCode {
    let mut cmd = Cli::command();
    print_completions(args.shell, &mut cmd);
    ExitCode::Success
}

fn print_completions<G: Generator>(generator: G, cmd: &mut clap::Command) {
    clap_complete::generate(
        generator,
        cmd,
        cmd.get_name().to_string(),
        &mut std::io::stdout(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completions_bash() {
        let mut cmd = Cli::command();
        let mut buf = Vec::new();
        clap_complete::generate(Shell::Bash, &mut cmd, "rc", &mut buf);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("rc"));
        assert!(output.contains("complete"));
    }

    #[test]
    fn test_completions_zsh() {
        let mut cmd = Cli::command();
        let mut buf = Vec::new();
        clap_complete::generate(Shell::Zsh, &mut cmd, "rc", &mut buf);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("rc"));
        assert!(output.contains("compdef"));
    }

    #[test]
    fn test_completions_fish() {
        let mut cmd = Cli::command();
        let mut buf = Vec::new();
        clap_complete::generate(Shell::Fish, &mut cmd, "rc", &mut buf);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("rc"));
        assert!(output.contains("complete"));
    }

    #[test]
    fn test_completions_powershell() {
        let mut cmd = Cli::command();
        let mut buf = Vec::new();
        clap_complete::generate(Shell::PowerShell, &mut cmd, "rc", &mut buf);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("rc"));
        assert!(output.contains("Register-ArgumentCompleter"));
    }
}
