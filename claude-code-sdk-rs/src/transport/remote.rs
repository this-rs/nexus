//! Running the Claude Code CLI on another machine, over SSH.
//!
//! The local command line is built exactly as always ([`super::subprocess`]); this
//! module then turns it into `ssh <host> '<that command>'`. The stream-json
//! protocol is unchanged, it simply travels over the SSH channel's stdin/stdout.
//!
//! A remote `claude` runs tools (files, shell) on a machine this process does not
//! control, so the rules are strict and every one of them is a test:
//!
//! * the host key is **pinned**: the caller gives the public key it registered, it
//!   is the only entry of a private `known_hosts` file, and `StrictHostKeyChecking`
//!   is `yes`. Nothing is ever learned or accepted on first use;
//! * no interaction, no agent, no forwarding, no ssh configuration file, no
//!   connection sharing: `BatchMode`, `-F /dev/null`, `IdentitiesOnly`,
//!   `ClearAllForwardings`, `ControlPath=none`;
//! * nothing secret on the remote command line: only a short allowlist of
//!   variables is forwarded, and an MCP configuration is refused (it carries
//!   credentials, and a file path on this machine means nothing over there);
//! * the working directory is the **remote** one, never a local path.
//!
//! The remote environment is the remote user's login environment: the local
//! [`EnvPolicy`](super::spawn::EnvPolicy) only shapes the `ssh` client process.

use super::spawn::SecretFile;
use crate::errors::{Result, SdkError};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Variables that may cross to the remote command. Anything else set on the local
/// command is dropped: a key or token must not end up on a remote `argv`.
pub const FORWARDED_ENV: &[&str] = &[
    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
    "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING",
];

/// Where and how to reach the machine that runs `claude`.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteHost {
    /// Host name or address. Validated: no leading `-`, no whitespace or control character.
    pub host: String,
    /// Remote user (the ssh default when `None`).
    pub user: Option<String>,
    /// Port (22 when `None`).
    pub port: Option<u16>,
    /// The pinned public key of the host, `"<type> <base64>"` (what `ssh-keyscan`
    /// prints after the host name). The caller registered it and showed its
    /// fingerprint to a human.
    pub host_key: String,
    /// Private key file on THIS machine, owner-only. `None` lets ssh find no
    /// identity at all, which fails: a remote machine always needs one.
    pub identity_file: Option<PathBuf>,
    /// The program to run on the remote machine (looked up in its `PATH`).
    pub cli: String,
    /// Working directory on the remote machine.
    pub cwd: Option<String>,
    /// Seconds before giving up on connecting.
    pub connect_timeout_secs: u32,
    /// The local `ssh` client (`ssh` from `PATH` when `None`).
    pub ssh_program: Option<PathBuf>,
    /// Allow the no-confirmation permission mode on this machine. Refused by
    /// default: the tools run where nobody is watching, so an exception is explicit
    /// and per machine.
    pub allow_trust: bool,
}

impl std::fmt::Debug for RemoteHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key is public and the identity is a PATH, but there is no reason to
        // print either in a log.
        f.debug_struct("RemoteHost")
            .field("host", &self.host)
            .field("user", &self.user)
            .field("port", &self.port)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

impl RemoteHost {
    /// A remote host with the safe defaults (`claude`, 10 s connection timeout).
    pub fn new(host: impl Into<String>, host_key: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            user: None,
            port: None,
            host_key: host_key.into(),
            identity_file: None,
            cli: "claude".to_string(),
            cwd: None,
            connect_timeout_secs: 10,
            ssh_program: None,
            allow_trust: false,
        }
    }

    /// Who and where, for a session's resume token: a CLI session lives in the home
    /// of ONE user on ONE machine, so its identifier means nothing elsewhere.
    pub fn machine(&self) -> String {
        format!(
            "{}{}:{}",
            self.user
                .as_deref()
                .map(|u| format!("{u}@"))
                .unwrap_or_default(),
            self.host,
            self.port.unwrap_or(22)
        )
    }

    /// Refuses a configuration that could turn a field into an `ssh` option or a
    /// shell fragment.
    pub fn validate(&self) -> Result<()> {
        word("host", &self.host)?;
        if let Some(user) = &self.user {
            word("user", user)?;
        }
        if self.port == Some(0) {
            return Err(SdkError::ConfigError("remote port 0 is invalid".into()));
        }
        if self.cli.is_empty() || self.cli.contains(['\0', '\n']) {
            return Err(SdkError::ConfigError(
                "remote cli is empty or malformed".into(),
            ));
        }
        if self
            .cwd
            .as_deref()
            .is_some_and(|c| c.contains(['\0', '\n']))
        {
            return Err(SdkError::ConfigError("remote cwd is malformed".into()));
        }
        let mut parts = self.host_key.split_whitespace();
        let (kind, blob, rest) = (parts.next(), parts.next(), parts.next());
        let ok = matches!(kind, Some(k) if k.starts_with("ssh-") || k.starts_with("ecdsa-") || k.starts_with("sk-"))
            && blob.is_some_and(|b| {
                b.len() >= 16
                    && b.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"+/=".contains(&c))
            })
            && rest.is_none();
        if !ok {
            return Err(SdkError::ConfigError(
                "remote host_key must be exactly '<type> <base64>': the key is pinned, never learned".into(),
            ));
        }
        if self.connect_timeout_secs == 0 || self.connect_timeout_secs > 300 {
            return Err(SdkError::ConfigError(
                "remote connect_timeout_secs must be between 1 and 300".into(),
            ));
        }
        Ok(())
    }
}

/// A host or user name: letters, digits and `. _ - : @ [ ] %` (IPv6, zones),
/// not starting with `-` (it would be read as an option by ssh).
fn word(what: &str, value: &str) -> Result<()> {
    let ok = !value.is_empty()
        && value.len() <= 253
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-:@[]%".contains(&c));
    if ok {
        Ok(())
    } else {
        Err(SdkError::ConfigError(format!(
            "remote {what} {value:?} is not a plain name"
        )))
    }
}

/// Quotes one word for a POSIX shell: inside single quotes nothing is special.
pub fn shell_quote(word: &str) -> String {
    let mut out = String::with_capacity(word.len() + 2);
    out.push('\'');
    for c in word.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// What must outlive the child: the private `known_hosts` file holding the pinned key.
#[derive(Debug)]
pub struct RemoteLaunch {
    known_hosts: SecretFile,
}

impl RemoteLaunch {
    /// Validates `remote` and writes the pinned key to an owner-only file.
    ///
    /// Also refuses what cannot work remotely: an MCP configuration (credentials,
    /// and local paths) and an identity file readable by other users.
    pub fn prepare(remote: &RemoteHost, has_mcp_servers: bool) -> Result<Self> {
        remote.validate()?;
        if has_mcp_servers {
            return Err(SdkError::ConfigError(
                "a remote Claude Code cannot carry MCP servers: their configuration holds credentials \
                 that must not reach a remote command line"
                    .into(),
            ));
        }
        if let Some(identity) = &remote.identity_file {
            owner_only(identity)?;
        }
        let host = known_hosts_host(&remote.host, remote.port);
        let line = format!("{host} {}\n", remote.host_key.trim());
        let known_hosts =
            SecretFile::create("known_hosts", line.as_bytes()).map_err(SdkError::ProcessError)?;
        Ok(Self { known_hosts })
    }

    pub(crate) fn known_hosts_path(&self) -> &Path {
        self.known_hosts.path()
    }
}

#[cfg(unix)]
fn owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(SdkError::ProcessError)?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(SdkError::ConfigError(format!(
            "identity file {} must be readable by its owner only",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn owner_only(path: &Path) -> Result<()> {
    std::fs::metadata(path)
        .map(|_| ())
        .map_err(SdkError::ProcessError)
}

/// How a host appears in `known_hosts`: `host`, or `[host]:port` off port 22.
fn known_hosts_host(host: &str, port: Option<u16>) -> String {
    match port {
        Some(p) if p != 22 => format!("[{host}]:{p}"),
        _ => host.to_string(),
    }
}

/// Where the native installer puts `claude`: neither is on the `PATH` of a
/// non-interactive ssh session, which is what runs our command. Added after the
/// remote user's own `PATH` is read, expanded by the REMOTE shell, and only for a
/// bare program name (an absolute path needs no search).
const REMOTE_PATH: &str = r#"PATH="$HOME/.local/bin:$HOME/.claude/local:$PATH""#;

/// The remote shell command: `cd <cwd> && exec env K=v <cli> <args…>`, every word quoted.
fn remote_command(inner: &std::process::Command, remote: &RemoteHost) -> String {
    let mut out = String::new();
    if let Some(cwd) = &remote.cwd {
        out.push_str("cd ");
        out.push_str(&shell_quote(cwd));
        out.push_str(" && ");
    }
    out.push_str("exec ");
    let forwarded: Vec<String> = inner
        .get_envs()
        .filter_map(|(k, v)| {
            let key = k.to_str()?;
            let value = v?.to_str()?;
            FORWARDED_ENV
                .contains(&key)
                .then(|| shell_quote(&format!("{key}={value}")))
        })
        .collect();
    let searches = !remote.cli.contains('/');
    if !forwarded.is_empty() || searches {
        out.push_str("env ");
        for word in &forwarded {
            out.push_str(word);
            out.push(' ');
        }
        if searches {
            out.push_str(REMOTE_PATH);
            out.push(' ');
        }
    }
    out.push_str(&shell_quote(&remote.cli));
    for arg in inner.get_args() {
        out.push(' ');
        out.push_str(&shell_quote(&arg.to_string_lossy()));
    }
    out
}

/// Turns the local `claude` command into the `ssh` command that runs it remotely.
///
/// Stdio, process group and the SDK markers are applied by the caller afterwards,
/// as for a local child.
pub(crate) fn wrap_ssh(
    inner: &std::process::Command,
    remote: &RemoteHost,
    launch: &RemoteLaunch,
) -> Command {
    let program = remote
        .ssh_program
        .clone()
        .unwrap_or_else(|| PathBuf::from("ssh"));
    let mut cmd = Command::new(program);
    // The ssh client itself starts from an empty environment but for what it needs
    // to run; no SSH_AUTH_SOCK, so a forwarded or local agent is never consulted.
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.arg("-T")
        .arg("-F")
        .arg("/dev/null")
        .args(["-o", "BatchMode=yes"])
        .args(["-o", "StrictHostKeyChecking=yes"])
        .arg("-o")
        .arg(format!(
            "UserKnownHostsFile={}",
            launch.known_hosts_path().display()
        ))
        .args(["-o", "GlobalKnownHostsFile=/dev/null"])
        .args(["-o", "IdentitiesOnly=yes"])
        .args(["-o", "PasswordAuthentication=no"])
        .args(["-o", "KbdInteractiveAuthentication=no"])
        .args(["-o", "ForwardAgent=no"])
        .args(["-o", "ForwardX11=no"])
        .args(["-o", "ClearAllForwardings=yes"])
        .args(["-o", "ControlMaster=no"])
        .args(["-o", "ControlPath=none"])
        .args(["-o", "ServerAliveInterval=15"])
        .args(["-o", "ServerAliveCountMax=3"])
        .arg("-o")
        .arg(format!("ConnectTimeout={}", remote.connect_timeout_secs));
    if let Some(identity) = &remote.identity_file {
        cmd.arg("-i").arg(identity);
    }
    if let Some(port) = remote.port {
        cmd.arg("-p").arg(port.to_string());
    }
    if let Some(user) = &remote.user {
        cmd.arg("-l").arg(user);
    }
    // `--` ends the options: the host can never be taken for one.
    cmd.arg("--")
        .arg(&remote.host)
        .arg(remote_command(inner, remote));
    cmd
}

/// What asking the remote machine for its CLI version found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteProbe {
    /// The CLI answered; `None` when its answer was not a version.
    Version(Option<super::subprocess::SemVer>),
    /// The program is not on the remote `PATH`.
    CliMissing,
    /// The machine could not be used: a readable reason, never the raw `ssh` output.
    Unreachable(String),
}

/// Runs `<cli> --version` on the remote machine through the same pinned channel as
/// a session, with a deadline.
///
/// Failure texts are fixed sentences chosen from the kind of failure: the raw
/// stderr of `ssh` can carry paths and addresses and is never forwarded.
pub async fn probe_version(remote: &RemoteHost) -> RemoteProbe {
    let launch = match RemoteLaunch::prepare(remote, false) {
        Ok(l) => l,
        Err(e) => return RemoteProbe::Unreachable(e.to_string()),
    };
    let mut inner = std::process::Command::new(&remote.cli);
    inner.arg("--version");
    let mut cmd = wrap_ssh(&inner, remote, &launch);
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    let deadline = std::time::Duration::from_secs(u64::from(remote.connect_timeout_secs) + 10);
    let output = match tokio::time::timeout(deadline, cmd.output()).await {
        Err(_) => return RemoteProbe::Unreachable("the machine did not answer in time".into()),
        Ok(Err(_)) => {
            return RemoteProbe::Unreachable("the ssh client could not be started".into());
        },
        Ok(Ok(o)) => o,
    };
    match output.status.code() {
        Some(0) => RemoteProbe::Version(super::subprocess::SemVer::parse(
            String::from_utf8_lossy(&output.stdout).trim(),
        )),
        Some(127) => RemoteProbe::CliMissing,
        _ => RemoteProbe::Unreachable(classify_ssh_failure(&String::from_utf8_lossy(
            &output.stderr,
        ))),
    }
}

fn classify_ssh_failure(stderr: &str) -> String {
    let s = stderr.to_lowercase();
    let reason = if s.contains("host key verification failed")
        || s.contains("remote host identification has changed")
    {
        "the host key does not match the pinned key"
    } else if s.contains("permission denied") {
        "the machine refused the key"
    } else if s.contains("could not resolve") || s.contains("name or service not known") {
        "the host name does not resolve"
    } else if s.contains("connection refused") {
        "the machine refused the connection"
    } else if s.contains("timed out")
        || s.contains("no route to host")
        || s.contains("network is unreachable")
    {
        "the machine cannot be reached"
    } else {
        "the ssh connection failed"
    };
    reason.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

    fn host() -> RemoteHost {
        RemoteHost::new("build-1.example.net", KEY)
    }

    fn argv(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn inner(args: &[&str]) -> std::process::Command {
        let mut c = std::process::Command::new("claude");
        c.args(args);
        c
    }

    #[test]
    fn quoting_survives_every_awkward_character() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(
            shell_quote("$(rm -rf /); `x` \"y\" \n z"),
            "'$(rm -rf /); `x` \"y\" \n z'"
        );
    }

    #[test]
    fn a_name_that_looks_like_an_option_or_a_command_is_refused() {
        for bad in [
            "-oProxyCommand=evil",
            "",
            "a b",
            "a;b",
            "a\nb",
            "a$(x)",
            "a`b`",
            "é",
        ] {
            let mut h = host();
            h.host = bad.to_string();
            assert!(h.validate().is_err(), "host {bad:?} must be refused");
            let mut h = host();
            h.user = Some(bad.to_string());
            assert!(h.validate().is_err(), "user {bad:?} must be refused");
        }
        for good in [
            "build-1.example.net",
            "10.0.0.7",
            "[fe80::1]",
            "fe80::1%en0",
            "me@x",
        ] {
            let mut h = host();
            h.host = good.to_string();
            assert!(h.validate().is_ok(), "host {good:?} must be accepted");
        }
    }

    #[test]
    fn the_pinned_key_must_be_exactly_a_type_and_a_blob() {
        for bad in [
            "",
            "ssh-ed25519",
            "ssh-ed25519 short",
            "hello AAAAC3NzaC1lZDI1NTE5AAAAIOMq",
            &format!("{KEY} comment"),
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA!OMq",
        ] {
            let mut h = host();
            h.host_key = bad.to_string();
            assert!(h.validate().is_err(), "key {bad:?} must be refused");
        }
        assert!(host().validate().is_ok());
    }

    #[test]
    fn the_ssh_line_pins_the_host_and_switches_everything_else_off() {
        let launch = RemoteLaunch::prepare(&host(), false).unwrap();
        let mut h = host();
        h.port = Some(2222);
        h.user = Some("dev".into());
        let cmd = wrap_ssh(&inner(&["--model", "opus"]), &h, &launch);
        let a = argv(&cmd);
        for needed in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "GlobalKnownHostsFile=/dev/null",
            "IdentitiesOnly=yes",
            "PasswordAuthentication=no",
            "ForwardAgent=no",
            "ClearAllForwardings=yes",
            "ControlPath=none",
        ] {
            assert!(a.iter().any(|x| x == needed), "{needed} missing in {a:?}");
        }
        assert!(a.contains(&"-T".to_string()));
        assert_eq!(
            a[a.iter().position(|x| x == "-F").unwrap() + 1],
            "/dev/null"
        );
        assert!(a.windows(2).any(|w| w == ["-p", "2222"]));
        assert!(a.windows(2).any(|w| w == ["-l", "dev"]));
        // The host comes right after `--`, the remote command last.
        let dd = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(a[dd + 1], "build-1.example.net");
        assert_eq!(a.len(), dd + 3);
        assert_eq!(
            a[dd + 2],
            "exec env PATH=\"$HOME/.local/bin:$HOME/.claude/local:$PATH\" 'claude' '--model' 'opus'"
        );
    }

    #[test]
    fn the_known_hosts_file_holds_only_the_pinned_key_for_that_host() {
        let mut h = host();
        h.port = Some(2222);
        let launch = RemoteLaunch::prepare(&h, false).unwrap();
        let text = std::fs::read_to_string(launch.known_hosts_path()).unwrap();
        assert_eq!(text, format!("[build-1.example.net]:2222 {KEY}\n"));
        let plain = RemoteLaunch::prepare(&host(), false).unwrap();
        let text = std::fs::read_to_string(plain.known_hosts_path()).unwrap();
        assert_eq!(text, format!("build-1.example.net {KEY}\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(plain.known_hosts_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "the file must be owner-only");
        }
    }

    #[test]
    fn the_working_directory_is_the_remote_one_and_a_local_one_is_never_set() {
        let launch = RemoteLaunch::prepare(&host(), false).unwrap();
        let mut h = host();
        h.cwd = Some("/srv/my project/it's".into());
        let mut local = inner(&["-p"]);
        local.current_dir("/definitely/not/on/the/remote");
        let cmd = wrap_ssh(&local, &h, &launch);
        assert!(cmd.as_std().get_current_dir().is_none());
        let last = argv(&cmd).pop().unwrap();
        assert_eq!(
            last,
            r#"cd '/srv/my project/it'\''s' && exec env PATH="$HOME/.local/bin:$HOME/.claude/local:$PATH" 'claude' '-p'"#
        );
        assert!(!last.contains("definitely"));
    }

    #[test]
    fn only_the_allowlisted_variables_cross_to_the_remote() {
        let launch = RemoteLaunch::prepare(&host(), false).unwrap();
        let mut local = inner(&[]);
        local.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "8192");
        local.env("ANTHROPIC_API_KEY", "sk-should-never-leave");
        local.env("MY_TOKEN", "t0k3n");
        let cmd = wrap_ssh(&local, &host(), &launch);
        let last = argv(&cmd).pop().unwrap();
        assert_eq!(
            last,
            "exec env 'CLAUDE_CODE_MAX_OUTPUT_TOKENS=8192' PATH=\"$HOME/.local/bin:$HOME/.claude/local:$PATH\" 'claude'"
        );
        let all = argv(&cmd).join(" ");
        assert!(!all.contains("sk-should-never-leave") && !all.contains("t0k3n"));
        // And the ssh client itself does not inherit them either.
        assert!(cmd.as_std().get_envs().all(|(k, _)| k == "PATH"));
    }

    #[test]
    fn a_bare_program_name_is_searched_where_the_native_installer_puts_it_and_a_path_is_not() {
        let launch = RemoteLaunch::prepare(&host(), false).unwrap();
        let last = argv(&wrap_ssh(&inner(&[]), &host(), &launch))
            .pop()
            .unwrap();
        assert!(
            last.contains(r#"PATH="$HOME/.local/bin:$HOME/.claude/local:$PATH""#),
            "{last}"
        );
        let mut absolute = host();
        absolute.cli = "/opt/claude/bin/claude".into();
        let last = argv(&wrap_ssh(&inner(&[]), &absolute, &launch))
            .pop()
            .unwrap();
        assert_eq!(last, "exec '/opt/claude/bin/claude'");
    }

    #[test]
    fn an_mcp_configuration_is_refused_not_forwarded() {
        let err = RemoteLaunch::prepare(&host(), true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("MCP"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn an_identity_readable_by_others_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("nexus-remote-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("id");
        std::fs::write(&key, "not a real key").unwrap();
        let mut h = host();
        h.identity_file = Some(key.clone());
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(RemoteLaunch::prepare(&h, false).is_err());
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(RemoteLaunch::prepare(&h, false).is_ok());
        let cmd = wrap_ssh(&inner(&[]), &h, &RemoteLaunch::prepare(&h, false).unwrap());
        assert!(
            argv(&cmd)
                .windows(2)
                .any(|w| w[0] == "-i" && w[1] == key.to_string_lossy())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// End to end with a stand-in `ssh` that runs the remote command through `sh`,
    /// as a real sshd would: the quoting must hand the awkward arguments over intact.
    #[cfg(unix)]
    #[tokio::test]
    async fn awkward_arguments_reach_the_remote_program_intact() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("nexus-fake-ssh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("ssh");
        // Last argument is the remote command line; sshd would hand it to the login shell.
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor last; do :; done\nexec sh -c \"$last\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut h = host();
        h.ssh_program = Some(fake);
        h.cli = "printf".to_string();
        h.cwd = Some(dir.to_string_lossy().into_owned());
        let launch = RemoteLaunch::prepare(&h, false).unwrap();
        let nasty = "it's \"q\" $(echo pwned) `echo pwned` ; && | > \n new line é";
        let mut local = std::process::Command::new("ignored");
        local.arg("<%s>").arg(nasty);
        let out = wrap_ssh(&local, &h, &launch).output().await.unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("<{nasty}>"));
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod launch_guard {
    use super::*;
    use crate::transport::subprocess::{CommandMode, build_cli_command};
    use crate::types::ClaudeCodeOptions;

    #[test]
    #[should_panic(expected = "refusing to run the CLI locally")]
    fn a_remote_option_without_a_prepared_launch_never_runs_locally() {
        let options = ClaudeCodeOptions {
            remote: Some(RemoteHost::new(
                "build-1.example.net",
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl",
            )),
            ..Default::default()
        };
        let _ = build_cli_command(
            Path::new("claude"),
            &options,
            CommandMode::Stream,
            None,
            None,
        );
    }

    #[test]
    fn a_local_option_is_left_exactly_as_it_was() {
        let options = ClaudeCodeOptions::default();
        let cmd = build_cli_command(
            Path::new("/opt/claude"),
            &options,
            CommandMode::Stream,
            None,
            None,
        );
        assert_eq!(cmd.as_std().get_program(), "/opt/claude");
    }
}
