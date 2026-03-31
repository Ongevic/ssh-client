# SSH/SFTP File Client

A Windows desktop SSH/SFTP client built with `customtkinter`, `paramiko`, and `Pillow`.

It provides:

- two local panes for local file browsing and transfer
- one remote pane for SSH/SFTP browsing
- remote file download, open, preview, rename, delete, and folder creation
- local copy/move between panes
- a Linux-oriented SSH command runner
- per-user bookmarks, filters, sort controls, and saved UI state

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

## Setup

This project is designed so the script itself can be shared without carrying personal machine settings:

- local bookmarks default from the current user profile at runtime
- remote bookmarks default from the loaded SSH username
- UI state is saved per user in their home directory
- config is saved per user in their home directory
- temp files for remote open/preview use a dedicated app temp folder and are cleaned up

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
