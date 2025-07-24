# Gaffa

A cross-platform process manager for [procfile](https://procfile.dev/) based applications in a single terminal.
Has an interactive terminal UI.

There are some good Procfile-based process management tools around already:
- [foreman](https://ddollar.github.io/foreman/)
- [overmind](https://github.com/DarthSim/overmind)
- [honcho](https://honcho.readthedocs.io/en/latest/)

but they didn't work on the windows machines i have to work on.
I do not need a lot of features, just some convenience to run my processes in a
single terminal with a single command from my `justfile`.

## Features

- **Cross-platform**: Works on Windows, macOS, and Linux without tmux dependency
- **Process Management**: Start, stop, and restart processes individually in interactive mode
- **Live Monitoring**: Process status, uptime, and restart counts


## Installation

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
```
