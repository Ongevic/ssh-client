//! Minimal OpenSSH client config (`~/.ssh/config`) reader.
//!
//! Supports what this client needs to behave like `ssh <alias>`:
//! `Host` blocks with several patterns (`*`, `?`, `!negation`), global lines
//! before the first `Host`, `Include` (globs, relative to `~/.ssh`),
//! case-insensitive keywords, `keyword value` and `keyword=value` forms,
//! double-quoted arguments, comments, and first-value-wins resolution
//! (`IdentityFile` accumulates, like OpenSSH). `Match` blocks are skipped.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

const MAX_INCLUDE_DEPTH: usize = 16;

/// Where `~` and relative `Include` paths point. Tests pass a temp dir so
/// they never touch the real `~/.ssh`.
#[derive(Clone, Debug)]
pub struct ParseContext {
    pub home: PathBuf,
    pub ssh_dir: PathBuf,
}

impl ParseContext {
    pub fn from_home(home: PathBuf) -> Self {
        let ssh_dir = home.join(".ssh");
        Self { home, ssh_dir }
    }
}

#[derive(Clone, Debug)]
enum Condition {
    /// Lines before any Host/Match line: apply to every host.
    Always,
    Host(Vec<String>),
    /// `Match` blocks are not evaluated; their options never apply.
    Match,
}

#[derive(Clone, Debug)]
struct OptionLine {
    block: usize,
    key: String,
    args: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct SshConfig {
    blocks: Vec<Condition>,
    lines: Vec<OptionLine>,
    pub warnings: Vec<String>,
    home: PathBuf,
}

/// Effective settings for one alias, as `ssh <alias>` would use them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedHost {
    pub alias: String,
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub identity_files: Vec<PathBuf>,
    /// True when IdentityFile came from the config, false for OpenSSH defaults.
    pub identity_files_explicit: bool,
    pub identities_only: bool,
    pub proxy_jump: Option<String>,
    pub proxy_command: Option<String>,
    pub server_alive_interval: Option<u64>,
    pub server_alive_count_max: Option<usize>,
    pub connect_timeout: Option<u64>,
}

impl ResolvedHost {
    pub fn summary(&self) -> String {
        format!("{}@{}:{}", self.user, self.hostname, self.port)
    }
}

/// A parsed `ProxyJump` hop: `[user@]host[:port]` or `ssh://[user@]host[:port]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpSpec {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

/// Parses a ProxyJump value. Returns Ok(None) for `none`, Err for
/// multi-hop chains or malformed values.
pub fn parse_proxy_jump(value: &str) -> Result<Option<JumpSpec>, String> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    if value.contains(',') {
        return Err(format!(
            "ProxyJump with several hops (`{value}`) is not supported yet; only a single jump host works"
        ));
    }
    let rest = value.strip_prefix("ssh://").unwrap_or(value);
    let (user, hostport) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u.to_string()), h),
        None => (None, rest),
    };
    let (host, port) = if let Some(stripped) = hostport.strip_prefix('[') {
        // [ipv6]:port
        let (h, after) = stripped
            .split_once(']')
            .ok_or_else(|| format!("malformed ProxyJump `{value}`"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p.parse::<u16>().map_err(|_| format!("bad port in ProxyJump `{value}`"))?),
            None => None,
        };
        (h.to_string(), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                Some(p.parse::<u16>().map_err(|_| format!("bad port in ProxyJump `{value}`"))?),
            ),
            None => (hostport.to_string(), None),
        }
    };
    if host.is_empty() {
        return Err(format!("malformed ProxyJump `{value}`"));
    }
    Ok(Some(JumpSpec { user, host, port }))
}

impl SshConfig {
    /// Loads `%USERPROFILE%\.ssh\config` (or `~/.ssh/config`). A missing file
    /// yields an empty config.
    pub fn load_default() -> Self {
        let Some(home) = dirs::home_dir() else {
            return Self::default();
        };
        let ctx = ParseContext::from_home(home);
        let path = ctx.ssh_dir.join("config");
        Self::load(&path, &ctx)
    }

    pub fn load(path: &Path, ctx: &ParseContext) -> Self {
        let mut cfg = SshConfig {
            blocks: vec![Condition::Always],
            home: ctx.home.clone(),
            ..Default::default()
        };
        match fs::read_to_string(path) {
            Ok(text) => {
                let mut current = 0;
                cfg.parse_text(&text, ctx, &mut current, 0);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => cfg
                .warnings
                .push(format!("cannot read {}: {err}", path.display())),
        }
        cfg
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn parse_str(text: &str, ctx: &ParseContext) -> Self {
        let mut cfg = SshConfig {
            blocks: vec![Condition::Always],
            home: ctx.home.clone(),
            ..Default::default()
        };
        let mut current = 0;
        cfg.parse_text(text, ctx, &mut current, 0);
        cfg
    }

    fn parse_text(&mut self, text: &str, ctx: &ParseContext, current: &mut usize, depth: usize) {
        for raw in text.lines() {
            let Some((key, args)) = split_line(raw) else {
                continue;
            };
            match key.as_str() {
                "host" => {
                    self.blocks.push(Condition::Host(args));
                    *current = self.blocks.len() - 1;
                }
                "match" => {
                    self.blocks.push(Condition::Match);
                    *current = self.blocks.len() - 1;
                }
                "include" => {
                    if depth >= MAX_INCLUDE_DEPTH {
                        self.warnings
                            .push("Include nested too deeply; ignored".to_string());
                        continue;
                    }
                    for pattern in args {
                        let expanded = expand_tilde(&pattern, &ctx.home);
                        let full = if Path::new(&expanded).is_absolute()
                            || Path::new(&expanded).has_root()
                        {
                            PathBuf::from(expanded)
                        } else {
                            ctx.ssh_dir.join(expanded)
                        };
                        for file in glob_paths(&full) {
                            match fs::read_to_string(&file) {
                                Ok(text) => {
                                    // The included file starts in the including
                                    // block; its own Host lines switch blocks, and
                                    // the outer block is restored afterwards.
                                    let mut inner = *current;
                                    self.parse_text(&text, ctx, &mut inner, depth + 1);
                                }
                                Err(err) => self
                                    .warnings
                                    .push(format!("cannot read include {}: {err}", file.display())),
                            }
                        }
                    }
                }
                _ => self.lines.push(OptionLine {
                    block: *current,
                    key,
                    args,
                }),
            }
        }
    }

    fn block_matches(&self, block: usize, host: &str) -> bool {
        match &self.blocks[block] {
            Condition::Always => true,
            Condition::Match => false,
            Condition::Host(patterns) => host_matches(patterns, host),
        }
    }

    /// Concrete aliases from Host lines (no wildcards, no negation),
    /// `hpc` first if present, then alphabetical (case-insensitive).
    pub fn concrete_aliases(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for block in &self.blocks {
            if let Condition::Host(patterns) = block {
                for p in patterns {
                    if is_concrete(p) && seen.insert(p.to_lowercase()) {
                        out.push(p.clone());
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            let a_hpc = a.eq_ignore_ascii_case("hpc");
            let b_hpc = b.eq_ignore_ascii_case("hpc");
            b_hpc
                .cmp(&a_hpc)
                .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
        });
        out
    }

    /// Resolves an alias exactly the way OpenSSH applies its config:
    /// every matching block in file order, first obtained value wins,
    /// IdentityFile values accumulate.
    pub fn resolve(&self, alias: &str, local_user: &str) -> ResolvedHost {
        let mut hostname: Option<String> = None;
        let mut user: Option<String> = None;
        let mut port: Option<u16> = None;
        let mut identity_raw: Vec<String> = Vec::new();
        let mut identities_only: Option<bool> = None;
        let mut proxy_jump: Option<String> = None;
        let mut proxy_command: Option<String> = None;
        let mut alive: Option<u64> = None;
        let mut alive_max: Option<usize> = None;
        let mut timeout: Option<u64> = None;

        for line in &self.lines {
            if !self.block_matches(line.block, alias) || line.args.is_empty() {
                continue;
            }
            let first = line.args[0].clone();
            match line.key.as_str() {
                "hostname" => {
                    hostname.get_or_insert(first);
                }
                "user" => {
                    user.get_or_insert(first);
                }
                "port" => {
                    if port.is_none() {
                        if let Ok(p) = first.parse::<u16>() {
                            port = Some(p);
                        }
                    }
                }
                "identityfile" => identity_raw.push(first),
                "identitiesonly" => {
                    identities_only.get_or_insert(first.eq_ignore_ascii_case("yes"));
                }
                "proxyjump" => {
                    proxy_jump.get_or_insert(line.args.join(" "));
                }
                "proxycommand" => {
                    proxy_command.get_or_insert(line.args.join(" "));
                }
                "serveraliveinterval" => {
                    if alive.is_none() {
                        alive = first.parse().ok();
                    }
                }
                "serveralivecountmax" => {
                    if alive_max.is_none() {
                        alive_max = first.parse().ok();
                    }
                }
                "connecttimeout" => {
                    if timeout.is_none() {
                        timeout = first.parse().ok();
                    }
                }
                _ => {}
            }
        }

        let hostname = hostname
            .map(|h| expand_tokens(&h, &TokenValues {
                home: &self.home,
                local_user,
                host: alias,
                remote_user: "",
                port: 0,
                alias,
            }))
            .unwrap_or_else(|| alias.to_string());
        let port = port.unwrap_or(22);
        let user = user.unwrap_or_else(|| local_user.to_string());
        let tokens = TokenValues {
            home: &self.home,
            local_user,
            host: &hostname,
            remote_user: &user,
            port,
            alias,
        };
        let identity_files_explicit = !identity_raw.is_empty();
        let mut identity_files: Vec<PathBuf> = Vec::new();
        if identity_files_explicit {
            for raw in &identity_raw {
                if raw.eq_ignore_ascii_case("none") {
                    continue;
                }
                let expanded = expand_tilde(&expand_tokens(raw, &tokens), &self.home);
                let path = PathBuf::from(expanded);
                let path = if path.is_absolute() || path.has_root() {
                    path
                } else {
                    self.home.join(path)
                };
                if !identity_files.contains(&path) {
                    identity_files.push(path);
                }
            }
        } else {
            for name in ["id_rsa", "id_ecdsa", "id_ed25519", "id_dsa"] {
                identity_files.push(self.home.join(".ssh").join(name));
            }
        }
        let proxy_jump = proxy_jump.filter(|v| !v.eq_ignore_ascii_case("none"));
        let proxy_command = proxy_command.filter(|v| !v.eq_ignore_ascii_case("none"));

        ResolvedHost {
            alias: alias.to_string(),
            hostname,
            user,
            port,
            identity_files,
            identity_files_explicit,
            identities_only: identities_only.unwrap_or(false),
            proxy_jump,
            proxy_command,
            server_alive_interval: alive.filter(|v| *v > 0),
            server_alive_count_max: alive_max,
            connect_timeout: timeout.filter(|v| *v > 0),
        }
    }
}

/// Local user name for `%u` and the default `User`.
pub fn local_username() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".to_string())
}

struct TokenValues<'a> {
    home: &'a Path,
    local_user: &'a str,
    host: &'a str,
    remote_user: &'a str,
    port: u16,
    alias: &'a str,
}

fn expand_tokens(value: &str, t: &TokenValues) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('d') => out.push_str(&t.home.display().to_string()),
            Some('u') => out.push_str(t.local_user),
            Some('h') => out.push_str(t.host),
            Some('r') => out.push_str(t.remote_user),
            Some('n') => out.push_str(t.alias),
            Some('p') => out.push_str(&t.port.to_string()),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

fn expand_tilde(value: &str, home: &Path) -> String {
    if value == "~" {
        return home.display().to_string();
    }
    if let Some(rest) = value.strip_prefix("~/").or_else(|| value.strip_prefix("~\\")) {
        return home.join(rest).display().to_string();
    }
    value.to_string()
}

/// Splits a config line into a lowercased keyword and its arguments.
/// Returns None for blank lines and comments.
fn split_line(raw: &str) -> Option<(String, Vec<String>)> {
    let line = raw.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let key_end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let key = line[..key_end].to_lowercase();
    let mut rest = line[key_end..].trim_start();
    if let Some(stripped) = rest.strip_prefix('=') {
        rest = stripped.trim_start();
    }
    Some((key, tokenize(rest)))
}

fn tokenize(rest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&c) = chars.peek() else {
            break;
        };
        if c == '#' {
            break; // trailing comment
        }
        let mut token = String::new();
        if c == '"' {
            chars.next();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                token.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                token.push(c);
                chars.next();
            }
        }
        out.push(token);
    }
    out
}

fn is_concrete(pattern: &str) -> bool {
    !pattern.is_empty()
        && !pattern.starts_with('!')
        && !pattern.contains('*')
        && !pattern.contains('?')
}

/// OpenSSH semantics: a negated match rejects; otherwise any positive match accepts.
fn host_matches(patterns: &[String], host: &str) -> bool {
    let host = host.to_lowercase();
    let mut matched = false;
    for p in patterns {
        let p = p.to_lowercase();
        if let Some(neg) = p.strip_prefix('!') {
            if wildmatch(neg, &host) {
                return false;
            }
        } else if wildmatch(&p, &host) {
            matched = true;
        }
    }
    matched
}

pub fn wildmatch(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Expands `*`/`?` in any path component; results sorted, missing paths dropped.
fn glob_paths(pattern: &Path) -> Vec<PathBuf> {
    let mut current: Vec<PathBuf> = vec![PathBuf::new()];
    for comp in pattern.components() {
        let comp_str = comp.as_os_str().to_string_lossy().to_string();
        let has_wild = comp_str.contains('*') || comp_str.contains('?');
        let mut next = Vec::new();
        for base in &current {
            if has_wild {
                let Ok(rd) = fs::read_dir(if base.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    base.as_path()
                }) else {
                    continue;
                };
                let mut names: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| wildmatch(&comp_str.to_lowercase(), &n.to_lowercase()))
                    .collect();
                names.sort();
                for n in names {
                    next.push(base.join(n));
                }
            } else {
                next.push(base.join(comp.as_os_str()));
            }
        }
        current = next;
    }
    current.into_iter().filter(|p| p.is_file()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ParseContext {
        ParseContext::from_home(PathBuf::from("/home/tester"))
    }

    fn temp_ctx(name: &str) -> ParseContext {
        let base = std::env::temp_dir().join(format!(
            "sshgrab-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join(".ssh")).unwrap();
        ParseContext::from_home(base)
    }

    #[test]
    fn multiple_patterns_and_basic_fields() {
        let cfg = SshConfig::parse_str(
            "Host hpc hpc-login\n  HostName login.cluster.example\n  User alice\n  Port 2222\n",
            &ctx(),
        );
        for alias in ["hpc", "hpc-login"] {
            let r = cfg.resolve(alias, "bob");
            assert_eq!(r.hostname, "login.cluster.example");
            assert_eq!(r.user, "alice");
            assert_eq!(r.port, 2222);
        }
        let other = cfg.resolve("elsewhere", "bob");
        assert_eq!(other.hostname, "elsewhere");
        assert_eq!(other.user, "bob");
        assert_eq!(other.port, 22);
    }

    #[test]
    fn host_star_defaults_and_first_value_wins() {
        let text = "\
Host hpc
    User alice
    ServerAliveInterval 30

Host *
    User nobody
    Port 2200
    ServerAliveInterval 99
    IdentitiesOnly yes
";
        let cfg = SshConfig::parse_str(text, &ctx());
        let r = cfg.resolve("hpc", "bob");
        assert_eq!(r.user, "alice", "specific block comes first and wins");
        assert_eq!(r.port, 2200, "Host * default applies");
        assert_eq!(r.server_alive_interval, Some(30));
        assert!(r.identities_only);

        // A later specific block cannot override an earlier Host * value.
        let cfg = SshConfig::parse_str("Host *\n User star\nHost hpc\n User late\n", &ctx());
        assert_eq!(cfg.resolve("hpc", "bob").user, "star");
    }

    #[test]
    fn global_lines_before_host_apply_everywhere() {
        let cfg = SshConfig::parse_str("Port 2022\nHost hpc\n Port 22\n", &ctx());
        assert_eq!(cfg.resolve("hpc", "u").port, 2022);
    }

    #[test]
    fn case_insensitive_keywords_equals_and_quotes() {
        let text = "HOST=hpc\n\tHOSTNAME=\"login node.example\"\n  user = alice\n  pOrT=2201\n  IdentityFile \"~/.ssh/my key\"\n";
        let cfg = SshConfig::parse_str(text, &ctx());
        let r = cfg.resolve("HPC", "bob");
        assert_eq!(r.hostname, "login node.example");
        assert_eq!(r.user, "alice");
        assert_eq!(r.port, 2201);
        assert_eq!(
            r.identity_files,
            vec![PathBuf::from("/home/tester").join(".ssh/my key")]
        );
    }

    #[test]
    fn comments_are_ignored() {
        let text = "# leading comment\nHost hpc # trailing\n  # inside\n  User alice # who\n";
        let cfg = SshConfig::parse_str(text, &ctx());
        assert_eq!(cfg.concrete_aliases(), vec!["hpc".to_string()]);
        assert_eq!(cfg.resolve("hpc", "bob").user, "alice");
    }

    #[test]
    fn identity_files_accumulate_and_expand_tokens() {
        let text = "\
Host hpc
    HostName login.example
    User alice
    IdentityFile ~/.ssh/id_%h_%r
    IdentityFile %d/keys/%u.key
Host *
    IdentityFile ~/.ssh/id_ed25519
";
        let cfg = SshConfig::parse_str(text, &ctx());
        let r = cfg.resolve("hpc", "bob");
        let home = PathBuf::from("/home/tester");
        assert_eq!(
            r.identity_files,
            vec![
                home.join(".ssh/id_login.example_alice"),
                PathBuf::from(format!("{}/keys/bob.key", home.display())),
                home.join(".ssh/id_ed25519"),
            ]
        );
        assert!(r.identity_files_explicit);
    }

    #[test]
    fn default_identity_files_when_none_configured() {
        let cfg = SshConfig::parse_str("Host hpc\n User a\n", &ctx());
        let r = cfg.resolve("hpc", "bob");
        assert!(!r.identity_files_explicit);
        assert!(r.identity_files.iter().any(|p| p.ends_with("id_ed25519")));
    }

    #[test]
    fn wildcard_hosts_filtered_from_aliases_and_hpc_first() {
        let text = "Host zeta alpha *.example ?ost !bad\nHost hpc\nHost *\nMatch host foo\n User m\nHost Beta\n";
        let cfg = SshConfig::parse_str(text, &ctx());
        assert_eq!(
            cfg.concrete_aliases(),
            vec!["hpc", "alpha", "Beta", "zeta"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn match_blocks_are_ignored_and_negation_works() {
        let text = "Match all\n User matched\nHost * !hpc\n Port 2000\nHost hpc\n User alice\n";
        let cfg = SshConfig::parse_str(text, &ctx());
        let r = cfg.resolve("hpc", "bob");
        assert_eq!(r.user, "alice");
        assert_eq!(r.port, 22, "negated pattern excludes hpc");
        assert_eq!(cfg.resolve("other", "bob").port, 2000);
    }

    #[test]
    fn include_with_glob_relative_to_ssh_dir() {
        let c = temp_ctx("include");
        let conf_d = c.ssh_dir.join("conf.d");
        fs::create_dir_all(&conf_d).unwrap();
        fs::write(conf_d.join("10-hpc.conf"), "Host hpc\n HostName hpc.example\n User alice\n").unwrap();
        fs::write(conf_d.join("20-lab.conf"), "Host lab\n HostName lab.example\n").unwrap();
        fs::write(conf_d.join("ignored.txt"), "Host nope\n").unwrap();
        let main = "Include conf.d/*.conf\nHost *\n User fallback\n";
        let cfg = SshConfig::parse_str(main, &c);
        assert_eq!(cfg.concrete_aliases(), vec!["hpc".to_string(), "lab".to_string()]);
        assert_eq!(cfg.resolve("hpc", "bob").hostname, "hpc.example");
        assert_eq!(cfg.resolve("lab", "bob").user, "fallback");
        let _ = fs::remove_dir_all(&c.home);
    }

    #[test]
    fn include_inside_block_restores_outer_block() {
        let c = temp_ctx("include-block");
        fs::write(c.ssh_dir.join("extra"), "Port 2022\n").unwrap();
        let main = "Host hpc\n Include extra\n User alice\nHost other\n";
        let cfg = SshConfig::parse_str(main, &c);
        let r = cfg.resolve("hpc", "bob");
        assert_eq!(r.port, 2022);
        assert_eq!(r.user, "alice");
        assert_eq!(cfg.resolve("other", "bob").port, 22);
        let _ = fs::remove_dir_all(&c.home);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let c = temp_ctx("missing");
        let cfg = SshConfig::load(&c.ssh_dir.join("config"), &c);
        assert!(cfg.concrete_aliases().is_empty());
        assert!(cfg.warnings.is_empty());
        let _ = fs::remove_dir_all(&c.home);
    }

    #[test]
    fn proxy_jump_parsing() {
        assert_eq!(parse_proxy_jump("none").unwrap(), None);
        assert_eq!(
            parse_proxy_jump("bob@gw.example:2022").unwrap(),
            Some(JumpSpec {
                user: Some("bob".into()),
                host: "gw.example".into(),
                port: Some(2022)
            })
        );
        assert_eq!(
            parse_proxy_jump("gw").unwrap(),
            Some(JumpSpec { user: None, host: "gw".into(), port: None })
        );
        assert!(parse_proxy_jump("a,b").is_err());
        let cfg = SshConfig::parse_str("Host hpc\n ProxyJump gw\nHost * \n ProxyJump none\n", &ctx());
        assert_eq!(cfg.resolve("hpc", "u").proxy_jump.as_deref(), Some("gw"));
        assert_eq!(cfg.resolve("x", "u").proxy_jump, None);
    }

    #[test]
    fn wildmatch_basics() {
        assert!(wildmatch("*.example", "a.example"));
        assert!(wildmatch("h?c", "hpc"));
        assert!(!wildmatch("h?c", "hpcc"));
        assert!(wildmatch("*", ""));
        assert!(wildmatch("a*b*c", "axxbyyc"));
    }
}
