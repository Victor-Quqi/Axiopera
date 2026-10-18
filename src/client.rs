//! CLI side: reach the daemon (starting it if needed), send one request, hand back the replies.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::ipc::{self, Envelope, ErrorCode, Reply, Request};
use crate::{identity, platform};

const SANDBOX_HINT: &str =
    "re-run this command with escalated permissions outside the agent sandbox";

pub struct Client {
    pub home: PathBuf,
    pub json: bool,
}

/// One line of the daemon's answer.
pub enum Line {
    Out { text: String, data: Value },
    Error(String),
}

impl Client {
    /// Send one request and print the replies. Returns the exit code.
    pub async fn run(&self, request: Request) -> Result<i32> {
        self.request(request, |line| match line {
            Line::Out { text, data } if self.json => {
                // The text carries result bodies that `data` does not repeat.
                let mut line = serde_json::json!({"text": text});
                if !data.is_null() {
                    line["data"] = data;
                }
                println!("{line}");
            }
            Line::Out { text, .. } if !text.is_empty() => println!("{text}"),
            Line::Out { .. } => {}
            Line::Error(message) => eprintln!("axiopera: {message}"),
        })
        .await
    }

    /// Send one request, handing each line of the answer to `sink`. Returns the exit code.
    pub async fn request(&self, request: Request, sink: impl FnMut(Line)) -> Result<i32> {
        let caller = identity::detect(|k| std::env::var(k).ok(), platform::ancestor_names);
        let launch = !matches!(request, Request::Stop { .. });
        self.request_as(caller, launch, request, sink).await
    }

    /// Without `launch`, a daemon that is not running answers nothing.
    pub async fn request_as(
        &self,
        caller: String,
        launch: bool,
        request: Request,
        mut sink: impl FnMut(Line),
    ) -> Result<i32> {
        if !request.allowed_in_sandbox() && platform::self_sandboxed() {
            sink(Line::Error(SANDBOX_HINT.into()));
            return Ok(ipc::exit::ERROR);
        }
        let env = request.needs_env().then(|| std::env::vars().collect());
        let envelope = Envelope {
            protocol: ipc::PROTOCOL,
            version: env!("CARGO_PKG_VERSION").into(),
            caller,
            cwd: std::env::current_dir()?,
            env,
            request,
        };
        // A version mismatch makes an idle daemon exit; then a fresh one is started.
        for _ in 0..3 {
            let stream = self.connect(launch).await?;
            let Some(stream) = stream else {
                return Ok(ipc::exit::OK);
            };
            match self.exchange(stream, &envelope, &mut sink).await? {
                Some(code) => return Ok(code),
                None => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
        bail!("daemon version mismatch persists; run `axiopera daemon stop --force`")
    }

    /// Ok(None) only without `may_launch`, when nothing is running.
    async fn connect(&self, may_launch: bool) -> Result<Option<platform::ClientStream>> {
        match platform::connect(&self.home).await {
            Ok(s) => return Ok(Some(s)),
            // Sandboxes that block sockets fail here rather than with "not found".
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => bail!("{SANDBOX_HINT}"),
            Err(_) => {}
        }
        if !may_launch {
            return Ok(None);
        }
        if platform::self_sandboxed() {
            bail!("{SANDBOX_HINT}");
        }
        launch(&self.home).await.map(Some)
    }

    /// Returns the exit code, or None when the daemon asked us to retry after a version change.
    async fn exchange(
        &self,
        stream: platform::ClientStream,
        envelope: &Envelope,
        sink: &mut impl FnMut(Line),
    ) -> Result<Option<i32>> {
        let (r, mut w) = tokio::io::split(stream);
        ipc::write(&mut w, envelope).await?;
        let mut reader = ipc::Reader::new(r);
        while let Some(reply) = reader.read::<Reply>().await? {
            match reply {
                Reply::Out { text, data } => sink(Line::Out { text, data }),
                Reply::Done { exit } => return Ok(Some(exit)),
                Reply::Error {
                    code: ErrorCode::Version,
                    ..
                } => return Ok(None),
                Reply::Error { message, .. } => {
                    sink(Line::Error(message));
                    return Ok(Some(ipc::exit::ERROR));
                }
            }
        }
        bail!("daemon closed the connection without finishing")
    }
}

/// Start the daemon unless another caller is already doing it, then wait for its endpoint.
async fn launch(home: &Path) -> Result<platform::ClientStream> {
    let dir = home.join("daemon");
    std::fs::create_dir_all(&dir)?;
    let lock = File::create(dir.join("launch.lock"))
        .with_context(|| format!("writing {}", dir.display()))?;
    tokio::task::block_in_place(|| lock.lock()).context("locking launch.lock")?;
    if let Ok(s) = platform::connect(home).await {
        return Ok(s);
    }
    let exe = std::env::current_exe()?;
    let home_arg = home.to_string_lossy();
    // A broken config.toml would only show up as a daemon that never answers.
    crate::config::load(home)?;
    platform::spawn_detached(&exe, &["daemon", "run", "--home", &home_arg])
        .context("starting the daemon")?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(s) = platform::connect(home).await {
            return Ok(s);
        }
    }
    bail!(
        "the daemon did not come up; see {}",
        dir.join("daemon.log").display()
    )
}
