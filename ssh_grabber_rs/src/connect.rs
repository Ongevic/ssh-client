//! SSH transport, host-key checking and the `ssh <alias>`-style
//! authentication sequence: none -> agent (OpenSSH agent pipe / Pageant)
//! -> IdentityFile keys (passphrase asked when encrypted) -> password or
//! keyboard-interactive. Secrets typed by the user are used once and dropped.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use russh::{
    MethodKind, MethodSet, client,
    client::{AuthResult, KeyboardInteractiveAuthResponse, Prompt},
    keys::{
        Algorithm, PrivateKeyWithHashAlg, PublicKey, load_public_key, load_secret_key,
        agent::client::{AgentClient, AgentStream},
        known_hosts::{check_known_hosts, known_host_keys, learn_known_hosts},
    },
};
use russh_sftp::client::SftpSession;

pub type SshHandle = client::Handle<ClientHandler>;
type DynAgent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

const DEFAULT_CONNECT_TIMEOUT: u64 = 20;
const MAX_AGENT_KEYS: usize = 6;

/// Trust-on-first-use against ~/.ssh/known_hosts; a changed key is rejected.
pub struct ClientHandler {
    pub host: String,
    pub port: u16,
    pub verification_note: Arc<Mutex<Option<String>>>,
}

impl ClientHandler {
    fn note(&self, text: String) {
        if let Ok(mut slot) = self.verification_note.lock() {
            *slot = Some(text);
        }
    }
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        let outcome = match check_known_hosts(&self.host, self.port, server_public_key) {
            Ok(true) => {
                self.note("Host key verified via known_hosts.".to_string());
                true
            }
            Ok(false) => match known_host_keys(&self.host, self.port) {
                Ok(keys) if keys.is_empty() => {
                    if learn_known_hosts(&self.host, self.port, server_public_key).is_ok() {
                        self.note("Learned new host key into known_hosts.".to_string());
                        true
                    } else {
                        self.note("Could not write known_hosts; refusing unknown key.".to_string());
                        false
                    }
                }
                _ => {
                    self.note(format!(
                        "HOST KEY MISMATCH for {}:{}: known_hosts has a different key. Refusing to connect.",
                        self.host, self.port
                    ));
                    false
                }
            },
            Err(err) => {
                self.note(format!(
                    "HOST KEY CHECK FAILED for {}:{} ({err}). Refusing to connect.",
                    self.host, self.port
                ));
                false
            }
        };
        Ok(outcome)
    }
}

pub fn client_config(alive_interval: Option<u64>, alive_max: Option<usize>) -> Arc<client::Config> {
    let mut config = client::Config::default();
    if let Some(secs) = alive_interval {
        config.keepalive_interval = Some(Duration::from_secs(secs));
        config.keepalive_max = alive_max.unwrap_or(3);
    }
    Arc::new(config)
}

fn take_note(note: &Arc<Mutex<Option<String>>>) -> Option<String> {
    note.lock().ok().and_then(|slot| slot.clone())
}

/// Opens a TCP connection and runs key exchange + host-key verification.
pub async fn open_direct(
    host: &str,
    port: u16,
    config: Arc<client::Config>,
    timeout_secs: Option<u64>,
    note: Arc<Mutex<Option<String>>>,
) -> Result<SshHandle> {
    let handler = ClientHandler {
        host: host.to_string(),
        port,
        verification_note: note.clone(),
    };
    let secs = timeout_secs.unwrap_or(DEFAULT_CONNECT_TIMEOUT);
    let fut = client::connect(config, (host, port), handler);
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Err(_) => bail!("connection to {host}:{port} timed out after {secs}s"),
        Ok(Err(err)) => match take_note(&note) {
            Some(n) if n.contains("Refusing") => bail!("{n}"),
            _ => Err(anyhow::Error::new(err))
                .with_context(|| format!("failed to connect to {host}:{port}")),
        },
        Ok(Ok(handle)) => Ok(handle),
    }
}

/// Opens a `direct-tcpip` channel through an authenticated jump host and
/// runs a full SSH handshake to the target over it (ProxyJump, one hop).
pub async fn open_via_jump(
    jump: &SshHandle,
    host: &str,
    port: u16,
    config: Arc<client::Config>,
    note: Arc<Mutex<Option<String>>>,
) -> Result<SshHandle> {
    let channel = jump
        .channel_open_direct_tcpip(host, port as u32, "127.0.0.1", 0)
        .await
        .with_context(|| format!("jump host refused forwarding to {host}:{port}"))?;
    let handler = ClientHandler {
        host: host.to_string(),
        port,
        verification_note: note.clone(),
    };
    match client::connect_stream(config, channel.into_stream(), handler).await {
        Ok(handle) => Ok(handle),
        Err(err) => match take_note(&note) {
            Some(n) if n.contains("Refusing") => bail!("{n}"),
            _ => Err(anyhow::Error::new(err))
                .with_context(|| format!("SSH handshake with {host}:{port} via jump host failed")),
        },
    }
}

pub async fn open_sftp(ssh: &SshHandle) -> Result<(SftpSession, String)> {
    let channel = ssh
        .channel_open_session()
        .await
        .context("failed to open session channel for SFTP")?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("failed to request SFTP subsystem")?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .context("failed to initialize SFTP session")?;
    let pwd = sftp
        .canonicalize(".")
        .await
        .unwrap_or_else(|_| ".".to_string());
    Ok((sftp, pwd))
}

/// What the authentication sequence needs next from the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthStep {
    Done,
    NeedPassphrase { key: PathBuf },
    NeedPassword,
    NeedKeyboard { prompt: String, echo: bool },
}

/// Drives authentication for one SSH connection.
pub struct AuthFlow {
    pub handle: SshHandle,
    pub user: String,
    /// `user@host` for prompts.
    pub label: String,
    keys: VecDeque<PathBuf>,
    identity_files: Vec<PathBuf>,
    identities_only: bool,
    none_done: bool,
    agent_done: bool,
    remaining: Option<MethodSet>,
    passphrase_attempts: u8,
    password_attempts: u8,
    kbd_rounds: u8,
    kbd_prompts: VecDeque<Prompt>,
    kbd_answers: Vec<String>,
    pub notes: Vec<String>,
}

impl AuthFlow {
    pub fn new(
        handle: SshHandle,
        user: String,
        host: &str,
        identity_files: Vec<PathBuf>,
        identities_only: bool,
    ) -> Self {
        Self {
            handle,
            label: format!("{user}@{host}"),
            user,
            keys: identity_files.iter().cloned().collect(),
            identity_files,
            identities_only,
            none_done: false,
            agent_done: false,
            remaining: None,
            passphrase_attempts: 0,
            password_attempts: 0,
            kbd_rounds: 0,
            kbd_prompts: VecDeque::new(),
            kbd_answers: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn allows(&self, kind: MethodKind) -> bool {
        self.remaining
            .as_ref()
            .is_none_or(|methods| methods.contains(&kind))
    }

    fn record(&mut self, result: AuthResult) -> bool {
        match result {
            AuthResult::Success => true,
            AuthResult::Failure {
                remaining_methods, ..
            } => {
                self.remaining = Some(remaining_methods);
                false
            }
        }
    }

    /// Runs every automatic method until success or user input is needed.
    pub async fn run(&mut self) -> Result<AuthStep> {
        if !self.none_done {
            self.none_done = true;
            let result = self
                .handle
                .authenticate_none(self.user.clone())
                .await
                .context("SSH authentication request failed")?;
            if self.record(result) {
                return Ok(AuthStep::Done);
            }
        }
        if !self.agent_done {
            self.agent_done = true;
            if self.allows(MethodKind::PublicKey) && self.try_agents().await? {
                return Ok(AuthStep::Done);
            }
        }
        while let Some(path) = self.keys.front().cloned() {
            if !self.allows(MethodKind::PublicKey) {
                self.keys.clear();
                break;
            }
            if !path.is_file() {
                self.keys.pop_front();
                continue;
            }
            match load_secret_key(&path, None) {
                Ok(key) => {
                    self.keys.pop_front();
                    if self.try_key(key, &path).await? {
                        return Ok(AuthStep::Done);
                    }
                }
                Err(russh::keys::Error::KeyIsEncrypted) => {
                    self.passphrase_attempts = 0;
                    return Ok(AuthStep::NeedPassphrase { key: path });
                }
                Err(err) => {
                    self.keys.pop_front();
                    self.notes
                        .push(format!("skipped key {}: {err}", file_label(&path)));
                }
            }
        }
        self.interactive_stage().await
    }

    async fn interactive_stage(&mut self) -> Result<AuthStep> {
        if self.allows(MethodKind::Password) && self.password_attempts < 3 {
            return Ok(AuthStep::NeedPassword);
        }
        if self.allows(MethodKind::KeyboardInteractive) && self.kbd_rounds < 3 {
            self.kbd_rounds += 1;
            let response = self
                .handle
                .authenticate_keyboard_interactive_start(self.user.clone(), None)
                .await
                .context("keyboard-interactive authentication failed")?;
            return self.handle_kbd(response).await;
        }
        let offered = self
            .remaining
            .as_ref()
            .map(|m| {
                m.iter()
                    .map(|k| <&str>::from(k).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let mut msg = format!("authentication failed for {}", self.label);
        if !offered.is_empty() {
            msg.push_str(&format!(" (server offers: {offered})"));
        }
        if !self.notes.is_empty() {
            msg.push_str(&format!("; {}", self.notes.join("; ")));
        }
        bail!(msg)
    }

    async fn handle_kbd(&mut self, mut response: KeyboardInteractiveAuthResponse) -> Result<AuthStep> {
        loop {
            match response {
                KeyboardInteractiveAuthResponse::Success => return Ok(AuthStep::Done),
                KeyboardInteractiveAuthResponse::Failure {
                    remaining_methods, ..
                } => {
                    self.remaining = Some(remaining_methods);
                    self.notes.push("keyboard-interactive rejected".to_string());
                    return Box::pin(self.interactive_stage()).await;
                }
                KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                    if prompts.is_empty() {
                        response = self
                            .handle
                            .authenticate_keyboard_interactive_respond(Vec::new())
                            .await
                            .context("keyboard-interactive authentication failed")?;
                        continue;
                    }
                    self.kbd_answers.clear();
                    self.kbd_prompts = prompts.into_iter().collect();
                    return Ok(self.next_kbd_prompt());
                }
            }
        }
    }

    fn next_kbd_prompt(&self) -> AuthStep {
        let p = &self.kbd_prompts[0];
        AuthStep::NeedKeyboard {
            prompt: p.prompt.trim().to_string(),
            echo: p.echo,
        }
    }

    /// `None` skips this key (the user pressed Esc).
    pub async fn with_passphrase(&mut self, passphrase: Option<String>) -> Result<AuthStep> {
        let Some(path) = self.keys.front().cloned() else {
            return self.run().await;
        };
        let Some(passphrase) = passphrase else {
            self.keys.pop_front();
            self.notes.push(format!("skipped key {}", file_label(&path)));
            return self.run().await;
        };
        match load_secret_key(&path, Some(passphrase.as_str())) {
            Ok(key) => {
                drop(passphrase);
                self.keys.pop_front();
                if self.try_key(key, &path).await? {
                    return Ok(AuthStep::Done);
                }
                self.run().await
            }
            Err(_) => {
                self.passphrase_attempts += 1;
                if self.passphrase_attempts >= 3 {
                    self.keys.pop_front();
                    self.notes
                        .push(format!("could not decrypt key {}", file_label(&path)));
                    self.run().await
                } else {
                    Ok(AuthStep::NeedPassphrase { key: path })
                }
            }
        }
    }

    pub async fn with_password(&mut self, password: String) -> Result<AuthStep> {
        self.password_attempts += 1;
        let result = self
            .handle
            .authenticate_password(self.user.clone(), password)
            .await
            .context("password authentication request failed")?;
        if self.record(result) {
            return Ok(AuthStep::Done);
        }
        self.notes.push("password rejected".to_string());
        self.interactive_stage().await
    }

    pub async fn with_keyboard_answer(&mut self, answer: String) -> Result<AuthStep> {
        self.kbd_prompts.pop_front();
        self.kbd_answers.push(answer);
        if !self.kbd_prompts.is_empty() {
            return Ok(self.next_kbd_prompt());
        }
        let answers = std::mem::take(&mut self.kbd_answers);
        let response = self
            .handle
            .authenticate_keyboard_interactive_respond(answers)
            .await
            .context("keyboard-interactive authentication failed")?;
        self.handle_kbd(response).await
    }

    async fn rsa_hash_for(&self, algorithm: &Algorithm) -> Option<russh::keys::HashAlg> {
        if matches!(algorithm, Algorithm::Rsa { .. }) {
            self.handle
                .best_supported_rsa_hash()
                .await
                .ok()
                .flatten()
                .flatten()
        } else {
            None
        }
    }

    async fn try_key(&mut self, key: russh::keys::PrivateKey, path: &Path) -> Result<bool> {
        let hash = self.rsa_hash_for(&key.algorithm()).await;
        let result = self
            .handle
            .authenticate_publickey(
                self.user.clone(),
                PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await
            .context("public key authentication request failed")?;
        let ok = self.record(result);
        if !ok {
            self.notes
                .push(format!("key {} not accepted", file_label(path)));
        }
        Ok(ok)
    }

    /// Public keys of the configured identity files (used for IdentitiesOnly).
    fn configured_public_keys(&self) -> Vec<PublicKey> {
        let mut out = Vec::new();
        for path in &self.identity_files {
            let mut pub_path = path.clone().into_os_string();
            pub_path.push(".pub");
            if let Ok(key) = load_public_key(PathBuf::from(pub_path)) {
                out.push(key);
            } else if let Ok(secret) = load_secret_key(path, None) {
                out.push(secret.public_key().clone());
            }
        }
        out
    }

    async fn try_agents(&mut self) -> Result<bool> {
        let allowed = self
            .identities_only
            .then(|| self.configured_public_keys());
        let mut tried = 0usize;
        let mut any_agent = false;
        for (name, mut agent) in connect_agents().await {
            any_agent = true;
            let identities = match tokio::time::timeout(
                Duration::from_secs(3),
                agent.request_identities(),
            )
            .await
            {
                Ok(Ok(ids)) => ids,
                _ => continue,
            };
            for identity in identities {
                if tried >= MAX_AGENT_KEYS || !self.allows(MethodKind::PublicKey) {
                    return Ok(false);
                }
                let public = identity.public_key().into_owned();
                if let Some(allowed) = &allowed {
                    if !allowed.iter().any(|k| k.key_data() == public.key_data()) {
                        continue;
                    }
                }
                tried += 1;
                let hash = self.rsa_hash_for(&public.algorithm()).await;
                match self
                    .handle
                    .authenticate_publickey_with(self.user.clone(), public, hash, &mut agent)
                    .await
                {
                    Ok(result) => {
                        if self.record(result) {
                            self.notes.push(format!("authenticated with {name} key"));
                            return Ok(true);
                        }
                    }
                    Err(err) => self.notes.push(format!("{name}: {err}")),
                }
            }
        }
        if !any_agent {
            self.notes.push("no ssh-agent reachable".to_string());
        }
        Ok(false)
    }
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Every reachable agent, in preference order.
async fn connect_agents() -> Vec<(&'static str, DynAgent)> {
    let mut out: Vec<(&'static str, DynAgent)> = Vec::new();
    #[cfg(windows)]
    {
        const OPENSSH_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
        let mut pipes: Vec<String> = Vec::new();
        if let Ok(sock) = std::env::var("SSH_AUTH_SOCK") {
            if sock.starts_with(r"\\") {
                pipes.push(sock);
            }
        }
        if !pipes.iter().any(|p| p.eq_ignore_ascii_case(OPENSSH_PIPE)) {
            pipes.push(OPENSSH_PIPE.to_string());
        }
        for pipe in pipes {
            let attempt = tokio::time::timeout(
                Duration::from_secs(2),
                AgentClient::connect_named_pipe(pipe),
            )
            .await;
            if let Ok(Ok(agent)) = attempt {
                out.push(("ssh-agent", agent.dynamic()));
                break;
            }
        }
        if let Ok(Ok(agent)) =
            tokio::time::timeout(Duration::from_secs(2), AgentClient::connect_pageant()).await
        {
            out.push(("Pageant", agent.dynamic()));
        }
    }
    #[cfg(unix)]
    {
        if let Ok(agent) = AgentClient::connect_env().await {
            out.push(("ssh-agent", agent.dynamic()));
        }
    }
    out
}
