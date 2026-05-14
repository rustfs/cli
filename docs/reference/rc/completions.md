# rc completions

## Purpose

`rc completions` generates shell completion scripts for supported shells.

## Syntax

```bash
rc [GLOBAL OPTIONS] completions <SHELL>
```

## Parameters

| Parameter | Description |
| --- | --- |
| `SHELL` | One of `bash`, `elvish`, `fish`, `powershell`, or `zsh`. |

## Examples

Generate zsh completions:

```bash
rc completions zsh > _rc
```

Generate bash completions:

```bash
rc completions bash > rc.bash
```

## Behavior

The command writes the completion script to stdout. Install the generated file according to the conventions of your shell and operating system.

Global options shown in command syntax use the same meaning everywhere:

| Option | Description |
| --- | --- |
| `--format auto\|human\|json` | Select automatic, human-readable, or JSON output. |
| `--json` | Emit JSON output where the command supports structured output. |
| `--no-color` | Disable terminal colors. |
| `--no-progress` | Disable progress bars. |
| `-q, --quiet` | Suppress non-error output. |
| `--debug` | Enable debug logging. |
