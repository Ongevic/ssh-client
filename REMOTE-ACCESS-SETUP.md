# Remote File Access for Sigma File Manager (via SSHFS) + SSH TUI integration

This documents how the Windows box was set up so that a remote SSH/SFTP host
(`<HPC_HOST>`, the HPC) is browsable as a normal drive in **Sigma File Manager**
(and Explorer, and everything else), and how the custom Rust TUI
(`ssh_grabber_rs`) was wired into Sigma's own right-click **"Open in terminal"**
menu.

_Last set up: 2026-06-22._

---

## 1. The problem & the chosen approach

- **Sigma File Manager** is installed from the **Microsoft Store (MSIX)**. Its
  binary lives in the protected `C:\Program Files\WindowsApps\…` and cannot be
  patched, and Sigma v1 has **no plugin / remote-backend API**. So "extend Sigma
  for remote access" cannot mean modifying Sigma itself.
- **Chosen approach ("Path A"): mount the remote over SFTP** so that *any* GUI
  file manager sees it as an ordinary local drive. Zero changes to Sigma.

Two other paths were considered and rejected for now:
- **B** — fork Sigma's source and add an SFTP backend in JS (most maintenance,
  throws away the existing Rust code).
- **C** — turn the Rust app into a WebDAV/WinFsp sidecar (reuses the Rust engine
  but is a bigger build). Worth revisiting if the Rust app should *be* the engine.

---

## 2. Components installed

| Component | Purpose | Install |
|-----------|---------|---------|
| **WinFsp** | Kernel-mode filesystem driver (FUSE-for-Windows) | `winget install WinFsp.WinFsp` |
| **SSHFS-Win** | Bridges SFTP → a Windows drive via WinFsp | `winget install SSHFS-Win.SSHFS-Win` |

SSHFS-Win binaries: `C:\Program Files\SSHFS-Win\bin\` (`sshfs.exe`, `ssh.exe`, …).

---

## 3. Single source of credentials — `ssh.txt`

Both the mount and the TUI read the same file: **`%USERPROFILE%\ssh.txt`**

```
<host>
<username>
<password>
<port>        # optional, defaults to 22
```

Edit this one file to change server/password; the TUI and the mount both pick it up.
The TUI auto-loads it on launch (status line: "Loaded login details from …"); in the
TUI just press **Enter** to connect, or **`l`** to reload.

---

## 4. The mount launcher — `remote-mount.ps1`

Location: `<SUITE_DIR>\remote-mount.ps1`

Reads `ssh.txt` and mounts via:
`net use X: \\sshfs\user@host <password> /user:user@host /persistent:no`

### Usage

```powershell
cd "<SUITE_DIR>"

# Mount remote HOME to X: and open Sigma + Explorer on it
& ".\remote-mount.ps1" -LaunchSigma

# Mount filesystem ROOT (/) to drive R:
& ".\remote-mount.ps1" -Drive R -Root

# Mount a specific subfolder (relative to remote home)
& ".\remote-mount.ps1" -RemotePath "CLIMATE_DATA"

# Unmount
& ".\remote-mount.ps1" -Unmount
```

### Switches

| Switch | Effect |
|--------|--------|
| `-Drive X` | Drive letter to use (default `X`) |
| `-Root` | Mount `/` instead of the user's home dir |
| `-RemotePath "sub/dir"` | Mount a specific subfolder |
| `-SshTxt <path>` | Use a different credentials file |
| `-LaunchSigma` | Open Sigma + Explorer on the mount after mounting |
| `-Unmount` | Tear down the drive |
| `-InstallStartup` | Register the auto-mount logon task (see §5) |
| `-RemoveStartup` | Remove the logon task |

> **Note:** the password is passed to `net use` on the command line (briefly
> visible in a process listing on this machine). Fine for a personal box. For
> stricter setups, switch to key auth using the `\\sshfs.k\` prefix.

---

## 5. Persistence — auto-mount at sign-in

`X:` is re-mounted automatically at every logon by a per-user **scheduled task**
named **`SSHFS-RemoteMount-X`** (runs `remote-mount.ps1` with a 15-second delay so
the WinFsp service is up first). The task is registered with **`-LaunchSigma`**, so
after the mount it **also opens Sigma** — by sign-in you have remote browsing with
zero clicks (Sigma opens only after `X:` exists, so its tab is populated).

**Backup recovery shortcut:** Desktop → **`Mount Remote (X-)`** runs
`remote-mount.ps1 -Drive X -LaunchSigma` in one click. Use it after booting
off-network: connect to the institutional LAN/VPN first (the host `<HPC_HOST>` is a
private address, so the mount only works on-network), then click it to mount +
open Sigma. To re-enable Sigma auto-launch yourself:
`& ".\remote-mount.ps1" -Drive X -InstallStartup -LaunchSigma`.

```powershell
& ".\remote-mount.ps1" -Drive X -InstallStartup   # enable (already done)
& ".\remote-mount.ps1" -Drive X -RemoveStartup    # disable
```

**Why a scheduled task instead of `net use /persistent:yes`?**
Persistent sshfs mappings come back as a dead "red X" drive after a reboot,
because Windows restores the mapping *before* the WinFsp service and credentials
are ready. The logon task re-runs the real mount logic, so it reconnects cleanly.

---

## 6. SSH TUI inside Sigma's "Open in terminal" menu

**Key discovery:** Sigma's right-click → **Open in terminal** submenu is populated
**directly from the Windows Terminal profile list** (the giveaway: it lists
"Azure Cloud Shell", which only exists as a WT dynamic profile). Sigma has **no
"add custom terminal" UI**. Therefore you add a custom terminal by adding a
**Windows Terminal profile**.

### What was added

File: `%LOCALAPPDATA%\Packages\Microsoft.WindowsTerminal_8wekyb3d8bbwe\LocalState\settings.json`
→ a new entry in `profiles.list`:

```json
{
  "name": "SSH TUI",
  "guid": "{7b3f9e2a-1c4d-4f6a-9b8e-2d5c6a7f8e90}",
  "commandline": "\"<SUITE_DIR>\\ssh_grabber_rs\\target\\release\\ssh_grabber_rs.exe\"",
  "icon": "🔌",
  "closeOnExit": "always",
  "hidden": false
}
```

### Important gotcha

**Sigma caches the WT profile list at launch.** After editing `settings.json` you
must **restart Sigma** (process name `sigma-file-manager`) for the new profile to
appear in the menu.

### Result

In Sigma: **right-click any folder → Open in terminal → SSH TUI** (or `Alt+T`).
The TUI launches with the right-clicked folder as its starting directory (its
*local* pane), and connects to the remote using `ssh.txt`.

`launch-tui.cmd` (same folder) is a standalone fallback launcher — redundant for
the Sigma path, but handy for a desktop/Start shortcut.

---

## 7. Rebuilding the TUI

**Prerequisites (both were missing once and had to be reinstalled):**
1. **Rust toolchain** — `winget install Rustlang.Rustup` then
   `rustup toolchain install stable`. `cargo` lands in `%USERPROFILE%\.cargo\bin`
   and is **not on PATH** by default in non-login shells — call it by full path or
   prepend `$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"`.
2. **MSVC C++ build tools** — required because `ring` (via `russh`) compiles C/asm
   and the MSVC target needs `link.exe`. Without it the build dies early with
   `error: linker 'link.exe' not found`. Install:
   `winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
cd "<SUITE_DIR>\ssh_grabber_rs"
cargo build --release      # ~2 min; Rust auto-detects the MSVC toolchain
```
Binary: `…\ssh_grabber_rs\target\release\ssh_grabber_rs.exe`
(Rust, `russh` + `russh-sftp` + `ratatui`.)

> **Watch for a stale binary:** if the TUI misbehaves (e.g. remote pane mirrors
> local, `d` does nothing), check that the exe's date is newer than `src\main.rs`.
> The exe is git-untracked, so editing the source does NOT rebuild it — you must
> run `cargo build --release` yourself.

### Key bindings (browser)
`Tab` switch pane · `Enter`/`l`/`→` open · `Bksp`/`h`/`←` up · `j`/`k` move ·
`u` upload · `d` download · **`Space` transfer current item** (upload from local
pane / download from remote pane) · `/` filter · `n` mkdir · `R` rename · `x`
delete · `:` command · `r` refresh · `q` quit.

---

## 8. Quick reference / files

| File | Role |
|------|------|
| `%USERPROFILE%\ssh.txt` | Credentials (host/user/pass/port) — shared by TUI + mount |
| `…\ssh_client_suite\remote-mount.ps1` | Mount / unmount / persist launcher |
| `…\ssh_client_suite\launch-tui.cmd` | Standalone TUI launcher (optional) |
| `…\ssh_client_suite\ssh_grabber_rs\` | The Rust TUI source + release binary |
| WT `settings.json` | Holds the "SSH TUI" profile that surfaces in Sigma |
| Task `SSHFS-RemoteMount-X` | Logon auto-mount of `X:` |

### Two reminders
1. Editing the "SSH TUI" WT profile → **restart Sigma** to see the change.
2. Changing server/password → edit **`ssh.txt`** only; both tools follow.
