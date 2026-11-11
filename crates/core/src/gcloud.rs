//! Google authentication by reusing the gcloud CLI's user credentials.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

/// gcloud access tokens live for 60 minutes; refresh well before that.
const TOKEN_TTL: Duration = Duration::from_secs(45 * 60);

/// Upper bound for non-interactive gcloud commands.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait for the user to finish signing in in the browser.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

static TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// Finds the gcloud binary. Apps launched from Finder get a minimal PATH, so
/// also look in the usual install locations.
fn gcloud_path() -> Result<PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).map(|d| d.join("gcloud")).collect())
            .unwrap_or_default();
        candidates.extend(
            [
                "/opt/homebrew/bin/gcloud",
                "/usr/local/bin/gcloud",
                "/opt/homebrew/share/google-cloud-sdk/bin/gcloud",
                "/usr/local/share/google-cloud-sdk/bin/gcloud",
            ]
            .map(PathBuf::from),
        );
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(home).join("google-cloud-sdk/bin/gcloud"));
        }
        candidates.into_iter().find(|p| p.is_file())
    })
    .clone()
    .context("gcloud CLI not found. Install the Google Cloud SDK and make sure `gcloud` is available.")
}

fn gcloud_command(args: &[&str]) -> Result<Command> {
    let path = gcloud_path()?;
    let mut cmd = Command::new(&path);
    cmd.args(args).stdin(Stdio::null()).kill_on_drop(true);
    // gcloud is a script that needs its own bin dir (and Homebrew's) on PATH.
    let mut paths = vec![path.parent().unwrap().to_path_buf(), "/opt/homebrew/bin".into(), "/usr/local/bin".into()];
    if let Some(p) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&p));
    }
    cmd.env("PATH", std::env::join_paths(paths)?);
    Ok(cmd)
}

async fn run_gcloud(args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(COMMAND_TIMEOUT, gcloud_command(args)?.output())
        .await
        .map_err(|_| anyhow::anyhow!("gcloud {} timed out", args.join(" ")))?
        .context("failed to run gcloud")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("gcloud {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The active gcloud account, or `None` when nobody is logged in.
pub async fn active_account() -> Result<Option<String>> {
    let out = run_gcloud(&["auth", "list", "--filter=status:ACTIVE", "--format=value(account)"]).await?;
    Ok(out.lines().next().map(str::to_string).filter(|s| !s.is_empty()))
}

static LOGIN_CANCEL: Mutex<Option<oneshot::Sender<()>>> = Mutex::new(None);

/// Runs `gcloud auth login`, which opens the browser and waits for Google to redirect back.
/// The sign-in URL gcloud prints is sent to `url_tx` so the UI can offer it again.
/// Ends on success, failure, [`cancel_login`], or after [`LOGIN_TIMEOUT`].
pub async fn login(url_tx: mpsc::UnboundedSender<String>) -> Result<()> {
    let (cancel_tx, cancel_rx) = oneshot::channel();
    *LOGIN_CANCEL.lock().unwrap() = Some(cancel_tx);

    let mut child = gcloud_command(&["auth", "login", "--brief"])?
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run gcloud")?;

    // gcloud reports progress (including the URL) on stderr; keep the tail for error messages.
    let stderr = child.stderr.take().context("no stderr")?;
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut tail: Vec<String> = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim().to_string();
            if line.starts_with("https://") {
                let _ = url_tx.send(line.clone());
            }
            if !line.is_empty() {
                tail.push(line);
            }
        }
        tail
    });

    let outcome = tokio::select! {
        status = child.wait() => Some(status?),
        _ = cancel_rx => None,
        _ = tokio::time::sleep(LOGIN_TIMEOUT) => {
            let _ = child.kill().await;
            bail!("Timed out waiting for the browser sign-in.");
        }
    };
    let Some(status) = outcome else {
        let _ = child.kill().await;
        bail!("Sign-in cancelled.");
    };
    let tail = reader.await.unwrap_or_default();
    if !status.success() {
        let start = tail.len().saturating_sub(6);
        bail!("gcloud auth login failed:\n{}", tail[start..].join("\n"));
    }
    invalidate_token();
    Ok(())
}

/// Stops a running [`login`].
pub fn cancel_login() {
    if let Some(tx) = LOGIN_CANCEL.lock().unwrap().take() {
        let _ = tx.send(());
    }
}

/// Fallback when the browser flow misbehaves: runs `gcloud auth login` in a new Terminal window,
/// where gcloud can also prompt interactively (e.g. for Google's re-authentication).
pub fn login_in_terminal() -> Result<()> {
    let gcloud = gcloud_path()?;
    let script = std::env::temp_dir().join("cirrus-login.command");
    std::fs::write(
        &script,
        format!(
            "#!/bin/zsh\n'{}' auth login && echo '\\nSigned in. You can close this window and click \"Check again\" in Cirrus.'\n",
            gcloud.display()
        ),
    )?;
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let status = std::process::Command::new("open").arg("-a").arg("Terminal").arg(&script).status()?;
    if !status.success() {
        bail!("could not open Terminal");
    }
    Ok(())
}

/// An OAuth2 access token for the active account (cached).
pub async fn access_token() -> Result<String> {
    if let Some((token, at)) = TOKEN.lock().unwrap().as_ref()
        && at.elapsed() < TOKEN_TTL
    {
        return Ok(token.clone());
    }
    let token = run_gcloud(&["auth", "print-access-token"]).await?;
    if token.is_empty() {
        bail!("gcloud returned an empty access token; run `gcloud auth login`");
    }
    *TOKEN.lock().unwrap() = Some((token.clone(), Instant::now()));
    Ok(token)
}

/// True when gcloud failed only because the user has to sign in (again), e.g. an expired
/// session or Google's periodic re-authentication — as opposed to a real error.
pub fn needs_login(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    ["Reauthentication", "gcloud auth login", "no credentialed accounts", "invalid_grant"]
        .iter()
        .any(|needle| msg.contains(needle))
}

pub fn invalidate_token() {
    *TOKEN.lock().unwrap() = None;
}

/// The PostgreSQL user name Cloud SQL assigns to an IAM principal.
pub fn iam_db_user(account: &str) -> String {
    account
        .strip_suffix(".gserviceaccount.com")
        .unwrap_or(account)
        .to_string()
}
