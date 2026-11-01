//! Cloudflare quick tunnels: the host's local session endpoint becomes a
//! public `trycloudflare.com` link without any account or configuration.
//!
//! `cloudflared` is used from the user's `PATH` when it is installed and is
//! otherwise downloaded into the application cache on first use.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use tokio::{
    io::{AsyncBufReadExt as _, BufReader},
    process::{Child, Command},
    sync::{mpsc, watch},
};

const START_TIMEOUT: Duration = Duration::from_secs(60);
const DOWNLOAD_URL: &str = "https://github.com/cloudflare/cloudflared/releases/latest/download/";

/// The tunnel's lifecycle, observed by the session host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TunnelStatus {
    Running,
    Stopped,
}

/// One running quick tunnel.
pub struct Tunnel {
    binary: PathBuf,
    child: Option<Child>,
    status: watch::Sender<TunnelStatus>,
}

impl Tunnel {
    /// Locates `cloudflared`, downloading it into the application cache when
    /// this machine has no copy of it.
    pub async fn acquire() -> Result<Self> {
        Ok(Self {
            binary: locate().await?,
            child: None,
            status: watch::channel(TunnelStatus::Stopped).0,
        })
    }

    /// Starts a quick tunnel to `origin` and waits for the public URL.
    pub async fn open(&mut self, origin: String) -> Result<String> {
        let mut child = Command::new(&self.binary)
            .args(["tunnel", "--url", &origin])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start {}", self.binary.display()))?;

        let mut lines = collector(child.stdout.take(), child.stderr.take());
        let url = await_url(&mut lines, &mut child).await;
        let Some(url) = url else {
            child.start_kill().ok();
            let _ = child.wait().await;
            let mut tail = Vec::new();
            while let Ok(Some(line)) =
                tokio::time::timeout(Duration::from_secs(1), lines.recv()).await
            {
                tail.push(line);
            }
            bail!(
                "cloudflared did not publish a trycloudflare link{}",
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(":\n{}", tail.join("\n"))
                }
            );
        };

        self.child = Some(child);
        self.status.send_replace(TunnelStatus::Running);
        // Keep draining output so the tunnel never blocks on a full pipe, and
        // notice when it stops on its own.
        let status = self.status.clone();
        tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                tracing::debug!(target: "cloudflared", "{line}");
            }
            tracing::info!("cloudflared stopped");
            status.send_replace(TunnelStatus::Stopped);
        });
        Ok(url)
    }

    pub fn subscribe(&self) -> watch::Receiver<TunnelStatus> {
        self.status.subscribe()
    }

    /// Stops the tunnel. Project state is unaffected.
    pub async fn close(mut self) {
        if let Some(mut child) = self.child.take() {
            child.start_kill().ok();
            let _ = child.wait().await;
        }
        self.status.send_replace(TunnelStatus::Stopped);
    }
}

async fn await_url(lines: &mut mpsc::Receiver<String>, child: &mut Child) -> Option<String> {
    let deadline = tokio::time::sleep(START_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            line = lines.recv() => match line {
                Some(line) => {
                    tracing::debug!(target: "cloudflared", "{line}");
                    if let Some(url) = public_url(&line) {
                        return Some(url);
                    }
                }
                None => return None,
            },
            _ = &mut deadline => return None,
            status = child.wait() => {
                tracing::warn!(?status, "cloudflared exited before publishing a link");
                return None;
            }
        }
    }
}

async fn locate() -> Result<PathBuf> {
    if let Some(path) = on_path("cloudflared") {
        return Ok(path);
    }
    let cached = cache_path();
    if cached.is_file() {
        return Ok(cached);
    }
    download(&cached).await?;
    Ok(cached)
}

fn on_path(name: &str) -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|entry| !entry.as_os_str().is_empty())
        .map(|entry| entry.join(&executable))
        .find(|candidate| candidate.is_file())
}

fn cache_path() -> PathBuf {
    let name = if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    };
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("koharu")
        .join("cloudflared")
        .join(name)
}

async fn download(target: &Path) -> Result<()> {
    let url = format!("{DOWNLOAD_URL}{}", release_asset()?);
    tracing::info!(%url, "downloading cloudflared");
    let response = reqwest::Client::builder()
        .user_agent(format!("koharu/{}", env!("CARGO_PKG_VERSION")))
        .build()?
        .get(&url)
        .send()
        .await
        .context("cloudflared download failed")?
        .error_for_status()
        .context("cloudflared download was refused")?;

    let parent = target
        .parent()
        .context("cloudflared cache path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temporary = target.with_extension("part");
    let bytes = response
        .bytes()
        .await
        .context("cloudflared download was interrupted")?;
    #[cfg(target_os = "macos")]
    let bytes = unpack_binary(&bytes)?;
    std::fs::write(&temporary, &bytes).context("failed to write the cloudflared binary")?;
    std::fs::rename(&temporary, target).context("failed to install the cloudflared binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// The release asset for this platform. Names follow cloudflared's release
/// page: Windows assets carry `.exe`, macOS ships only `.tgz` archives, and
/// there is no Windows ARM build.
fn release_asset() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("cloudflared-linux-amd64"),
        ("linux", "aarch64") => Ok("cloudflared-linux-arm64"),
        ("macos", "x86_64") => Ok("cloudflared-darwin-amd64.tgz"),
        ("macos", "aarch64") => Ok("cloudflared-darwin-arm64.tgz"),
        ("windows", "x86_64") => Ok("cloudflared-windows-amd64.exe"),
        (os, arch) => bail!("cloudflared is not published for {os}-{arch}"),
    }
}

/// Pulls the `cloudflared` executable out of a macOS release archive.
#[cfg(target_os = "macos")]
fn unpack_binary(archive: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read as _;

    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in archive
        .entries()
        .context("cloudflared archive is unreadable")?
    {
        let mut entry = entry.context("cloudflared archive is unreadable")?;
        if entry.path()?.file_name() == Some("cloudflared".as_ref()) {
            let mut binary = Vec::new();
            entry.read_to_end(&mut binary)?;
            return Ok(binary);
        }
    }
    bail!("cloudflared archive has no cloudflared binary")
}

/// Reads both of cloudflared's output streams into one channel; the caller
/// scans it for the public URL and then drains whatever is left.
fn collector(
    stdout: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
    stderr: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel::<String>(128);
    if let Some(stdout) = stdout {
        tokio::spawn(pump(BufReader::new(stdout), tx.clone()));
    }
    if let Some(stderr) = stderr {
        tokio::spawn(pump(BufReader::new(stderr), tx));
    }
    rx
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(mut reader: BufReader<R>, tx: mpsc::Sender<String>) {
    let mut line = String::new();
    while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
        let trimmed = line.trim().to_owned();
        line.clear();
        if !trimmed.is_empty() && tx.send(trimmed).await.is_err() {
            return;
        }
    }
}

/// Pulls the public URL out of one cloudflared log line.
pub fn public_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest.find("trycloudflare.com")? + "trycloudflare.com".len();
    let candidate = &rest[..end];
    if candidate
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-./:".contains(c))
    {
        Some(candidate.to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_public_link_in_cloudflared_output() {
        let line = "2026-01-02T03:04:05Z INF + https://neat-random-words-here.trycloudflare.com";
        assert_eq!(
            public_url(line).as_deref(),
            Some("https://neat-random-words-here.trycloudflare.com")
        );
        assert_eq!(public_url("INF starting tunnel"), None);
        assert_eq!(public_url("https://example.com done"), None);
    }
}
