use std::{
    collections::VecDeque,
    fs,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};
use russh::{
    ChannelMsg, Disconnect,
    keys::{PrivateKeyWithHashAlg, load_secret_key},
};
use russh_sftp::{client::SftpSession, protocol::FileType};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    runtime::Runtime,
    task::JoinHandle,
};

mod connect;
mod picker;
mod sshconfig;

use connect::{
    AuthFlow, AuthStep, SshHandle, client_config, open_direct, open_sftp, open_via_jump,
};
use picker::{Picker, PickerAction};
use sshconfig::{ResolvedHost, SshConfig, local_username, parse_proxy_jump};

const APP_TITLE: &str = "sshgrab";
const DEFAULT_PORT: &str = "22";
const MAX_LOG_LINES: usize = 200;
const TRANSFER_CHUNK: usize = 131_072;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Screen {
    Picker,
    Connect,
    Browser,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthPromptKind {
    Passphrase,
    Password,
    Keyboard,
}

/// Masked (unless the server says echo) prompt shown during authentication.
/// The typed value is passed to russh once and then dropped; never stored.
struct AuthPrompt {
    kind: AuthPromptKind,
    title: String,
    input: String,
    masked: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Jump,
    Target,
}

/// A connection that is authenticating (possibly waiting for user input).
struct PendingConnect {
    flow: AuthFlow,
    stage: Stage,
    jump: Option<SshHandle>,
    target: ResolvedHost,
    note: Arc<Mutex<Option<String>>>,
}

/// Slow work queued until after the next redraw so the status line shows.
enum Deferred {
    ConnectAlias {
        alias: String,
        user: Option<String>,
    },
    ConnectManual,
    AuthAnswer {
        kind: AuthPromptKind,
        secret: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthMethod {
    Password,
    Key,
}

impl AuthMethod {
    fn label(self) -> &'static str {
        match self {
            AuthMethod::Password => "Password",
            AuthMethod::Key => "SSH key",
        }
    }

    fn toggled(self) -> AuthMethod {
        match self {
            AuthMethod::Password => AuthMethod::Key,
            AuthMethod::Key => AuthMethod::Password,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectField {
    Host,
    User,
    Auth,
    Password,
    KeyPath,
    Passphrase,
    Port,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrowserFocus {
    Local,
    Remote,
}

#[derive(Clone, Debug)]
enum PromptKind {
    Command,
    Filter { focus: BrowserFocus },
    RemoteMkdir,
    RemoteRename { old_path: String, old_name: String },
    RemoteDelete { target_path: String, is_dir: bool, name: String },
    LocalMkdir,
    LocalRename { old_path: PathBuf, old_name: String },
    LocalDelete { target_path: PathBuf, is_dir: bool, name: String },
    ConfirmUpload { local_path: PathBuf, remote_path: String, name: String },
    ConfirmDownload {
        remote_path: String,
        local_path: PathBuf,
        name: String,
        is_dir: bool,
        size: Option<u64>,
    },
}

#[derive(Clone, Debug)]
struct PromptState {
    kind: PromptKind,
    input: String,
    title: String,
}

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    path: String,
    is_dir: bool,
    size: Option<u64>,
    modified: Option<String>,
}

#[derive(Debug, Default)]
struct Progress {
    label: String,
    current: String,
    files_total: u64,
    files_done: u64,
    bytes_total: u64,
    bytes_done: u64,
    done: bool,
    error: Option<String>,
}

impl Progress {
    fn new(label: String) -> Self {
        Self {
            label,
            ..Default::default()
        }
    }
}

struct TransferJob {
    progress: Arc<Mutex<Progress>>,
    handle: JoinHandle<()>,
    verb: &'static str,
}

struct App {
    runtime: Runtime,
    screen: Screen,
    connect_field: ConnectField,
    browser_focus: BrowserFocus,
    auth_method: AuthMethod,
    host: String,
    user: String,
    password: String,
    key_path: String,
    passphrase: String,
    port: String,
    login_file_path: Option<PathBuf>,
    local_path: PathBuf,
    local_entries: Vec<Entry>,
    local_selected: usize,
    local_filter: String,
    remote_path: String,
    remote_entries: Vec<Entry>,
    remote_selected: usize,
    remote_filter: String,
    command_input: String,
    command_output: VecDeque<String>,
    prompt: Option<PromptState>,
    transfer: Option<TransferJob>,
    ssh: Option<SshHandle>,
    jump_ssh: Option<SshHandle>,
    sftp: Option<Arc<SftpSession>>,
    config: SshConfig,
    picker: Picker,
    pending: Option<PendingConnect>,
    auth_prompt: Option<AuthPrompt>,
    deferred: Option<Deferred>,
    should_quit: bool,
    status: String,
}

impl App {
    fn new(start_dir: Option<PathBuf>) -> Result<Self> {
        let runtime = Runtime::new().context("failed to create Tokio runtime")?;
        let login_file_path = find_login_file();
        let default_key = default_key_path();
        let config = SshConfig::load_default();
        let picker = Picker::from_config(&config);
        let screen = if picker.entries.is_empty() {
            Screen::Connect
        } else {
            Screen::Picker
        };
        let local_path = start_dir
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let mut app = Self {
            runtime,
            screen,
            connect_field: ConnectField::Host,
            browser_focus: BrowserFocus::Remote,
            auth_method: AuthMethod::Password,
            host: String::new(),
            user: String::new(),
            password: String::new(),
            key_path: default_key,
            passphrase: String::new(),
            port: DEFAULT_PORT.to_string(),
            login_file_path,
            local_path,
            local_entries: Vec::new(),
            local_selected: 0,
            local_filter: String::new(),
            remote_path: ".".to_string(),
            remote_entries: Vec::new(),
            remote_selected: 0,
            remote_filter: String::new(),
            command_input: String::new(),
            command_output: VecDeque::new(),
            prompt: None,
            transfer: None,
            ssh: None,
            jump_ssh: None,
            sftp: None,
            config,
            picker,
            pending: None,
            auth_prompt: None,
            deferred: None,
            should_quit: false,
            status: "Fill the form and press Enter. Ctrl+L loads ssh.txt.".to_string(),
        };
        if app.login_file_path.is_some() {
            let _ = app.load_login_file();
        }
        if app.screen == Screen::Picker {
            app.status = format!(
                "{} host(s) from ~/.ssh/config. Enter connects, m for a manual host.",
                app.picker.entries.len()
            );
        }
        for warning in &app.config.warnings {
            app.status = format!("ssh config: {warning}");
        }
        if app.refresh_local().is_err() {
            app.local_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            app.refresh_local()?;
        }
        Ok(app)
    }

    // ---- ssh-config (alias) connections -------------------------------

    fn request_alias_connect(&mut self, alias: String, user: Option<String>) {
        let resolved = self.config.resolve(&alias, &local_username());
        let user_shown = user.clone().unwrap_or_else(|| resolved.user.clone());
        self.status = format!(
            "Connecting to {alias} ({user_shown}@{}:{})...",
            resolved.hostname, resolved.port
        );
        self.deferred = Some(Deferred::ConnectAlias { alias, user });
    }

    fn run_deferred(&mut self) {
        let Some(action) = self.deferred.take() else {
            return;
        };
        let result = match action {
            Deferred::ConnectAlias { alias, user } => self.start_alias_connect(&alias, user),
            Deferred::ConnectManual => self.connect(),
            Deferred::AuthAnswer { kind, secret } => self.answer_auth(kind, secret),
        };
        if let Err(err) = result {
            self.status = format!("{err:#}");
        }
    }

    fn start_alias_connect(&mut self, alias: &str, user_override: Option<String>) -> Result<()> {
        let local_user = local_username();
        let mut target = self.config.resolve(alias, &local_user);
        if let Some(user) = user_override {
            target.user = user;
        }
        if let Some(command) = &target.proxy_command {
            bail!(
                "`{alias}` uses ProxyCommand (`{command}`), which is not supported yet. Use ProxyJump or a plain ssh shell."
            );
        }
        let jump_spec = match &target.proxy_jump {
            Some(value) => parse_proxy_jump(value).map_err(anyhow::Error::msg)?,
            None => None,
        };
        let note = Arc::new(Mutex::new(None));
        let (flow, stage) = if let Some(spec) = jump_spec {
            let mut jump = self.config.resolve(&spec.host, &local_user);
            if let Some(user) = spec.user {
                jump.user = user;
            }
            if let Some(port) = spec.port {
                jump.port = port;
            }
            if jump.proxy_jump.is_some() || jump.proxy_command.is_some() {
                bail!(
                    "jump host `{}` itself needs a ProxyJump/ProxyCommand; multi-hop chains are not supported yet",
                    spec.host
                );
            }
            let config = client_config(
                jump.server_alive_interval.or(target.server_alive_interval),
                jump.server_alive_count_max,
            );
            let jump_note = Arc::new(Mutex::new(None));
            let handle = self
                .runtime
                .block_on(open_direct(
                    &jump.hostname,
                    jump.port,
                    config,
                    jump.connect_timeout.or(target.connect_timeout),
                    jump_note,
                ))
                .with_context(|| format!("jump host {}", jump.hostname))?;
            (
                AuthFlow::new(
                    handle,
                    jump.user.clone(),
                    &jump.hostname,
                    jump.identity_files.clone(),
                    jump.identities_only,
                ),
                Stage::Jump,
            )
        } else {
            let config = client_config(target.server_alive_interval, target.server_alive_count_max);
            let handle = self.runtime.block_on(open_direct(
                &target.hostname,
                target.port,
                config,
                target.connect_timeout,
                note.clone(),
            ))?;
            (
                AuthFlow::new(
                    handle,
                    target.user.clone(),
                    &target.hostname,
                    target.identity_files.clone(),
                    target.identities_only,
                ),
                Stage::Target,
            )
        };
        let mut pending = PendingConnect {
            flow,
            stage,
            jump: None,
            target,
            note,
        };
        let step = self.runtime.block_on(pending.flow.run());
        self.pending = Some(pending);
        self.drive_auth(step)
    }

    fn drive_auth(&mut self, step: Result<AuthStep>) -> Result<()> {
        let step = match step {
            Ok(step) => step,
            Err(err) => {
                self.abort_pending();
                return Err(err);
            }
        };
        let label = self
            .pending
            .as_ref()
            .map(|p| p.flow.label.clone())
            .unwrap_or_default();
        match step {
            AuthStep::Done => return self.after_auth(),
            AuthStep::NeedPassphrase { key } => {
                let name = key
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                self.auth_prompt = Some(AuthPrompt {
                    kind: AuthPromptKind::Passphrase,
                    title: format!("Passphrase for key {name} ({label})"),
                    input: String::new(),
                    masked: true,
                });
                self.status = "Key is encrypted. Enter its passphrase, Esc skips this key.".to_string();
            }
            AuthStep::NeedPassword => {
                self.auth_prompt = Some(AuthPrompt {
                    kind: AuthPromptKind::Password,
                    title: format!("Password for {label}"),
                    input: String::new(),
                    masked: true,
                });
                self.status = "Keys/agent did not log in. Enter password, Esc cancels.".to_string();
            }
            AuthStep::NeedKeyboard { prompt, echo } => {
                let prompt = if prompt.is_empty() {
                    "Response".to_string()
                } else {
                    prompt
                };
                self.auth_prompt = Some(AuthPrompt {
                    kind: AuthPromptKind::Keyboard,
                    title: format!("{label}: {prompt}"),
                    input: String::new(),
                    masked: !echo,
                });
                self.status = "Server asks for input. Esc cancels.".to_string();
            }
        }
        Ok(())
    }

    fn answer_auth(&mut self, kind: AuthPromptKind, secret: Option<String>) -> Result<()> {
        let Some(mut pending) = self.pending.take() else {
            return Ok(());
        };
        let step = self.runtime.block_on(async {
            match (kind, secret) {
                (AuthPromptKind::Passphrase, secret) => pending.flow.with_passphrase(secret).await,
                (AuthPromptKind::Password, Some(secret)) => pending.flow.with_password(secret).await,
                (AuthPromptKind::Keyboard, Some(secret)) => {
                    pending.flow.with_keyboard_answer(secret).await
                }
                (_, None) => Err(anyhow::anyhow!("connection cancelled")),
            }
        });
        self.pending = Some(pending);
        self.drive_auth(step)
    }

    fn after_auth(&mut self) -> Result<()> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        match pending.stage {
            Stage::Jump => {
                let PendingConnect {
                    flow, target, note, ..
                } = pending;
                let jump_handle = flow.handle;
                let config =
                    client_config(target.server_alive_interval, target.server_alive_count_max);
                let opened = self.runtime.block_on(open_via_jump(
                    &jump_handle,
                    &target.hostname,
                    target.port,
                    config,
                    note.clone(),
                ));
                let handle = match opened {
                    Ok(handle) => handle,
                    Err(err) => {
                        self.disconnect_handle(jump_handle);
                        return Err(err);
                    }
                };
                let flow = AuthFlow::new(
                    handle,
                    target.user.clone(),
                    &target.hostname,
                    target.identity_files.clone(),
                    target.identities_only,
                );
                let mut next = PendingConnect {
                    flow,
                    stage: Stage::Target,
                    jump: Some(jump_handle),
                    target,
                    note,
                };
                let step = self.runtime.block_on(next.flow.run());
                self.pending = Some(next);
                self.drive_auth(step)
            }
            Stage::Target => {
                let opened = self.runtime.block_on(open_sftp(&pending.flow.handle));
                let (sftp, remote_pwd) = match opened {
                    Ok(v) => v,
                    Err(err) => {
                        self.pending = Some(pending);
                        self.abort_pending();
                        return Err(err);
                    }
                };
                let PendingConnect {
                    flow,
                    jump,
                    target,
                    note,
                    ..
                } = pending;
                self.host = target.hostname.clone();
                self.user = target.user.clone();
                self.port = target.port.to_string();
                self.ssh = Some(flow.handle);
                self.jump_ssh = jump;
                let via = if self.jump_ssh.is_some() {
                    " (via ProxyJump)"
                } else {
                    ""
                };
                let note = note
                    .lock()
                    .ok()
                    .and_then(|slot| slot.clone())
                    .unwrap_or_else(|| "Connected.".to_string());
                self.enter_browser(sftp, remote_pwd);
                self.status = format!(
                    "{} {note}{via} `Tab` switches panes, `:` runs commands.",
                    target.alias
                );
                self.refresh_remote()?;
                Ok(())
            }
        }
    }

    fn disconnect_handle(&self, handle: SshHandle) {
        let _ = self.runtime.block_on(async {
            handle
                .disconnect(Disconnect::ByApplication, "", "English")
                .await
        });
    }

    fn abort_pending(&mut self) {
        self.auth_prompt = None;
        if let Some(pending) = self.pending.take() {
            self.disconnect_handle(pending.flow.handle);
            if let Some(jump) = pending.jump {
                self.disconnect_handle(jump);
            }
        }
    }

    fn enter_browser(&mut self, sftp: SftpSession, remote_pwd: String) {
        self.sftp = Some(Arc::new(sftp));
        self.remote_path = if remote_pwd.trim().is_empty() {
            ".".to_string()
        } else {
            remote_pwd
        };
        self.remote_selected = 0;
        self.screen = Screen::Browser;
        self.browser_focus = BrowserFocus::Remote;
    }

    fn handle_auth_prompt_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(prompt) = self.auth_prompt.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                let kind = prompt.kind;
                self.auth_prompt = None;
                if kind == AuthPromptKind::Passphrase {
                    self.status = "Skipping key...".to_string();
                    self.deferred = Some(Deferred::AuthAnswer { kind, secret: None });
                } else {
                    self.abort_pending();
                    self.status = "Connection cancelled.".to_string();
                }
            }
            KeyCode::Enter => {
                let kind = prompt.kind;
                let secret = std::mem::take(&mut prompt.input);
                self.auth_prompt = None;
                self.status = "Authenticating...".to_string();
                self.deferred = Some(Deferred::AuthAnswer {
                    kind,
                    secret: Some(secret),
                });
            }
            KeyCode::Backspace => {
                prompt.input.pop();
            }
            KeyCode::Char('u') if ctrl => prompt.input.clear(),
            KeyCode::Char(c) if !ctrl => prompt.input.push(c),
            _ => {}
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent) {
        match self.picker.handle_key(key) {
            PickerAction::None => {}
            PickerAction::Quit => self.should_quit = true,
            PickerAction::Manual => {
                self.screen = Screen::Connect;
                self.connect_field = ConnectField::Host;
                self.status =
                    "Manual connection. Esc returns to the host list. Ctrl+L loads ssh.txt."
                        .to_string();
            }
            PickerAction::Reload => {
                self.config = SshConfig::load_default();
                self.picker = Picker::from_config(&self.config);
                self.status = format!("Reloaded ~/.ssh/config: {} host(s).", self.picker.entries.len());
            }
            PickerAction::Connect(alias) => self.request_alias_connect(alias, None),
        }
    }

    fn load_login_file(&mut self) -> Result<()> {
        let Some(path) = self.login_file_path.clone() else {
            bail!("No ssh.txt found in %USERPROFILE%, current directory, or parent directory");
        };
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read login file {}", path.display()))?;
        let lines: Vec<_> = content
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if lines.len() < 3 {
            bail!("login file must contain host, username, password, and optionally port");
        }
        self.host = lines[0].to_string();
        self.user = lines[1].to_string();
        self.password = lines[2].to_string();
        self.port = lines.get(3).unwrap_or(&DEFAULT_PORT).to_string();
        self.auth_method = AuthMethod::Password;
        self.status = format!("Loaded login details from {}", path.display());
        Ok(())
    }

    fn connect(&mut self) -> Result<()> {
        let host = self.host.trim().to_string();
        let user = self.user.trim().to_string();
        let password = self.password.clone();
        let key_path = self.key_path.trim().to_string();
        let passphrase = self.passphrase.clone();
        let auth_method = self.auth_method;
        let port = self
            .port
            .trim()
            .parse::<u16>()
            .with_context(|| format!("invalid port `{}`", self.port.trim()))?;

        if host.is_empty() || user.is_empty() {
            bail!("host and username are required");
        }
        if auth_method == AuthMethod::Key && key_path.is_empty() {
            bail!("a private key path is required for key authentication");
        }

        self.status = format!("Connecting to {host}:{port}...");

        let verification_note = Arc::new(Mutex::new(None));
        let verification_note_for_handler = verification_note.clone();
        let (ssh, sftp, remote_pwd) = self.runtime.block_on(async move {
            let config = client_config(None, None);
            // Same known_hosts trust-on-first-use handler as the alias path.
            let mut ssh =
                open_direct(&host, port, config, None, verification_note_for_handler).await?;

            let auth = match auth_method {
                AuthMethod::Password => ssh
                    .authenticate_password(user.as_str(), password)
                    .await
                    .context("password authentication failed")?,
                AuthMethod::Key => {
                    let passphrase_opt = if passphrase.is_empty() {
                        None
                    } else {
                        Some(passphrase.as_str())
                    };
                    let key = load_secret_key(&key_path, passphrase_opt)
                        .with_context(|| format!("failed to load private key {key_path}"))?;
                    let hash = ssh
                        .best_supported_rsa_hash()
                        .await
                        .context("failed to negotiate key hash")?
                        .flatten();
                    ssh.authenticate_publickey(
                        user.as_str(),
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    )
                    .await
                    .context("public key authentication failed")?
                }
            };
            if !auth.success() {
                bail!("authentication failed");
            }

            let (sftp, remote_pwd) = open_sftp(&ssh).await?;
            Ok::<_, anyhow::Error>((ssh, sftp, remote_pwd))
        })?;

        self.ssh = Some(ssh);
        self.jump_ssh = None;
        self.enter_browser(sftp, remote_pwd);
        let verification_note = verification_note
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .unwrap_or_else(|| "Connected.".to_string());
        self.status = format!("{verification_note} `Tab` switches panes, `:` runs commands.");
        self.refresh_remote()?;
        Ok(())
    }

    fn refresh_local(&mut self) -> Result<()> {
        self.local_entries = read_local_entries(&self.local_path)?;
        self.clamp_local_selection();
        Ok(())
    }

    fn refresh_remote(&mut self) -> Result<()> {
        let Some(sftp) = self.sftp.clone() else {
            bail!("not connected");
        };
        let remote_path = self.remote_path.clone();
        self.remote_entries = self
            .runtime
            .block_on(read_remote_entries(sftp.as_ref(), &remote_path))?;
        self.clamp_remote_selection();
        Ok(())
    }

    fn clamp_local_selection(&mut self) {
        let len = self.local_visible().len();
        if self.local_selected >= len {
            self.local_selected = len.saturating_sub(1);
        }
    }

    fn clamp_remote_selection(&mut self) {
        let len = self.remote_visible().len();
        if self.remote_selected >= len {
            self.remote_selected = len.saturating_sub(1);
        }
    }

    fn local_visible(&self) -> Vec<Entry> {
        visible_entries(&self.local_entries, &self.local_filter)
    }

    fn remote_visible(&self) -> Vec<Entry> {
        visible_entries(&self.remote_entries, &self.remote_filter)
    }

    fn selected_local_entry(&self) -> Option<Entry> {
        self.local_visible().get(self.local_selected).cloned()
    }

    fn selected_remote_entry(&self) -> Result<Entry> {
        self.remote_visible()
            .get(self.remote_selected)
            .cloned()
            .context("no remote item selected")
    }

    fn handle_event(&mut self, event: Event) -> Result<()> {
        if let Event::Key(key) = event {
            if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                return Ok(());
            }
            if self.deferred.is_some() {
                return Ok(()); // busy: ignore keys until queued work runs
            }
            if self.transfer.is_some() {
                self.handle_transfer_key(key);
                return Ok(());
            }
            if self.auth_prompt.is_some() {
                self.handle_auth_prompt_key(key);
                return Ok(());
            }
            if self.prompt.is_some() {
                return self.handle_prompt_key(key);
            }
            match self.screen {
                Screen::Picker => self.handle_picker_key(key),
                Screen::Connect => self.handle_connect_key(key)?,
                Screen::Browser => self.handle_browser_key(key)?,
            }
        }
        Ok(())
    }

    fn handle_transfer_key(&mut self, key: KeyEvent) {
        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('q')
        ) {
            if let Some(job) = self.transfer.take() {
                job.handle.abort();
                self.status = format!("{} cancelled.", job.verb);
            }
        }
    }

    fn connect_fields(&self) -> Vec<ConnectField> {
        match self.auth_method {
            AuthMethod::Password => vec![
                ConnectField::Host,
                ConnectField::User,
                ConnectField::Auth,
                ConnectField::Password,
                ConnectField::Port,
            ],
            AuthMethod::Key => vec![
                ConnectField::Host,
                ConnectField::User,
                ConnectField::Auth,
                ConnectField::KeyPath,
                ConnectField::Passphrase,
                ConnectField::Port,
            ],
        }
    }

    fn cycle_connect_field(&mut self, delta: isize) {
        let fields = self.connect_fields();
        let current = fields
            .iter()
            .position(|f| *f == self.connect_field)
            .unwrap_or(0) as isize;
        let len = fields.len() as isize;
        let next = ((current + delta) % len + len) % len;
        self.connect_field = fields[next as usize];
    }

    fn connect_text_field_mut(&mut self) -> Option<&mut String> {
        match self.connect_field {
            ConnectField::Host => Some(&mut self.host),
            ConnectField::User => Some(&mut self.user),
            ConnectField::Password => Some(&mut self.password),
            ConnectField::KeyPath => Some(&mut self.key_path),
            ConnectField::Passphrase => Some(&mut self.passphrase),
            ConnectField::Port => Some(&mut self.port),
            ConnectField::Auth => None,
        }
    }

    fn handle_connect_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                if self.picker.entries.is_empty() {
                    self.should_quit = true;
                } else {
                    self.screen = Screen::Picker;
                    self.status = "Host list. Enter connects, m for a manual host.".to_string();
                }
            }
            KeyCode::Tab => self.cycle_connect_field(1),
            KeyCode::BackTab => self.cycle_connect_field(-1),
            KeyCode::Enter => {
                let host = self.host.trim().to_string();
                let is_alias = self
                    .picker
                    .entries
                    .iter()
                    .any(|e| e.alias.eq_ignore_ascii_case(&host));
                if is_alias && self.user.trim().is_empty() {
                    // Typing a config alias behaves like `ssh <alias>`.
                    self.request_alias_connect(host, None);
                } else {
                    self.status = format!("Connecting to {host}...");
                    self.deferred = Some(Deferred::ConnectManual);
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self.connect_field == ConnectField::Auth =>
            {
                self.auth_method = self.auth_method.toggled();
                self.connect_field = ConnectField::Auth;
                self.status = format!("Auth method: {}", self.auth_method.label());
            }
            KeyCode::Char('l') | KeyCode::Char('L') if ctrl => {
                if let Err(err) = self.load_login_file() {
                    self.status = err.to_string();
                }
            }
            KeyCode::Backspace => {
                if let Some(field) = self.connect_text_field_mut() {
                    field.pop();
                }
            }
            KeyCode::Char(c) if !ctrl => {
                if let Some(field) = self.connect_text_field_mut() {
                    field.push(c);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_browser_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            self.should_quit = true;
            return Ok(());
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Tab => {
                self.browser_focus = match self.browser_focus {
                    BrowserFocus::Local => BrowserFocus::Remote,
                    BrowserFocus::Remote => BrowserFocus::Local,
                };
            }
            KeyCode::Char(':') => self.start_command_prompt(),
            KeyCode::Char('/') => self.start_filter_prompt(),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.open_selected()?,
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.go_parent()?,
            KeyCode::Char('r') => {
                self.refresh_local()?;
                self.refresh_remote()?;
                self.status = "Refreshed local and remote views.".to_string();
            }
            KeyCode::Char('u') => self.upload_selected_local()?,
            KeyCode::Char('d') => self.download_selected_remote()?,
            KeyCode::Char(' ') => match self.browser_focus {
                BrowserFocus::Local => self.upload_selected_local()?,
                BrowserFocus::Remote => self.download_selected_remote()?,
            },
            KeyCode::Char('n') => self.start_mkdir_prompt(),
            KeyCode::Char('R') => self.start_rename_prompt()?,
            KeyCode::Char('x') => self.start_delete_prompt()?,
            _ => {}
        }
        Ok(())
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                let cleared_filter = match &self.prompt {
                    Some(prompt) => {
                        if let PromptKind::Filter { focus } = prompt.kind {
                            match focus {
                                BrowserFocus::Local => {
                                    self.local_filter.clear();
                                    self.local_selected = 0;
                                }
                                BrowserFocus::Remote => {
                                    self.remote_filter.clear();
                                    self.remote_selected = 0;
                                }
                            }
                            true
                        } else {
                            false
                        }
                    }
                    None => false,
                };
                self.prompt = None;
                self.status = if cleared_filter {
                    "Filter cleared.".to_string()
                } else {
                    "Prompt cancelled.".to_string()
                };
            }
            KeyCode::Enter => {
                self.submit_prompt()?;
            }
            KeyCode::Backspace => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.input.pop();
                }
                self.apply_live_filter();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.input.push(c);
                }
                self.apply_live_filter();
            }
            _ => {}
        }
        Ok(())
    }

    fn apply_live_filter(&mut self) {
        let Some(prompt) = self.prompt.as_ref() else {
            return;
        };
        if let PromptKind::Filter { focus } = prompt.kind {
            let value = prompt.input.clone();
            match focus {
                BrowserFocus::Local => {
                    self.local_filter = value;
                    self.local_selected = 0;
                }
                BrowserFocus::Remote => {
                    self.remote_filter = value;
                    self.remote_selected = 0;
                }
            }
        }
    }

    fn start_command_prompt(&mut self) {
        self.prompt = Some(PromptState {
            kind: PromptKind::Command,
            input: self.command_input.clone(),
            title: "Remote Command".to_string(),
        });
        self.status = "Enter a remote shell command.".to_string();
    }

    fn start_filter_prompt(&mut self) {
        let (focus, current) = match self.browser_focus {
            BrowserFocus::Local => (BrowserFocus::Local, self.local_filter.clone()),
            BrowserFocus::Remote => (BrowserFocus::Remote, self.remote_filter.clone()),
        };
        self.prompt = Some(PromptState {
            kind: PromptKind::Filter { focus },
            input: current,
            title: "Filter (Enter keeps, Esc clears)".to_string(),
        });
        self.status = "Type to filter the focused pane.".to_string();
    }

    fn start_mkdir_prompt(&mut self) {
        let kind = match self.browser_focus {
            BrowserFocus::Local => PromptKind::LocalMkdir,
            BrowserFocus::Remote => PromptKind::RemoteMkdir,
        };
        self.prompt = Some(PromptState {
            kind,
            input: String::new(),
            title: "New Folder".to_string(),
        });
        self.status = "Enter a new folder name.".to_string();
    }

    fn start_rename_prompt(&mut self) -> Result<()> {
        match self.browser_focus {
            BrowserFocus::Local => {
                let entry = self
                    .selected_local_entry()
                    .context("select a local item to rename")?;
                if entry.name == ".." {
                    bail!("select a local item to rename");
                }
                self.prompt = Some(PromptState {
                    kind: PromptKind::LocalRename {
                        old_path: PathBuf::from(entry.path.clone()),
                        old_name: entry.name.clone(),
                    },
                    input: entry.name.clone(),
                    title: "Rename Local Item".to_string(),
                });
            }
            BrowserFocus::Remote => {
                let entry = self.selected_remote_entry()?;
                if entry.name == ".." {
                    bail!("select a remote item to rename");
                }
                self.prompt = Some(PromptState {
                    kind: PromptKind::RemoteRename {
                        old_path: entry.path.clone(),
                        old_name: entry.name.clone(),
                    },
                    input: entry.name.clone(),
                    title: "Rename Remote Item".to_string(),
                });
            }
        }
        self.status = "Enter the new item name.".to_string();
        Ok(())
    }

    fn start_delete_prompt(&mut self) -> Result<()> {
        match self.browser_focus {
            BrowserFocus::Local => {
                let entry = self
                    .selected_local_entry()
                    .context("select a local item to delete")?;
                if entry.name == ".." {
                    bail!("select a local item to delete");
                }
                self.prompt = Some(PromptState {
                    kind: PromptKind::LocalDelete {
                        target_path: PathBuf::from(entry.path.clone()),
                        is_dir: entry.is_dir,
                        name: entry.name.clone(),
                    },
                    input: String::new(),
                    title: format!("Delete local {}? Type `yes`", entry.name),
                });
            }
            BrowserFocus::Remote => {
                let entry = self.selected_remote_entry()?;
                if entry.name == ".." {
                    bail!("select a remote item to delete");
                }
                self.prompt = Some(PromptState {
                    kind: PromptKind::RemoteDelete {
                        target_path: entry.path.clone(),
                        is_dir: entry.is_dir,
                        name: entry.name.clone(),
                    },
                    input: String::new(),
                    title: format!("Delete remote {}? Type `yes`", entry.name),
                });
            }
        }
        self.status = "Type `yes` and press Enter to delete.".to_string();
        Ok(())
    }

    fn submit_prompt(&mut self) -> Result<()> {
        let Some(prompt) = self.prompt.take() else {
            return Ok(());
        };
        match prompt.kind {
            PromptKind::Command => {
                let command = prompt.input.trim().to_string();
                self.command_input = command.clone();
                if !command.is_empty() {
                    self.run_remote_command(&command)?;
                } else {
                    self.status = "Command cancelled.".to_string();
                }
            }
            PromptKind::Filter { .. } => {
                self.status = "Filter applied.".to_string();
            }
            PromptKind::RemoteMkdir => {
                let name = prompt.input.trim();
                if name.is_empty() {
                    self.status = "Folder creation cancelled.".to_string();
                } else {
                    self.create_remote_dir(name)?;
                }
            }
            PromptKind::LocalMkdir => {
                let name = prompt.input.trim();
                if name.is_empty() {
                    self.status = "Folder creation cancelled.".to_string();
                } else {
                    self.create_local_dir(name)?;
                }
            }
            PromptKind::RemoteRename { old_path, old_name } => {
                let new_name = prompt.input.trim();
                if new_name.is_empty() || new_name == old_name {
                    self.status = "Rename cancelled.".to_string();
                } else {
                    self.rename_remote_item(&old_path, new_name)?;
                }
            }
            PromptKind::LocalRename { old_path, old_name } => {
                let new_name = prompt.input.trim();
                if new_name.is_empty() || new_name == old_name {
                    self.status = "Rename cancelled.".to_string();
                } else {
                    self.rename_local_item(&old_path, new_name)?;
                }
            }
            PromptKind::RemoteDelete {
                target_path,
                is_dir,
                name,
            } => {
                if prompt.input.trim() == "yes" {
                    self.delete_remote_item(&target_path, is_dir, &name)?;
                } else {
                    self.status = "Delete cancelled.".to_string();
                }
            }
            PromptKind::LocalDelete {
                target_path,
                is_dir,
                name,
            } => {
                if prompt.input.trim() == "yes" {
                    self.delete_local_item(&target_path, is_dir, &name)?;
                } else {
                    self.status = "Delete cancelled.".to_string();
                }
            }
            PromptKind::ConfirmUpload {
                local_path,
                remote_path,
                name,
            } => {
                self.begin_upload(local_path, remote_path, name)?;
            }
            PromptKind::ConfirmDownload {
                remote_path,
                local_path,
                name,
                is_dir,
                size,
            } => {
                self.begin_download(remote_path, local_path, name, is_dir, size)?;
            }
        }
        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        let len = match self.browser_focus {
            BrowserFocus::Local => self.local_visible().len(),
            BrowserFocus::Remote => self.remote_visible().len(),
        };
        let selected = match self.browser_focus {
            BrowserFocus::Local => &mut self.local_selected,
            BrowserFocus::Remote => &mut self.remote_selected,
        };

        if len == 0 {
            *selected = 0;
            return;
        }

        let current = *selected as isize;
        let next = (current + delta).clamp(0, len.saturating_sub(1) as isize) as usize;
        *selected = next;
    }

    fn open_selected(&mut self) -> Result<()> {
        match self.browser_focus {
            BrowserFocus::Local => {
                let Some(entry) = self.selected_local_entry() else {
                    return Ok(());
                };
                if entry.name == ".." {
                    self.go_parent()?;
                } else if entry.is_dir {
                    self.local_path = PathBuf::from(entry.path);
                    self.local_selected = 0;
                    self.local_filter.clear();
                    self.refresh_local()?;
                } else {
                    self.status = "Local file selected. Press `u` to upload it.".to_string();
                }
            }
            BrowserFocus::Remote => {
                let Some(entry) = self.remote_visible().get(self.remote_selected).cloned() else {
                    return Ok(());
                };
                if entry.name == ".." {
                    self.go_parent()?;
                } else if entry.is_dir {
                    self.remote_path = entry.path;
                    self.remote_selected = 0;
                    self.remote_filter.clear();
                    self.refresh_remote()?;
                } else {
                    self.status = "Remote file selected. Press `d` to download it.".to_string();
                }
            }
        }
        Ok(())
    }

    fn go_parent(&mut self) -> Result<()> {
        match self.browser_focus {
            BrowserFocus::Local => {
                if let Some(parent) = self.local_path.parent() {
                    self.local_path = parent.to_path_buf();
                    self.local_selected = 0;
                    self.local_filter.clear();
                    self.refresh_local()?;
                }
            }
            BrowserFocus::Remote => {
                self.remote_path = remote_parent(&self.remote_path);
                self.remote_selected = 0;
                self.remote_filter.clear();
                self.refresh_remote()?;
            }
        }
        Ok(())
    }

    fn upload_selected_local(&mut self) -> Result<()> {
        let entry = self
            .selected_local_entry()
            .context("no local item selected")?;
        if entry.name == ".." {
            bail!("select a file or folder to upload");
        }
        let Some(sftp) = self.sftp.clone() else {
            bail!("not connected");
        };
        let remote_path = join_remote(&self.remote_path, &entry.name);
        let exists = self
            .runtime
            .block_on(async { sftp.try_exists(remote_path.clone()).await })
            .unwrap_or(false);
        let local_path = PathBuf::from(entry.path);
        if exists {
            self.prompt = Some(PromptState {
                kind: PromptKind::ConfirmUpload {
                    local_path,
                    remote_path,
                    name: entry.name.clone(),
                },
                input: String::new(),
                title: format!("Remote {} exists. Overwrite?", entry.name),
            });
            self.status = "Enter to overwrite, Esc to cancel.".to_string();
            Ok(())
        } else {
            self.begin_upload(local_path, remote_path, entry.name)
        }
    }

    fn download_selected_remote(&mut self) -> Result<()> {
        let entry = self.selected_remote_entry()?;
        if entry.name == ".." {
            bail!("select a remote file or folder to download");
        }
        if self.sftp.is_none() {
            bail!("not connected");
        }
        let local_path = self.local_path.join(&entry.name);
        if local_path.exists() {
            self.prompt = Some(PromptState {
                kind: PromptKind::ConfirmDownload {
                    remote_path: entry.path.clone(),
                    local_path,
                    name: entry.name.clone(),
                    is_dir: entry.is_dir,
                    size: entry.size,
                },
                input: String::new(),
                title: format!("Local {} exists. Overwrite?", entry.name),
            });
            self.status = "Enter to overwrite, Esc to cancel.".to_string();
            Ok(())
        } else {
            self.begin_download(entry.path, local_path, entry.name, entry.is_dir, entry.size)
        }
    }

    fn begin_upload(
        &mut self,
        local_path: PathBuf,
        remote_path: String,
        name: String,
    ) -> Result<()> {
        let Some(sftp) = self.sftp.clone() else {
            bail!("not connected");
        };
        let progress = Arc::new(Mutex::new(Progress::new(format!("Uploading {name}"))));
        let task_progress = progress.clone();
        let handle = self
            .runtime
            .spawn(transfer_upload(sftp, local_path, remote_path, task_progress));
        self.transfer = Some(TransferJob {
            progress,
            handle,
            verb: "Upload",
        });
        self.status = format!("Uploading {name}... (c cancels)");
        Ok(())
    }

    fn begin_download(
        &mut self,
        remote_path: String,
        local_path: PathBuf,
        name: String,
        is_dir: bool,
        size: Option<u64>,
    ) -> Result<()> {
        let Some(sftp) = self.sftp.clone() else {
            bail!("not connected");
        };
        let progress = Arc::new(Mutex::new(Progress::new(format!("Downloading {name}"))));
        let task_progress = progress.clone();
        let handle = self.runtime.spawn(transfer_download(
            sftp,
            remote_path,
            local_path,
            is_dir,
            size,
            task_progress,
        ));
        self.transfer = Some(TransferJob {
            progress,
            handle,
            verb: "Download",
        });
        self.status = format!("Downloading {name}... (c cancels)");
        Ok(())
    }

    fn poll_transfer(&mut self) {
        let Some(job) = self.transfer.as_ref() else {
            return;
        };
        let finished = job
            .progress
            .lock()
            .map(|progress| progress.done)
            .unwrap_or(true);
        if !finished {
            return;
        }
        let job = self.transfer.take().unwrap();
        let (error, label) = job
            .progress
            .lock()
            .map(|progress| (progress.error.clone(), progress.label.clone()))
            .unwrap_or((None, job.verb.to_string()));
        let _ = self.runtime.block_on(async { job.handle.await });
        let _ = self.refresh_local();
        let _ = self.refresh_remote();
        self.status = match error {
            Some(err) => format!("{label} failed: {err}"),
            None => format!("{label} complete."),
        };
    }

    fn create_remote_dir(&mut self, name: &str) -> Result<()> {
        let Some(sftp) = self.sftp.clone() else {
            bail!("not connected");
        };
        let remote_path = join_remote(&self.remote_path, name);
        self.runtime.block_on(async {
            if sftp.try_exists(remote_path.clone()).await? {
                bail!("remote path already exists: {remote_path}");
            }
            sftp.create_dir(remote_path.clone())
                .await
                .with_context(|| format!("failed to create remote dir {remote_path}"))?;
            Ok::<_, anyhow::Error>(())
        })?;
        self.refresh_remote()?;
        self.status = format!("Created remote folder {remote_path}");
        Ok(())
    }

    fn create_local_dir(&mut self, name: &str) -> Result<()> {
        let target = self.local_path.join(name);
        if target.exists() {
            bail!("local path already exists: {}", target.display());
        }
        fs::create_dir(&target)
            .with_context(|| format!("failed to create {}", target.display()))?;
        self.refresh_local()?;
        self.status = format!("Created local folder {}", target.display());
        Ok(())
    }

    fn rename_remote_item(&mut self, old_path: &str, new_name: &str) -> Result<()> {
        let Some(sftp) = self.sftp.clone() else {
            bail!("not connected");
        };
        let new_path = join_remote(&remote_parent(old_path), new_name);
        self.runtime.block_on(async {
            if sftp.try_exists(new_path.clone()).await? {
                bail!("remote path already exists: {new_path}");
            }
            sftp.rename(old_path.to_string(), new_path.clone())
                .await
                .with_context(|| format!("failed to rename {old_path} to {new_path}"))?;
            Ok::<_, anyhow::Error>(())
        })?;
        self.refresh_remote()?;
        self.status = format!("Renamed {old_path} -> {new_path}");
        Ok(())
    }

    fn rename_local_item(&mut self, old_path: &Path, new_name: &str) -> Result<()> {
        let parent = old_path.parent().unwrap_or(Path::new("."));
        let new_path = parent.join(new_name);
        if new_path.exists() {
            bail!("local path already exists: {}", new_path.display());
        }
        fs::rename(old_path, &new_path).with_context(|| {
            format!(
                "failed to rename {} to {}",
                old_path.display(),
                new_path.display()
            )
        })?;
        self.refresh_local()?;
        self.status = format!("Renamed {} -> {}", old_path.display(), new_path.display());
        Ok(())
    }

    fn delete_remote_item(&mut self, target_path: &str, is_dir: bool, name: &str) -> Result<()> {
        let Some(sftp) = self.sftp.clone() else {
            bail!("not connected");
        };
        self.runtime
            .block_on(delete_remote_path_recursive(sftp.as_ref(), target_path, is_dir))?;
        self.refresh_remote()?;
        self.status = format!("Deleted remote item {name}");
        Ok(())
    }

    fn delete_local_item(&mut self, target_path: &Path, is_dir: bool, name: &str) -> Result<()> {
        if is_dir {
            fs::remove_dir_all(target_path)
                .with_context(|| format!("failed to delete {}", target_path.display()))?;
        } else {
            fs::remove_file(target_path)
                .with_context(|| format!("failed to delete {}", target_path.display()))?;
        }
        self.refresh_local()?;
        self.status = format!("Deleted local item {name}");
        Ok(())
    }

    fn run_remote_command(&mut self, command: &str) -> Result<()> {
        let Some(ssh) = self.ssh.as_mut() else {
            bail!("not connected");
        };
        let current_remote = self.remote_path.clone();
        let wrapped = format!(
            "cd -- {} 2>/dev/null || exit 1; {}; status=$?; printf '\\n__SSHGRAB_PWD__%s\\n' \"$PWD\"; printf '__SSHGRAB_EXIT__%s\\n' \"$status\"",
            shell_quote(&current_remote),
            command
        );

        let captured = self.runtime.block_on(exec_capture(ssh, &wrapped))?;

        self.push_output(format!("$ {command}"));

        let mut next_remote = self.remote_path.clone();
        for line in captured.lines() {
            if let Some(rest) = line.strip_prefix("__SSHGRAB_PWD__") {
                next_remote = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("__SSHGRAB_EXIT__") {
                if rest.trim() != "0" {
                    self.push_output(format!("[exit {}]", rest.trim()));
                }
            } else {
                self.push_output(line.to_string());
            }
        }

        self.remote_path = if next_remote.trim().is_empty() {
            self.remote_path.clone()
        } else {
            next_remote
        };
        self.refresh_remote()?;
        self.status = "Remote command finished.".to_string();
        Ok(())
    }

    fn push_output(&mut self, line: String) {
        if self.command_output.len() >= MAX_LOG_LINES {
            self.command_output.pop_front();
        }
        self.command_output.push_back(line);
    }
}

fn visible_entries(entries: &[Entry], filter: &str) -> Vec<Entry> {
    if filter.is_empty() {
        return entries.to_vec();
    }
    let needle = filter.to_lowercase();
    entries
        .iter()
        .filter(|entry| entry.name == ".." || entry.name.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

async fn exec_capture(ssh: &SshHandle, command: &str) -> Result<String> {
    let mut channel = ssh
        .channel_open_session()
        .await
        .context("failed to open remote exec channel")?;
    channel
        .exec(true, command)
        .await
        .context("failed to execute remote command")?;

    let mut captured = String::new();
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                captured.push_str(&String::from_utf8_lossy(&data));
            }
            ChannelMsg::ExitStatus { .. } => {}
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(captured)
}

async fn transfer_upload(
    sftp: Arc<SftpSession>,
    local: PathBuf,
    remote: String,
    progress: Arc<Mutex<Progress>>,
) {
    let result = async {
        let (files, bytes) = count_local(&local)?;
        if let Ok(mut guard) = progress.lock() {
            guard.files_total = files;
            guard.bytes_total = bytes;
        }
        upload_recursive(&sftp, &local, &remote, &progress).await
    }
    .await;
    finish_progress(&progress, result);
}

async fn transfer_download(
    sftp: Arc<SftpSession>,
    remote: String,
    local: PathBuf,
    is_dir: bool,
    size: Option<u64>,
    progress: Arc<Mutex<Progress>>,
) {
    let result = async {
        let (files, bytes) = if is_dir {
            count_remote(&sftp, &remote).await?
        } else {
            (1, size.unwrap_or(0))
        };
        if let Ok(mut guard) = progress.lock() {
            guard.files_total = files;
            guard.bytes_total = bytes;
        }
        download_recursive(&sftp, &remote, &local, is_dir, &progress).await
    }
    .await;
    finish_progress(&progress, result);
}

fn finish_progress(progress: &Arc<Mutex<Progress>>, result: Result<()>) {
    if let Ok(mut guard) = progress.lock() {
        if let Err(err) = result {
            guard.error = Some(err.to_string());
        }
        guard.done = true;
    }
}

fn count_local(path: &Path) -> Result<(u64, u64)> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to read {}", path.display()))?;
    if metadata.is_file() {
        return Ok((1, metadata.len()));
    }
    let mut files = 0;
    let mut bytes = 0;
    for entry in
        fs::read_dir(path).with_context(|| format!("failed to list {}", path.display()))?
    {
        let entry = entry?;
        let (child_files, child_bytes) = count_local(&entry.path())?;
        files += child_files;
        bytes += child_bytes;
    }
    Ok((files, bytes))
}

async fn count_remote(sftp: &SftpSession, path: &str) -> Result<(u64, u64)> {
    let mut files = 0;
    let mut bytes = 0;
    let entries = sftp
        .read_dir(path.to_string())
        .await
        .with_context(|| format!("failed to list remote path {path}"))?;
    for entry in entries {
        let name = entry.file_name();
        let child = join_remote(path, &name);
        if entry.file_type() == FileType::Dir {
            let (child_files, child_bytes) = Box::pin(count_remote(sftp, &child)).await?;
            files += child_files;
            bytes += child_bytes;
        } else {
            files += 1;
            bytes += entry.metadata().len();
        }
    }
    Ok((files, bytes))
}

async fn upload_recursive(
    sftp: &SftpSession,
    local_path: &Path,
    remote_path: &str,
    progress: &Arc<Mutex<Progress>>,
) -> Result<()> {
    let metadata = fs::metadata(local_path)
        .with_context(|| format!("failed to read {}", local_path.display()))?;
    if metadata.is_dir() {
        if !sftp.try_exists(remote_path.to_string()).await.unwrap_or(false) {
            sftp.create_dir(remote_path.to_string())
                .await
                .with_context(|| format!("failed to create remote dir {remote_path}"))?;
        }
        for entry in fs::read_dir(local_path)
            .with_context(|| format!("failed to list {}", local_path.display()))?
        {
            let entry = entry?;
            let child_name = entry.file_name().to_string_lossy().to_string();
            let child_local = entry.path();
            let child_remote = join_remote(remote_path, &child_name);
            Box::pin(upload_recursive(sftp, &child_local, &child_remote, progress)).await?;
        }
    } else {
        if let Ok(mut guard) = progress.lock() {
            guard.current = remote_path.to_string();
        }
        let mut local_file = tokio::fs::File::open(local_path)
            .await
            .with_context(|| format!("failed to open {}", local_path.display()))?;
        let mut remote_file = sftp
            .create(remote_path.to_string())
            .await
            .with_context(|| format!("failed to create remote file {remote_path}"))?;
        let mut buffer = vec![0u8; TRANSFER_CHUNK];
        loop {
            let read = local_file
                .read(&mut buffer)
                .await
                .with_context(|| format!("failed to read {}", local_path.display()))?;
            if read == 0 {
                break;
            }
            remote_file
                .write_all(&buffer[..read])
                .await
                .with_context(|| format!("failed to upload to {remote_path}"))?;
            if let Ok(mut guard) = progress.lock() {
                guard.bytes_done += read as u64;
            }
        }
        remote_file.shutdown().await.ok();
        if let Ok(mut guard) = progress.lock() {
            guard.files_done += 1;
        }
    }
    Ok(())
}

async fn download_recursive(
    sftp: &SftpSession,
    remote_path: &str,
    local_path: &Path,
    is_dir: bool,
    progress: &Arc<Mutex<Progress>>,
) -> Result<()> {
    if is_dir {
        fs::create_dir_all(local_path)
            .with_context(|| format!("failed to create {}", local_path.display()))?;
        let entries = sftp
            .read_dir(remote_path.to_string())
            .await
            .with_context(|| format!("failed to list remote path {remote_path}"))?;
        for entry in entries {
            let name = entry.file_name();
            let child_remote = join_remote(remote_path, &name);
            let child_local = local_path.join(&name);
            let child_is_dir = entry.file_type() == FileType::Dir;
            Box::pin(download_recursive(
                sftp,
                &child_remote,
                &child_local,
                child_is_dir,
                progress,
            ))
            .await?;
        }
    } else {
        if let Ok(mut guard) = progress.lock() {
            guard.current = remote_path.to_string();
        }
        let mut remote_file = sftp
            .open(remote_path.to_string())
            .await
            .with_context(|| format!("failed to open remote file {remote_path}"))?;
        let mut local_file = tokio::fs::File::create(local_path)
            .await
            .with_context(|| format!("failed to create {}", local_path.display()))?;
        let mut buffer = vec![0u8; TRANSFER_CHUNK];
        loop {
            let read = remote_file
                .read(&mut buffer)
                .await
                .with_context(|| format!("failed to read remote file {remote_path}"))?;
            if read == 0 {
                break;
            }
            local_file
                .write_all(&buffer[..read])
                .await
                .with_context(|| format!("failed to write {}", local_path.display()))?;
            if let Ok(mut guard) = progress.lock() {
                guard.bytes_done += read as u64;
            }
        }
        local_file.shutdown().await.ok();
        if let Ok(mut guard) = progress.lock() {
            guard.files_done += 1;
        }
    }
    Ok(())
}

async fn delete_remote_path_recursive(
    sftp: &SftpSession,
    remote_path: &str,
    is_dir: bool,
) -> Result<()> {
    if is_dir {
        let entries = sftp
            .read_dir(remote_path.to_string())
            .await
            .with_context(|| format!("failed to list remote path {remote_path}"))?;
        for entry in entries {
            let name = entry.file_name();
            let child_remote = join_remote(remote_path, &name);
            let child_is_dir = entry.file_type() == FileType::Dir;
            Box::pin(delete_remote_path_recursive(sftp, &child_remote, child_is_dir)).await?;
        }
        sftp.remove_dir(remote_path.to_string())
            .await
            .with_context(|| format!("failed to remove remote dir {remote_path}"))?;
    } else {
        sftp.remove_file(remote_path.to_string())
            .await
            .with_context(|| format!("failed to remove remote file {remote_path}"))?;
    }
    Ok(())
}

const HELP_TEXT: &str = "\
sshgrab (ssh_grabber_rs) - terminal SSH/SFTP client

USAGE:
    ssh_grabber_rs [ALIAS | USER@ALIAS] [START_DIR]
    ssh_grabber_rs --list-hosts
    ssh_grabber_rs --help

ARGUMENTS:
    ALIAS        Host alias from ~/.ssh/config (or a plain hostname); connects
                 straight away, like `ssh ALIAS`.
    START_DIR    Existing local directory for the local pane. An argument that
                 is an existing directory is always treated as START_DIR.

OPTIONS:
    --list-hosts Print the aliases in ~/.ssh/config with their resolved
                 user@host:port and whether each IdentityFile exists, then exit.
                 Key contents are never read or printed.
    -h, --help   Show this help.

Without an alias, a host picker lists the ~/.ssh/config aliases (`hpc` first).
Authentication order: ssh-agent / Windows OpenSSH agent / Pageant, then each
IdentityFile (passphrase prompt if encrypted), then password or
keyboard-interactive. Host keys are checked against ~/.ssh/known_hosts
(trust on first use; a changed key is refused).";

struct CliArgs {
    start_dir: Option<PathBuf>,
    alias: Option<String>,
    user: Option<String>,
}

enum CliCommand {
    Run(CliArgs),
    Help,
    ListHosts,
}

fn parse_cli(args: &[String]) -> Result<CliCommand> {
    let mut parsed = CliArgs {
        start_dir: None,
        alias: None,
        user: None,
    };
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" | "/?" => return Ok(CliCommand::Help),
            "--list-hosts" => return Ok(CliCommand::ListHosts),
            other if other.starts_with('-') => bail!("unknown option `{other}` (see --help)"),
            other => {
                if Path::new(other).is_dir() {
                    parsed.start_dir = Some(PathBuf::from(other));
                } else if parsed.alias.is_none() {
                    match other.rsplit_once('@') {
                        Some((user, host)) if !user.is_empty() && !host.is_empty() => {
                            parsed.user = Some(user.to_string());
                            parsed.alias = Some(host.to_string());
                        }
                        _ => parsed.alias = Some(other.to_string()),
                    }
                } else {
                    bail!("unexpected argument `{other}` (see --help)");
                }
            }
        }
    }
    Ok(CliCommand::Run(parsed))
}

fn list_hosts() {
    let config = SshConfig::load_default();
    let config_path = dirs::home_dir().map(|h| h.join(".ssh").join("config"));
    match &config_path {
        Some(path) if path.exists() => println!("ssh config: {}", path.display()),
        Some(path) => println!("ssh config: {} (not found)", path.display()),
        None => println!("ssh config: home directory not found"),
    }
    for warning in &config.warnings {
        println!("warning: {warning}");
    }
    let aliases = config.concrete_aliases();
    if aliases.is_empty() {
        println!("(no concrete Host aliases)");
        return;
    }
    let user = local_username();
    let width = aliases.iter().map(|a| a.len()).max().unwrap_or(4);
    for alias in aliases {
        let r = config.resolve(&alias, &user);
        let mut line = format!("{alias:<width$}  {}", r.summary());
        if let Some(jump) = &r.proxy_jump {
            match parse_proxy_jump(jump) {
                Ok(_) => line.push_str(&format!("  via ProxyJump {jump}")),
                Err(err) => line.push_str(&format!("  [{err}]")),
            }
        }
        if r.proxy_command.is_some() {
            line.push_str("  [ProxyCommand: not supported yet]");
        }
        if r.identities_only {
            line.push_str("  IdentitiesOnly");
        }
        if let Some(secs) = r.server_alive_interval {
            line.push_str(&format!("  ServerAliveInterval {secs}"));
        }
        println!("{line}");
        let label = if r.identity_files_explicit {
            "IdentityFile"
        } else {
            "default key"
        };
        for key in &r.identity_files {
            let state = if key.is_file() { "exists" } else { "missing" };
            println!("    {label}: {} [{state}]", key.display());
        }
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_cli(&args) {
        Ok(CliCommand::Help) => {
            println!("{HELP_TEXT}");
            return Ok(());
        }
        Ok(CliCommand::ListHosts) => {
            list_hosts();
            return Ok(());
        }
        Ok(CliCommand::Run(cli)) => cli,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    };
    let mut app = App::new(cli.start_dir)?;
    if let Some(alias) = cli.alias {
        app.request_alias_connect(alias, cli.user);
    }
    let mut terminal = setup_terminal()?;
    let result = run_app(&mut terminal, &mut app);
    if let Some(job) = app.transfer.take() {
        job.handle.abort();
    }
    app.abort_pending();
    if let Some(ssh) = app.ssh.take() {
        app.disconnect_handle(ssh);
    }
    if let Some(jump) = app.jump_ssh.take() {
        app.disconnect_handle(jump);
    }
    restore_terminal(&mut terminal)?;
    result
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    while !app.should_quit {
        app.poll_transfer();
        terminal.draw(|frame| render(frame, app))?;
        if app.deferred.is_some() {
            app.run_deferred();
            continue;
        }
        if event::poll(Duration::from_millis(100))? {
            let ev = event::read()?;
            if let Err(err) = app.handle_event(ev) {
                app.status = err.to_string();
            }
        }
    }
    Ok(())
}

fn render(frame: &mut Frame, app: &App) {
    match app.screen {
        Screen::Picker => {
            let area = centered_rect(84, 80, frame.area());
            frame.render_widget(Clear, area);
            app.picker.render(frame, area, &app.status);
        }
        Screen::Connect => render_connect(frame, app),
        Screen::Browser => render_browser(frame, app),
    }
    if let Some(prompt) = &app.auth_prompt {
        render_auth_prompt(frame, prompt);
    }
}

fn render_auth_prompt(frame: &mut Frame, prompt: &AuthPrompt) {
    let area = centered_rect(60, 24, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(prompt.title.as_str())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    frame.render_widget(block, area);
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Length(3), Constraint::Min(2)])
        .split(area);
    let shown = if prompt.masked {
        "*".repeat(prompt.input.chars().count())
    } else {
        prompt.input.clone()
    };
    frame.render_widget(
        Paragraph::new(shown).block(Block::default().borders(Borders::ALL)),
        inner[0],
    );
    let hint = match prompt.kind {
        AuthPromptKind::Passphrase => "Enter submits. Esc skips this key. Not stored.",
        _ => "Enter submits. Esc cancels the connection. Not stored.",
    };
    frame.render_widget(Paragraph::new(hint).wrap(Wrap { trim: true }), inner[1]);
}

fn render_connect(frame: &mut Frame, app: &App) {
    let area = centered_rect(72, 70, frame.area());
    frame.render_widget(Clear, area);

    let outer = Block::default()
        .title(format!("{APP_TITLE}  connect"))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(outer, area);

    let mut rows: Vec<(String, String, bool, bool)> = vec![
        (
            "Host".to_string(),
            app.host.clone(),
            app.connect_field == ConnectField::Host,
            false,
        ),
        (
            "Username".to_string(),
            app.user.clone(),
            app.connect_field == ConnectField::User,
            false,
        ),
        (
            "Auth method (Space toggles)".to_string(),
            format!("< {} >", app.auth_method.label()),
            app.connect_field == ConnectField::Auth,
            false,
        ),
    ];
    match app.auth_method {
        AuthMethod::Password => rows.push((
            "Password".to_string(),
            app.password.clone(),
            app.connect_field == ConnectField::Password,
            true,
        )),
        AuthMethod::Key => {
            rows.push((
                "Private key path".to_string(),
                app.key_path.clone(),
                app.connect_field == ConnectField::KeyPath,
                false,
            ));
            rows.push((
                "Key passphrase (optional)".to_string(),
                app.passphrase.clone(),
                app.connect_field == ConnectField::Passphrase,
                true,
            ));
        }
    }
    rows.push((
        "Port".to_string(),
        app.port.clone(),
        app.connect_field == ConnectField::Port,
        false,
    ));

    let mut constraints = vec![Constraint::Length(3)];
    for _ in &rows {
        constraints.push(Constraint::Length(3));
    }
    constraints.push(Constraint::Min(4));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints(constraints)
        .split(area);

    let help = Paragraph::new(vec![
        Line::from("A terminal SSH/SFTP client. Tab / Shift+Tab move between fields."),
        Line::from(
            "Space toggles auth on the Auth line. Ctrl+L loads ssh.txt. Enter connects (a ~/.ssh/config alias with an empty username connects like `ssh <alias>`). Esc: host list / quit.",
        ),
    ])
    .wrap(Wrap { trim: true });
    frame.render_widget(help, chunks[0]);

    for (index, (title, value, active, masked)) in rows.iter().enumerate() {
        render_input(frame, chunks[index + 1], title, value, *active, *masked);
    }

    let status = Paragraph::new(app.status.as_str())
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Status").borders(Borders::ALL));
    frame.render_widget(status, chunks[rows.len() + 1]);
}

fn render_browser(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(10),
            Constraint::Length(3),
            Constraint::Length(8),
        ])
        .split(frame.area());

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            APP_TITLE,
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::raw(format!(
            "{}@{}  remote:{}  local:{}",
            app.user,
            app.host,
            app.remote_path,
            app.local_path.display()
        )),
    ]));
    frame.render_widget(header, chunks[0]);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    render_entries(
        frame,
        panes[0],
        "Local",
        &app.local_visible(),
        app.local_selected,
        app.browser_focus == BrowserFocus::Local,
        &app.local_filter,
    );
    render_entries(
        frame,
        panes[1],
        "Remote",
        &app.remote_visible(),
        app.remote_selected,
        app.browser_focus == BrowserFocus::Remote,
        &app.remote_filter,
    );

    let command_value = app
        .prompt
        .as_ref()
        .filter(|prompt| matches!(prompt.kind, PromptKind::Command))
        .map(|prompt| prompt.input.as_str())
        .unwrap_or(app.command_input.as_str());
    let command_active = app
        .prompt
        .as_ref()
        .is_some_and(|prompt| matches!(prompt.kind, PromptKind::Command));
    let command = Paragraph::new(command_value).block(
        Block::default()
            .title(if command_active {
                "Command (active)"
            } else {
                "Command (`:` to edit)"
            })
            .borders(Borders::ALL)
            .border_style(highlight_style(command_active)),
    );
    frame.render_widget(command, chunks[2]);

    let footer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(chunks[3]);

    let log_items: Vec<ListItem> = app
        .command_output
        .iter()
        .rev()
        .take(6)
        .map(|line| ListItem::new(line.clone()))
        .collect();
    let log = List::new(log_items).block(Block::default().title("Output").borders(Borders::ALL));
    frame.render_widget(log, footer[0]);

    let help = Paragraph::new(vec![
        Line::from("Tab switch  Enter open  Bksp up"),
        Line::from("u upload  d download  space=transfer  / filter"),
        Line::from("n mkdir  R rename  x delete"),
        Line::from(": command  r refresh  q quit"),
        Line::from(format!("Status: {}", app.status)),
    ])
    .wrap(Wrap { trim: true })
    .block(Block::default().title("Keys").borders(Borders::ALL));
    frame.render_widget(help, footer[1]);

    if let Some(prompt) = &app.prompt {
        render_prompt(frame, prompt);
    }
    if let Some(job) = &app.transfer {
        render_transfer(frame, job);
    }
}

fn render_input(frame: &mut Frame, area: Rect, title: &str, value: &str, active: bool, masked: bool) {
    let shown = if masked {
        "*".repeat(value.chars().count())
    } else {
        value.to_string()
    };
    let paragraph = Paragraph::new(shown).block(
        Block::default()
            .title(title.to_string())
            .borders(Borders::ALL)
            .border_style(highlight_style(active)),
    );
    frame.render_widget(paragraph, area);
}

fn render_entries(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    entries: &[Entry],
    selected: usize,
    active: bool,
    filter: &str,
) {
    let items: Vec<ListItem> = entries
        .iter()
        .map(|entry| {
            let icon = if entry.is_dir { "[D]" } else { "[F]" };
            let modified = entry.modified.as_deref().unwrap_or("-");
            let size = entry
                .size
                .map(human_size)
                .unwrap_or_else(|| "-".to_string());
            ListItem::new(format!("{icon:<3} {:<36} {:>8}  {modified}", entry.name, size))
        })
        .collect();

    let title = if filter.is_empty() {
        format!("{title} ({})", entries.len())
    } else {
        format!("{title} ({}) /{}", entries.len(), filter)
    };

    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(highlight_style(active)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    let mut state = ListState::default();
    if !entries.is_empty() {
        state.select(Some(selected.min(entries.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_prompt(frame: &mut Frame, prompt: &PromptState) {
    let area = centered_rect(60, 20, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(prompt.title.as_str())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    frame.render_widget(block, area);
    let inner = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Length(3), Constraint::Min(2)])
        .split(area);
    frame.render_widget(
        Paragraph::new(prompt.input.as_str()).block(Block::default().borders(Borders::ALL)),
        inner[0],
    );
    frame.render_widget(
        Paragraph::new("Enter submits. Esc cancels.").wrap(Wrap { trim: true }),
        inner[1],
    );
}

fn render_transfer(frame: &mut Frame, job: &TransferJob) {
    let area = centered_rect(60, 24, frame.area());
    frame.render_widget(Clear, area);
    let Ok(progress) = job.progress.lock() else {
        return;
    };
    let block = Block::default()
        .title(progress.label.as_str())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
    frame.render_widget(block, area);

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(1),
        ])
        .split(area);

    let ratio = if progress.bytes_total > 0 {
        (progress.bytes_done as f64 / progress.bytes_total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL))
        .gauge_style(Style::default().fg(Color::Green))
        .ratio(ratio)
        .label(format!("{:.0}%", ratio * 100.0));
    frame.render_widget(gauge, inner[0]);

    let detail = Paragraph::new(format!(
        "files {}/{}   {} / {}",
        progress.files_done,
        progress.files_total,
        human_size(progress.bytes_done),
        human_size(progress.bytes_total)
    ));
    frame.render_widget(detail, inner[1]);

    let current = Paragraph::new(format!("{}\nPress `c` or Esc to cancel.", progress.current))
        .wrap(Wrap { trim: true });
    frame.render_widget(current, inner[2]);
}

fn highlight_style(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("failed to initialize terminal")
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn read_local_entries(path: &Path) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    if path.parent().is_some() {
        entries.push(Entry {
            name: "..".to_string(),
            path: path.parent().unwrap_or(path).display().to_string(),
            is_dir: true,
            size: None,
            modified: None,
        });
    }

    let mut items: Vec<Entry> = fs::read_dir(path)
        .with_context(|| format!("failed to list {}", path.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let metadata = entry.metadata().ok()?;
            Some(Entry {
                name: entry.file_name().to_string_lossy().to_string(),
                path: entry.path().display().to_string(),
                is_dir: metadata.is_dir(),
                size: metadata.is_file().then_some(metadata.len()),
                modified: metadata.modified().ok().map(format_system_time),
            })
        })
        .collect();

    items.sort_by(|a, b| {
        a.is_dir
            .cmp(&b.is_dir)
            .reverse()
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries.extend(items);
    Ok(entries)
}

async fn read_remote_entries(sftp: &SftpSession, path: &str) -> Result<Vec<Entry>> {
    let mut entries = vec![Entry {
        name: "..".to_string(),
        path: remote_parent(path),
        is_dir: true,
        size: None,
        modified: None,
    }];

    let mut items = Vec::new();
    for entry in sftp
        .read_dir(path.to_string())
        .await
        .with_context(|| format!("failed to list remote path {path}"))?
    {
        let name = entry.file_name();
        let metadata = entry.metadata();
        let is_dir = entry.file_type() == FileType::Dir;
        items.push(Entry {
            name: name.clone(),
            path: join_remote(path, &name),
            is_dir,
            size: (!is_dir).then_some(metadata.len()),
            modified: metadata.modified().ok().map(format_system_time),
        });
    }

    items.sort_by(|a, b| {
        a.is_dir
            .cmp(&b.is_dir)
            .reverse()
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries.extend(items);
    Ok(entries)
}

fn format_system_time(time: std::time::SystemTime) -> String {
    let datetime: DateTime<Local> = time.into();
    datetime.format("%Y-%m-%d %H:%M").to_string()
}

fn human_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit_index = 0;
    while value >= 1024.0 && unit_index < UNITS.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{size}{}", UNITS[unit_index])
    } else {
        format!("{value:.1}{}", UNITS[unit_index])
    }
}

fn join_remote(base: &str, child: &str) -> String {
    if base == "/" {
        format!("/{child}")
    } else if base == "." || base.is_empty() {
        child.to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), child)
    }
}

fn remote_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "." || trimmed == "/" {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(index) => trimmed[..index].to_string(),
        None => ".".to_string(),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn find_login_file() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join("ssh.txt"));
    }
    if let Ok(current) = std::env::current_dir() {
        candidates.push(current.join("ssh.txt"));
        if let Some(parent) = current.parent() {
            candidates.push(parent.join("ssh.txt"));
        }
    }
    candidates.into_iter().find(|path| path.exists())
}

fn default_key_path() -> String {
    if let Some(home) = dirs::home_dir() {
        for name in ["id_ed25519", "id_rsa", "id_ecdsa"] {
            let candidate = home.join(".ssh").join(name);
            if candidate.exists() {
                return candidate.display().to_string();
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn run(args: &[&str]) -> CliArgs {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse_cli(&args).unwrap() {
            CliCommand::Run(cli) => cli,
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn existing_directory_is_start_dir_not_alias() {
        let dir = std::env::temp_dir();
        let cli = run(&[dir.to_str().unwrap()]);
        assert_eq!(cli.start_dir, Some(dir));
        assert!(cli.alias.is_none());
    }

    #[test]
    fn alias_and_user_at_alias() {
        let cli = run(&["hpc-not-a-dir-xyz"]);
        assert_eq!(cli.alias.as_deref(), Some("hpc-not-a-dir-xyz"));
        let cli = run(&["alice@hpc-not-a-dir-xyz"]);
        assert_eq!(cli.user.as_deref(), Some("alice"));
        assert_eq!(cli.alias.as_deref(), Some("hpc-not-a-dir-xyz"));
    }

    #[test]
    fn flags() {
        let args = vec!["--list-hosts".to_string()];
        assert!(matches!(parse_cli(&args).unwrap(), CliCommand::ListHosts));
        let args = vec!["--help".to_string()];
        assert!(matches!(parse_cli(&args).unwrap(), CliCommand::Help));
        let args = vec!["--bogus".to_string()];
        assert!(parse_cli(&args).is_err());
    }
}
