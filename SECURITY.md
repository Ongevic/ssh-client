# Security Policy

## Sensitive Data

This app may handle:

- SSH hosts
- usernames
- passwords
- remote file paths
- downloaded remote file contents

Treat all connection data as sensitive.

## What Not To Share

Do not publish or send:

- `ssh.txt`
- screenshots containing credentials
- exported config or state files containing personal paths
- remote hostnames, usernames, or private server paths unless you intend to disclose them

## Local Storage

The app may create:

- `%USERPROFILE%\\.ssh_sftp_file_client_config.json`
- `%USERPROFILE%\\.ssh_sftp_file_client_state.json`
- temporary remote-open files in the app temp folder

The app temp folder is cleaned on startup and best-effort cleaned on exit, but users should still avoid opening sensitive remote files on untrusted machines.

## Sharing This Project Safely

Before sharing:

1. Remove any real `ssh.txt` file.
2. Remove generated Python cache folders.
3. Do not include user home-directory config/state files.
4. Review the code and documentation for hardcoded hosts, usernames, or paths.

## Credentials

This project currently supports password-based login file loading for convenience.

Recommended precautions:

- use a non-production test account when demonstrating the app
- rotate any credentials that were ever stored in plain text
- prefer key-based authentication or a safer credential flow if this project evolves
- review screenshots, demo assets, logs, and generated binaries for accidental personal data before publishing
- keep the `LICENSE` file with the repository when archiving or sharing it

## Reporting

If you find a security issue in the project, report it privately to the maintainer and avoid posting credentials, hostnames, or sensitive screenshots publicly.

## Audit notes

- No hardcoded hosts, usernames, passwords or private keys in the tracked source. The setup guide uses placeholders such as `<HPC_HOST>`.
- Host keys: both clients trust a host on first use and then check it against `~/.ssh/known_hosts`; a changed key is refused by the terminal client. Verify the fingerprint out of band on first connect.
- `ssh.txt` stores the password in plain text. Prefer key authentication. If you do use it, restrict it to your account: `icacls "%USERPROFILE%\ssh.txt" /inheritance:r /grant:r "%USERNAME%:R"`.
- `remote-mount.ps1` passes the password to `net use` on its command line, where other local processes can briefly see it. Prefer key authentication on shared machines.
- The password field is masked in both clients and is not logged.
