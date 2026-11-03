# SSH/SFTP File Client

> **Use at your own risk.** No warranty; see the [Disclaimer](#disclaimer).

A Windows desktop SSH/SFTP client built with `customtkinter`, `paramiko`, and `Pillow`.

This repository can be published to GitHub without including personal credentials or local machine state, provided the private files listed below are not committed.

It provides:

- two local panes for local file browsing and transfer
- one remote pane for SSH/SFTP browsing
- remote file download, open, preview, rename, delete, and folder creation
- local copy/move between panes
- a Linux-oriented SSH command runner
- per-user bookmarks, filters, sort controls, and saved UI state

## Rust terminal client (`ssh_grabber_rs`)

A standalone terminal (TUI) SSH/SFTP client built with `ratatui`, `russh`, and `russh-sftp`. It runs entirely in the terminal with side-by-side local and remote panes.

Features:

- password **or** SSH public-key authentication (key path + optional passphrase), toggled on the connect screen; defaults to `~/.ssh/id_ed25519`/`id_rsa`/`id_ecdsa` if present
- host-key verification against `known_hosts` (learns new keys on first connect)
- local and remote file operations: create folder (`n`), rename (`R`), delete (`x`) — applied to whichever pane is focused
- live type-to-filter in each pane (`/`); Enter keeps the filter, Esc clears it
- background uploads/downloads with a live progress gauge (bytes + file counts) and cancel (`c`), plus overwrite confirmation when a target already exists
- a remote shell command runner (`:`) that tracks the working directory
- optional `ssh.txt` login loading (Ctrl+L)

- reads `%USERPROFILE%\.ssh\config` (Host blocks with several patterns, `Host *` defaults, first-value-wins, `Include` globs, quotes, `=`, `%d/%u/%h/%r/%p/%n` tokens; `Match` blocks ignored) so `hpc` works like `ssh hpc`
- startup host picker: concrete aliases (`hpc` first) with resolved `user@host:port`; Enter connects, `/` filters, `m` opens the manual form, `r` reloads the config
- auth order for aliases: ssh-agent (Windows OpenSSH agent pipe) / Pageant, then each `IdentityFile` (masked passphrase prompt if encrypted, Esc skips the key), then a masked password or keyboard-interactive prompt. Typed secrets are never stored. `IdentitiesOnly`, `ServerAliveInterval`/`CountMax` and `ConnectTimeout` are honoured
- `ProxyJump` with one hop is supported (tunnelled over a direct-tcpip channel, both hosts checked against known_hosts); multi-hop chains and `ProxyCommand` show a clear "not supported yet" message

Build and run:

```powershell
cd ssh_grabber_rs
cargo run --release                 # host picker
ssh_grabber_rs.exe hpc              # connect straight away, like `ssh hpc`
ssh_grabber_rs.exe alice@hpc D:\data  # user override + starting local directory
ssh_grabber_rs.exe --list-hosts     # resolved aliases + whether key files exist (never prints key contents)
ssh_grabber_rs.exe --help
```

An argument that is an existing directory is always the local starting directory; anything else is a host alias.

Key bindings (browser): `Tab` switch panes, `Enter`/`l` open, `Backspace`/`h` parent, `u` upload, `d` download, `/` filter, `n` mkdir, `R` rename, `x` delete, `:` command, `r` refresh, `q` quit.

## Requirements

- Python 3.10+
- `paramiko`
- `customtkinter`
- `pillow`

Install dependencies:

```powershell
python -m pip install paramiko customtkinter pillow
```

Run the app:

```powershell
python ssh_client.py
```

## Login File

The app optionally reads an `ssh.txt` file from:

1. `%USERPROFILE%\\ssh.txt`
2. the same folder as `ssh_client.py`

Expected format:

```text
host
username
password
22
```

Line 4 is optional and defaults to `22`.

Do not share a real `ssh.txt` file.

## Shareable Setup

This project is designed so the script itself can be shared without carrying personal machine settings:

- local bookmarks default from the current user profile at runtime
- remote bookmarks default from the loaded SSH username
- UI state is saved per user in their home directory
- config is saved per user in their home directory
- temp files for remote open/preview use a dedicated app temp folder and are cleaned up

Files that should stay private:

- `ssh.txt`
- `%USERPROFILE%\\.ssh_sftp_file_client_config.json`
- `%USERPROFILE%\\.ssh_sftp_file_client_state.json`
- Rust build output under `ssh_grabber_rs/target/`
- any exported binaries, logs, temp files, or screenshots containing personal server details

## Packaging

For non-technical users, package it as a Windows executable with PyInstaller:

```powershell
python -m pip install pyinstaller
pyinstaller --noconfirm --onefile --windowed ssh_client.py
```

The built executable will appear in `dist\\`.

## Notes

- Remote `Open` downloads a temporary local copy, then opens it with the local default application.
- `Download To A` and `Download To B` save real copies into the visible local panes.
- Remote delete is permanent and includes recursive folder deletion after confirmation.

## Disclaimer

This software is provided "as is", without warranty of any kind (see [LICENSE](LICENSE)). It has not been security audited. **Use it at your own risk.** The author is not responsible for data loss, damage to files or systems, security incidents, account lockouts, or any misuse of credentials resulting from its use. Prefer SSH keys over passwords, keep backups, and try it on non-critical systems first. Read [SECURITY.md](SECURITY.md) before connecting to anything important.