//! The tmux session Terminus drives, and installing tmux and asciinema when the image lacks them.

use super::*;

/// Command execution inside the environment (the agent's `AgentCtx`; mocked in tests).
#[async_trait::async_trait]
pub trait Shell: Send + Sync {
    /// `root`: run as root, else as the phase's default user.
    async fn sh(&self, cmd: &str, root: bool, timeout: Option<Duration>) -> Result<ExecResult>;
}

#[async_trait::async_trait]
impl Shell for AgentCtx<'_> {
    async fn sh(&self, cmd: &str, root: bool, timeout: Option<Duration>) -> Result<ExecResult> {
        let env = BTreeMap::new();
        if root { self.exec_as(cmd, Some("root"), &env, timeout).await } else { self.exec(cmd, &env, timeout).await }
    }
}

const ENTER_KEYS: [&str; 6] = ["Enter", "C-m", "KPEnter", "C-j", "^M", "^J"];
const COMPLETION_COMMAND: &str = "; tmux wait -S done";
/// tmux rejects client commands above ~16 KB ("command too long").
pub(super) const MAX_COMMAND_LENGTH: usize = 16_000;
const PASTE_CHUNK: usize = 65_536;
const INSTALL_TIMEOUT: Duration = Duration::from_secs(120);
const INSTALL_BUDGET: Duration = Duration::from_secs(240);

pub struct Tmux {
    pub(super) name: String,
    pub(super) logging_path: String,
    pub(super) recording_path: Option<String>,
    pub(super) width: i64,
    pub(super) height: i64,
    pub(super) extra_env: BTreeMap<String, String>,
    pub(super) previous_buffer: Option<String>,
}

fn is_executing_command(k: &str) -> bool {
    ENTER_KEYS.contains(&k) || k.ends_with('\n') || k.ends_with('\r')
}

impl Tmux {
    pub(super) fn start_command(&self) -> String {
        let env: String = self.extra_env.iter().map(|(k, v)| format!("-e {} ", quote(&format!("{k}={v}")))).collect();
        format!(
            "export TERM=xterm-256color && export SHELL=/bin/bash && tmux new-session {env}-x {} -y {} -d -s {} 'bash --login' \\; pipe-pane -t {} 'cat > {}'",
            self.width, self.height, self.name, self.name, self.logging_path
        )
    }

    fn send_keys_prefix(&self) -> String {
        format!("tmux send-keys -t {} --", quote(&self.name))
    }

    fn max_escaped_key_length(&self) -> usize {
        MAX_COMMAND_LENGTH - self.send_keys_prefix().len() - 1
    }

    pub(super) fn send_keys_command(&self, keys: &[String]) -> String {
        let quoted: Vec<String> = keys.iter().map(|k| quote(k)).collect();
        format!("{} {}", self.send_keys_prefix(), quoted.join(" "))
    }

    pub(super) fn key_requires_paste(&self, key: &str) -> bool {
        quote(key).len() > self.max_escaped_key_length()
    }

    /// Group keys into batches whose send-keys command fits the limit.
    pub(super) fn batch_keys(&self, keys: &[String]) -> Result<Vec<Vec<String>>> {
        let prefix = self.send_keys_prefix().len();
        let (mut batches, mut batch, mut len) = (vec![], vec![], prefix);
        for k in keys {
            let escaped = quote(k).len();
            if escaped > self.max_escaped_key_length() {
                bail!("key exceeds the tmux send-keys command-length limit; it must be sent via a paste buffer instead");
            }
            if !batch.is_empty() && len + 1 + escaped > MAX_COMMAND_LENGTH {
                batches.push(std::mem::take(&mut batch));
                len = prefix;
            }
            batch.push(k.clone());
            len += 1 + escaped;
        }
        if !batch.is_empty() {
            batches.push(batch);
        }
        Ok(batches)
    }

    pub async fn start(&mut self, sh: &dyn Shell) -> Result<()> {
        // Recording tools are best-effort: a hung install must not eat the setup budget.
        if let Ok(r) = tokio::time::timeout(INSTALL_BUDGET, self.install_tools(sh)).await {
            r?;
        }
        let r = sh.sh(&self.start_command(), false, None).await?;
        if r.code != 0 {
            bail!("Failed to start tmux session. Error: {}", r.stderr);
        }
        sh.sh("tmux set-option -g history-limit 10000000", false, None).await?;
        if let Some(p) = self.recording_path.clone() {
            self.send_keys(sh, vec![format!("asciinema rec --stdin {p}"), "Enter".into()], false, 1.0, 180.0).await?;
            self.send_keys(sh, vec!["clear".into(), "Enter".into()], false, 0.0, 180.0).await?;
        }
        Ok(())
    }

    async fn install_tools(&self, sh: &dyn Shell) -> Result<()> {
        let tmux_ok = sh.sh("tmux -V", true, None).await?.code == 0;
        let needs_asciinema = self.recording_path.is_some();
        let asciinema_ok = !needs_asciinema || sh.sh("asciinema --version", true, None).await?.code == 0;
        if tmux_ok && asciinema_ok {
            return Ok(());
        }
        let mut tools = vec![];
        if !tmux_ok {
            tools.push("tmux");
        }
        if !asciinema_ok {
            tools.push("asciinema");
        }
        let mut pm = None;
        for p in ["apt-get", "dnf", "yum", "apk", "pacman", "brew", "pkg", "zypper"] {
            if sh.sh(&format!("which {p} >/dev/null 2>&1"), true, None).await?.code == 0 {
                pm = Some(p);
                break;
            }
        }
        if let Some(pm) = pm {
            let pkgs = tools.join(" ");
            let cmd = match pm {
                "apt-get" => format!("{APT_INSTALL_FN}export DEBIAN_FRONTEND=noninteractive; hb_apt_install {pkgs}"),
                "dnf" | "yum" => format!("{pm} install -y {pkgs}"),
                "apk" => format!("apk add --no-cache {pkgs}"),
                "pacman" => format!("pacman -S --noconfirm {pkgs}"),
                "brew" => format!("brew install {pkgs}"),
                "pkg" => format!("ASSUME_ALWAYS_YES=yes pkg install -y {pkgs}"),
                _ => format!("zypper install -y -n {pkgs}"),
            };
            if matches!(sh.sh(&cmd, true, Some(INSTALL_TIMEOUT)).await, Ok(r) if r.code == 0) {
                if !tmux_ok && sh.sh("tmux -V", true, None).await?.code != 0 {
                    build_tmux_from_source(sh).await;
                }
                if !asciinema_ok && sh.sh("asciinema --version", true, None).await?.code != 0 {
                    install_asciinema_with_pip(sh).await;
                }
                return Ok(());
            }
        }
        if !tmux_ok {
            build_tmux_from_source(sh).await;
        }
        if !asciinema_ok {
            install_asciinema_with_pip(sh).await;
        }
        Ok(())
    }

    pub async fn is_alive(&self, sh: &dyn Shell) -> Result<bool> {
        Ok(sh.sh(&format!("tmux has-session -t {}", self.name), false, None).await?.code == 0)
    }

    pub async fn capture_pane(&self, sh: &dyn Shell, entire: bool) -> Result<String> {
        let extra = if entire { "-S - " } else { "" };
        Ok(sh.sh(&format!("tmux capture-pane -p {extra}-t {}", self.name), false, None).await?.stdout)
    }

    fn send_error(&self, action: &str, cmd: &str, r: &ExecResult) -> anyhow::Error {
        let c: String = quote_repr(cmd).chars().take(100).collect();
        anyhow!(
            "{}: failed to send {action} keys: command={c}, return_code={}, stderr={}, stdout={}",
            self.name,
            r.code,
            quote_repr(&r.stderr),
            quote_repr(&r.stdout)
        )
    }

    /// Deliver a key too long for send-keys through a paste buffer (staged with base64 chunks).
    async fn paste_key(&self, sh: &dyn Shell, key: &str, action: &str) -> Result<()> {
        if key.is_empty() {
            return Ok(());
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        let path = quote(&format!("/tmp/.hbench-tmux-paste-{token}"));
        let payload = base64::engine::general_purpose::STANDARD.encode(key.as_bytes());
        let run = async {
            for (i, chunk) in payload.as_bytes().chunks(PASTE_CHUNK).enumerate() {
                let redirect = if i > 0 { ">>" } else { ">" };
                let cmd = format!("printf %s {} | base64 -d {redirect} {path}", std::str::from_utf8(chunk).unwrap());
                let r = sh.sh(&cmd, false, None).await?;
                if r.code != 0 {
                    return Err(self.send_error(action, &cmd, &r));
                }
            }
            let cmd = format!(
                "tmux load-buffer -b hbench-paste-{token} {path} && tmux paste-buffer -d -b hbench-paste-{token} -t {}",
                quote(&self.name)
            );
            let r = sh.sh(&cmd, false, None).await?;
            if r.code != 0 {
                return Err(self.send_error(action, &cmd, &r));
            }
            Ok(())
        }
        .await;
        sh.sh(&format!("rm -f {path}"), false, None).await?;
        run
    }

    fn too_long(r: &ExecResult) -> bool {
        r.code != 0 && r.stderr.contains("command too long")
    }

    async fn send_key_batches(&self, sh: &dyn Shell, keys: &[String], action: &str) -> Result<()> {
        for batch in self.batch_keys(keys)? {
            let cmd = self.send_keys_command(&batch);
            let r = sh.sh(&cmd, false, None).await?;
            if r.code == 0 {
                continue;
            }
            if !Self::too_long(&r) {
                return Err(self.send_error(action, &cmd, &r));
            }
            // This tmux enforces a smaller limit: resend one key at a time, pasting rejects.
            for k in batch {
                let cmd = self.send_keys_command(std::slice::from_ref(&k));
                let r = sh.sh(&cmd, false, None).await?;
                if Self::too_long(&r) {
                    self.paste_key(sh, &k, action).await?;
                } else if r.code != 0 {
                    return Err(self.send_error(action, &cmd, &r));
                }
            }
        }
        Ok(())
    }

    /// Send keys in order, pasting oversized literal keys.
    async fn send_keys_to_session(&self, sh: &dyn Shell, keys: &[String], action: &str) -> Result<()> {
        let mut batch: Vec<String> = vec![];
        for k in keys {
            if self.key_requires_paste(k) {
                if !batch.is_empty() {
                    self.send_key_batches(sh, &std::mem::take(&mut batch), action).await?;
                }
                self.paste_key(sh, k, action).await?;
            } else {
                batch.push(k.clone());
            }
        }
        if !batch.is_empty() {
            self.send_key_batches(sh, &batch, action).await?;
        }
        Ok(())
    }

    /// Returns false when a blocking command timed out.
    pub async fn send_keys(
        &self,
        sh: &dyn Shell,
        mut keys: Vec<String>,
        block: bool,
        min_timeout_sec: f64,
        max_timeout_sec: f64,
    ) -> Result<bool> {
        if block && keys.last().is_some_and(|k| is_executing_command(k)) {
            // Strip the trailing "execute" keys, then run `; tmux wait -S done` with Enter.
            while let Some(last) = keys.last().cloned().filter(|k| is_executing_command(k)) {
                let stripped = last.trim_end_matches(['\r', '\n']);
                if ENTER_KEYS.contains(&last.as_str()) || stripped.is_empty() {
                    keys.pop();
                } else {
                    *keys.last_mut().unwrap() = stripped.to_string();
                }
            }
            keys.extend([COMPLETION_COMMAND.to_string(), "Enter".to_string()]);
            self.send_keys_to_session(sh, &keys, "blocking").await?;
            let r = sh.sh(&format!("timeout {}s tmux wait done", pyf(max_timeout_sec)), false, None).await?;
            return Ok(r.code == 0);
        }
        let start = Instant::now();
        self.send_keys_to_session(sh, &keys, "non-blocking").await?;
        let remaining = min_timeout_sec - start.elapsed().as_secs_f64();
        if remaining > 0.0 && remaining.is_finite() {
            tokio::time::sleep(Duration::from_secs_f64(remaining)).await;
        }
        Ok(true)
    }

    /// New output since the last call (quirky prefix matching kept as is), else the screen.
    pub async fn get_incremental_output(&mut self, sh: &dyn Shell) -> Result<String> {
        let current = self.capture_pane(sh, true).await?;
        let Some(prev) = self.previous_buffer.replace(current.clone()) else {
            return Ok(format!("Current Terminal Screen:\n{}", self.capture_pane(sh, false).await?));
        };
        let pb = prev.trim();
        if let Some(mut idx) = current.find(pb) {
            if let Some(nl) = pb.rfind('\n') {
                // Indexes `current` with an offset into `pb` (character-based).
                let chars = pb[..nl].chars().count();
                idx = current.char_indices().nth(chars).map(|(i, _)| i).unwrap_or(current.len());
            }
            let new = &current[idx..];
            if !new.trim().is_empty() {
                return Ok(format!("New Terminal Output:\n{new}"));
            }
        }
        Ok(format!("Current Terminal Screen:\n{}", self.capture_pane(sh, false).await?))
    }
}

async fn build_tmux_from_source(sh: &dyn Shell) {
    let _ = async {
        for cmd in [
            "export DEBIAN_FRONTEND=noninteractive; hb_apt_install build-essential libevent-dev libncurses5-dev curl",
            "yum groupinstall -y 'Development Tools' && yum install -y libevent-devel ncurses-devel curl",
            "dnf groupinstall -y 'Development Tools' && dnf install -y libevent-devel ncurses-devel curl",
            "apk add --no-cache build-base libevent-dev ncurses-dev curl",
        ] {
            if sh.sh(&format!("{APT_INSTALL_FN}{cmd}"), true, Some(INSTALL_TIMEOUT)).await?.code == 0 {
                break;
            }
        }
        let v = pins::TMUX.version;
        sh.sh(
            &format!(
                "cd /tmp && curl -L https://github.com/tmux/tmux/releases/download/{v}/tmux-{v}.tar.gz -o tmux.tar.gz && tar -xzf tmux.tar.gz && \
                 cd tmux-{v} && ./configure --prefix=/usr/local && make && make install"
            ),
            true,
            Some(INSTALL_TIMEOUT),
        )
        .await?;
        sh.sh("tmux -V || /usr/local/bin/tmux -V", true, None).await
    }
    .await;
}

async fn install_asciinema_with_pip(sh: &dyn Shell) {
    let _ = async {
        for cmd in [
            "export DEBIAN_FRONTEND=noninteractive; hb_apt_install python3-pip",
            "yum install -y python3-pip",
            "dnf install -y python3-pip",
            "apk add --no-cache python3 py3-pip",
        ] {
            if sh.sh(&format!("{APT_INSTALL_FN}{cmd}"), true, Some(INSTALL_TIMEOUT)).await?.code == 0 {
                break;
            }
        }
        for pip in ["pip3", "pip"] {
            let cmd = format!("{pip} install {}", pins::ASCIINEMA.requirement());
            if sh.sh(&cmd, true, Some(INSTALL_TIMEOUT)).await?.code == 0 && sh.sh("asciinema --version", true, None).await?.code == 0 {
                break;
            }
        }
        anyhow::Ok(())
    }
    .await;
}
