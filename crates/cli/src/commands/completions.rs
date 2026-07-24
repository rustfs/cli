//! Shell completion generation
//!
//! Generate shell completion scripts for bash, zsh, fish, and powershell.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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

    /// Install the completion script in the shell's user completion directory
    #[arg(long)]
    pub install: bool,

    /// Replace an existing installed completion script
    #[arg(long, requires = "install")]
    pub force: bool,

    /// Override the completion installation directory
    #[arg(long, value_name = "DIRECTORY", requires = "install")]
    pub install_dir: Option<PathBuf>,
}

/// Generate shell completions and either print or install them
pub fn execute(args: CompletionsArgs) -> ExitCode {
    let completion = generate_completions(args.shell);
    if !args.install {
        if let Err(error) = io::stdout().write_all(&completion) {
            eprintln!("Failed to write completion script: {error}");
            return ExitCode::GeneralError;
        }
        return ExitCode::Success;
    }

    let destination = match completion_path(args.shell, args.install_dir.as_deref()) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("Failed to determine completion installation path: {error}");
            return ExitCode::GeneralError;
        }
    };

    match install_completions(&destination, &completion, args.force) {
        Ok(()) => {
            println!("Installed completions to {}", destination.display());
            ExitCode::Success
        }
        Err(error) => {
            eprintln!("Failed to install completions: {error}");
            ExitCode::GeneralError
        }
    }
}

fn generate_completions<G: Generator>(generator: G) -> Vec<u8> {
    let mut command = Cli::command();
    let binary_name = command.get_name().to_string();
    let mut output = Vec::new();
    clap_complete::generate(generator, &mut command, binary_name, &mut output);
    output
}

fn completion_path(shell: Shell, install_dir: Option<&Path>) -> io::Result<PathBuf> {
    let file_name = match shell {
        Shell::Bash => "rc",
        Shell::Elvish => "rc.elv",
        Shell::Fish => "rc.fish",
        Shell::PowerShell => "rc.ps1",
        Shell::Zsh => "_rc",
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("completion installation is not supported for {shell}"),
            ));
        }
    };
    if let Some(directory) = install_dir {
        return Ok(directory.join(file_name));
    }

    let home = env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "HOME is not set; pass --install-dir explicitly",
        )
    })?;
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));

    let directory = match shell {
        Shell::Bash => data_home.join("bash-completion/completions"),
        Shell::Elvish => data_home.join("elvish/lib"),
        Shell::Fish => config_home.join("fish/completions"),
        Shell::PowerShell => config_home.join("powershell/completions"),
        Shell::Zsh => data_home.join("zsh/site-functions"),
        _ => unreachable!("unsupported shells returned above"),
    };
    Ok(directory.join(file_name))
}

fn install_completions(destination: &Path, contents: &[u8], force: bool) -> io::Result<()> {
    let directory = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "completion destination has no parent directory",
        )
    })?;
    fs::create_dir_all(directory)?;

    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(contents)?;
    temporary.as_file_mut().sync_all()?;

    if force {
        temporary
            .persist(destination)
            .map_err(|error| error.error)?;
    } else {
        temporary
            .persist_noclobber(destination)
            .map_err(|error| error.error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completions_bash() {
        let buf = generate_completions(Shell::Bash);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("rc"));
        assert!(output.contains("complete"));
    }

    #[test]
    fn test_completions_zsh() {
        let buf = generate_completions(Shell::Zsh);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("rc"));
        assert!(output.contains("compdef"));
    }

    #[test]
    fn test_completions_fish() {
        let buf = generate_completions(Shell::Fish);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("rc"));
        assert!(output.contains("complete"));
    }

    #[test]
    fn test_completions_powershell() {
        let buf = generate_completions(Shell::PowerShell);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("rc"));
        assert!(output.contains("Register-ArgumentCompleter"));
    }

    #[test]
    fn install_uses_shell_specific_file_name() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            completion_path(Shell::Bash, Some(directory.path())).unwrap(),
            directory.path().join("rc")
        );
        assert_eq!(
            completion_path(Shell::Zsh, Some(directory.path())).unwrap(),
            directory.path().join("_rc")
        );
        assert_eq!(
            completion_path(Shell::Fish, Some(directory.path())).unwrap(),
            directory.path().join("rc.fish")
        );
        assert_eq!(
            completion_path(Shell::PowerShell, Some(directory.path())).unwrap(),
            directory.path().join("rc.ps1")
        );
    }

    #[test]
    fn install_refuses_to_replace_existing_script_without_force() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("rc");
        fs::write(&destination, b"old completion").unwrap();

        let error = install_completions(&destination, b"new completion", false).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&destination).unwrap(), b"old completion");
    }

    #[test]
    fn force_install_replaces_existing_script() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("nested/rc.fish");
        install_completions(&destination, b"old completion", false).unwrap();

        install_completions(&destination, b"new completion", true).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"new completion");
    }
}
