# Gaffa

A cross-platform process manager for [procfile](https://procfile.dev/) based applications in a single terminal.
Has an interactive terminal UI.

There are some good Procfile-based process management tools around already:
- [foreman](https://ddollar.github.io/foreman/)
- [overmind](https://github.com/DarthSim/overmind)
- [honcho](https://honcho.readthedocs.io/en/latest/)

but they didn't work on the windows machines i have to work on.
I do not need a lot of features, just some convenience to run my processes in a
single terminal with a single command from my [justfile](https://crates.io/crates/just).

A `gaffa` is someone who manages a group of performers, think of a rock band or
circus. Whatever you like. 🧙‍♀️

## Features

- **Cross-platform**: Works on Windows, macOS, and Linux without tmux dependency
- **Process Management**: Start, stop, and restart processes individually in interactive mode
- **Live Monitoring**: Process status, uptime, and restart counts
- **Interactive Mode**: Terminal UI with keyboard shortcuts and mouse scrolling


## Installation

Available on crates.io: [gaffa](https://crates.io/crates/gaffa)

```bash
cargo install gaffa
```

Or build from source:

```bash
git clone https://github.com/oryon-dominik/gaffa.git
cd gaffa
cargo build --release
```

## Usage

```bash
# Run all processes from Procfile
gaffa run

# Run specific processes
gaffa run web worker

# Interactive mode
gaffa run --interactive

# Log output to a file
gaffa run --log-file log.txt

# Custom Procfile
gaffa run --procfile custom.procfile

# Set environment variables
gaffa run --env PORT=8000 --env PYTHONUNBUFFERED=1

# Load environment from file
gaffa run --env-file .env

# Choose the shell that runs the Procfile command lines
gaffa run --shell bash

# Full command example
gaffa run devserver tailwind --procfile procfile --log-file logs/gaffa.log --env PYTHONUNBUFFERED=1 --interactive
```

## Shell

Each Procfile command line is passed verbatim to a shell — pipes, `&&`,
redirects, and PATH lookups (including `.cmd` shims like `npm` on Windows)
work exactly as they would when typed into that shell.

Default: `pwsh` (fallback `cmd` if PowerShell 7 is not installed) on Windows,
`sh` on Unix. Override with `--shell <PROGRAM>` or the `GAFFA_SHELL`
environment variable; `--shell` wins. Known shells (`cmd`, `pwsh`,
`powershell`) get their native command flag, everything else is invoked
POSIX-style with `-c`.

Note for Windows: with the `pwsh` default, command lines are PowerShell code —
`$VAR` inside double quotes is expanded by PowerShell. Use single quotes for
literal strings, or `--shell cmd` for cmd-style lines.

## Output Buffering

Some processes buffer their output when not connected to a terminal. To see real-time output:

- **Python**: Set `PYTHONUNBUFFERED=1` environment variable
