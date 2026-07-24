# rc completions

## Purpose

`rc completions` generates shell completion scripts for supported shells.

## Syntax

```bash
rc [GLOBAL OPTIONS] completions <SHELL> [--install] [--force] [--install-dir <DIRECTORY>]
```

## Parameters

| Parameter | Description |
| --- | --- |
| `SHELL` | One of `bash`, `elvish`, `fish`, `powershell`, or `zsh`. |
| `--install` | Install the generated script in the shell's user completion directory. |
| `--force` | Replace an existing installed script. Requires `--install`. |
| `--install-dir <DIRECTORY>` | Install into a specific directory. Requires `--install`. |

## Examples

Generate zsh completions:

```bash
rc completions zsh > _rc
```

Generate bash completions:

```bash
rc completions bash > rc.bash
```

Install fish completions using the standard user-level directory:

```bash
rc completions fish --install
```

Install zsh completions in an explicitly configured directory:

```bash
rc completions zsh --install --install-dir ~/.zfunc
```

## Behavior

Without `--install`, the command writes the completion script to stdout. With
`--install`, it creates the destination directory and atomically installs the
script. Existing scripts are preserved unless `--force` is supplied.

The default user-level locations honor `XDG_DATA_HOME` and `XDG_CONFIG_HOME`
when set, falling back to directories below `HOME`:

| Shell | Default path |
| --- | --- |
| Bash | `$XDG_DATA_HOME/bash-completion/completions/rc` |
| Elvish | `$XDG_DATA_HOME/elvish/lib/rc.elv` |
| Fish | `$XDG_CONFIG_HOME/fish/completions/rc.fish` |
| PowerShell | `$XDG_CONFIG_HOME/powershell/completions/rc.ps1` |
| Zsh | `$XDG_DATA_HOME/zsh/site-functions/_rc` |

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |
