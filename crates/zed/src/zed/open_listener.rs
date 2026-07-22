use crate::handle_open_request;
use agent_ui::ExternalSourcePrompt;
use anyhow::{Context as _, Result, anyhow};
use cli::{CliRequest, CliResponse, CliResponseSink};
use cli::{IpcHandshake, ipc};
use client::{ZedLink, parse_zed_link};
use fs::Fs;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::channel::{mpsc, oneshot};
use futures::future;

use futures::{FutureExt, StreamExt};
use git_ui::{file_diff_view::FileDiffView, multi_diff_view::MultiDiffView};
use gpui::{App, AsyncApp, Global, WindowHandle};
use recent_projects::{RemoteSettings, navigate_to_positions};
use remote::{RemoteConnectionOptions, WslConnectionOptions};
use settings::Settings;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use ui::SharedString;
use util::ResultExt;
use util::paths::PathWithPosition;
use workspace::item::ItemHandle;
use workspace::{AppState, MultiWorkspace, OpenResult, SerializedWorkspaceLocation};

#[derive(Default, Debug)]
pub struct OpenRequest {
    pub kind: Option<OpenRequestKind>,
    pub open_paths: Vec<String>,
    pub diff_paths: Vec<[String; 2]>,
    pub diff_all: bool,
    pub dev_container: bool,
    pub open_channel_notes: Vec<(u64, Option<String>)>,
    pub join_channel: Option<u64>,
    pub remote_connection: Option<RemoteConnectionOptions>,
    pub open_behavior: Option<cli::OpenBehavior>,
}

pub enum OpenRequestKind {
    CliConnection(
        (
            mpsc::UnboundedReceiver<CliRequest>,
            Box<dyn CliResponseSink>,
        ),
    ),
    FocusApp,
    Extension {
        extension_id: String,
    },
    AgentPanel {
        external_source_prompt: Option<ExternalSourcePrompt>,
    },
    InstallSkill {
        /// Full `SKILL.md` contents embedded in a `zed://skill` share link.
        content: String,
    },
    DockMenuAction {
        index: usize,
    },
    BuiltinJsonSchema {
        schema_path: String,
    },
    Setting {
        /// `None` opens settings without navigating to a specific path.
        setting_path: Option<String>,
    },
    GitClone {
        repo_url: SharedString,
    },
    GitCommit {
        sha: String,
    },
}

impl std::fmt::Debug for OpenRequestKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CliConnection(_) => write!(f, "CliConnection(..)"),
            Self::FocusApp => write!(f, "FocusApp"),
            Self::Extension { extension_id } => f
                .debug_struct("Extension")
                .field("extension_id", extension_id)
                .finish(),
            Self::AgentPanel {
                external_source_prompt,
            } => f
                .debug_struct("AgentPanel")
                .field("external_source_prompt", external_source_prompt)
                .finish(),
            Self::InstallSkill { content } => f
                .debug_struct("InstallSkill")
                .field("content_len", &content.len())
                .finish(),
            Self::DockMenuAction { index } => f
                .debug_struct("DockMenuAction")
                .field("index", index)
                .finish(),
            Self::BuiltinJsonSchema { schema_path } => f
                .debug_struct("BuiltinJsonSchema")
                .field("schema_path", schema_path)
                .finish(),
            Self::Setting { setting_path } => f
                .debug_struct("Setting")
                .field("setting_path", setting_path)
                .finish(),
            Self::GitClone { repo_url } => f
                .debug_struct("GitClone")
                .field("repo_url", repo_url)
                .finish(),
            Self::GitCommit { sha } => f.debug_struct("GitCommit").field("sha", sha).finish(),
        }
    }
}

impl OpenRequest {
    pub fn is_focus_app_only(&self) -> bool {
        matches!(self.kind, Some(OpenRequestKind::FocusApp))
            && self.open_paths.is_empty()
            && self.diff_paths.is_empty()
            && self.remote_connection.is_none()
            && self.join_channel.is_none()
            && self.open_channel_notes.is_empty()
    }

    pub fn parse(request: RawOpenRequest, cx: &App) -> Result<Self> {
        let mut this = Self::default();

        this.diff_paths = request.diff_paths;
        this.diff_all = request.diff_all;
        this.dev_container = request.dev_container;
        this.open_behavior = request.open_behavior;
        if let Some(wsl) = request.wsl {
            let (user, distro_name) = if let Some((user, distro)) = wsl.split_once('@') {
                if user.is_empty() {
                    anyhow::bail!("user is empty in wsl argument");
                }
                (Some(user.to_string()), distro.to_string())
            } else {
                (None, wsl)
            };
            this.remote_connection = Some(RemoteConnectionOptions::Wsl(WslConnectionOptions {
                distro_name,
                user,
            }));
        }

        for url in request.urls {
            if let Some(server_name) = url.strip_prefix("zed-cli://") {
                this.kind = Some(OpenRequestKind::CliConnection(connect_to_cli(server_name)?));
            } else if let Some(action_index) = url.strip_prefix("zed-dock-action://") {
                this.kind = Some(OpenRequestKind::DockMenuAction {
                    index: action_index.parse()?,
                });
            } else if let Some(file) = url.strip_prefix("file://") {
                this.parse_file_path(file)
            } else if let Some(file) = url.strip_prefix("zed://file") {
                this.parse_file_path(file)
            } else if let Some(file) = url.strip_prefix("zed://ssh") {
                let ssh_url = "ssh:/".to_string() + file;
                this.parse_ssh_file_path(&ssh_url, cx)?
            } else if let Some(extension_id) = url.strip_prefix("zed://extension/") {
                this.kind = Some(OpenRequestKind::Extension {
                    extension_id: extension_id.to_string(),
                });
            } else if url.starts_with(agent_skills::SKILL_SHARE_LINK_PREFIX) {
                this.parse_skill_install_url(&url)?
            } else if let Some(agent_path) = url.strip_prefix("zed://agent") {
                this.parse_agent_url(agent_path)
            } else if url == "zed://" || url == "zed://open" || url == "zed://open/" {
                this.kind = Some(OpenRequestKind::FocusApp);
            } else if let Some(schema_path) = url.strip_prefix("zed://schemas/") {
                this.kind = Some(OpenRequestKind::BuiltinJsonSchema {
                    schema_path: schema_path.to_string(),
                });
            } else if url == "zed://settings" || url == "zed://settings/" {
                this.kind = Some(OpenRequestKind::Setting { setting_path: None });
            } else if let Some(setting_path) = url.strip_prefix("zed://settings/") {
                this.kind = Some(OpenRequestKind::Setting {
                    setting_path: Some(setting_path.to_string()),
                });
            } else if let Some(clone_path) = url.strip_prefix("zed://git/clone") {
                this.parse_git_clone_url(clone_path)?
            } else if let Some(commit_path) = url.strip_prefix("zed://git/commit/") {
                this.parse_git_commit_url(commit_path)?
            } else if url.starts_with("ssh://") {
                this.parse_ssh_file_path(&url, cx)?
            } else if let Some(zed_link) = parse_zed_link(&url, cx) {
                match zed_link {
                    ZedLink::Channel { channel_id } => {
                        this.join_channel = Some(channel_id);
                    }
                    ZedLink::ChannelNotes {
                        channel_id,
                        heading,
                    } => {
                        this.open_channel_notes.push((channel_id, heading));
                    }
                }
            } else {
                log::error!("unhandled url: {}", url);
            }
        }

        Ok(this)
    }

    fn parse_file_path(&mut self, file: &str) {
        if let Some(decoded) = urlencoding::decode(file).log_err() {
            self.open_paths.push(decoded.into_owned())
        }
    }

    fn parse_agent_url(&mut self, agent_path: &str) {
        // Format: "" or "?prompt=<text>".
        let agent_path = agent_path.strip_prefix('/').unwrap_or(agent_path);
        let external_source_prompt = agent_path.strip_prefix('?').and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find_map(|(key, value)| (key == "prompt").then_some(value))
                .and_then(|prompt| ExternalSourcePrompt::new(prompt.as_ref()))
        });
        self.kind = Some(OpenRequestKind::AgentPanel {
            external_source_prompt,
        });
    }

    fn parse_skill_install_url(&mut self, url: &str) -> Result<()> {
        // Format: zed://skill?data=<base64url of SKILL.md contents>
        let content = agent_skills::decode_skill_share_link(url)?;
        self.kind = Some(OpenRequestKind::InstallSkill { content });
        Ok(())
    }

    fn parse_git_clone_url(&mut self, clone_path: &str) -> Result<()> {
        // Format: /?repo=<url> or ?repo=<url>
        let clone_path = clone_path.strip_prefix('/').unwrap_or(clone_path);

        let query = clone_path
            .strip_prefix('?')
            .context("invalid git clone url: missing query string")?;

        let repo_url = url::form_urlencoded::parse(query.as_bytes())
            .find_map(|(key, value)| (key == "repo").then_some(value))
            .filter(|s| !s.is_empty())
            .context("invalid git clone url: missing repo query parameter")?
            .to_string()
            .into();

        self.kind = Some(OpenRequestKind::GitClone { repo_url });

        Ok(())
    }

    fn parse_git_commit_url(&mut self, commit_path: &str) -> Result<()> {
        // Format: <sha>?repo=<path>
        let (sha, query) = commit_path
            .split_once('?')
            .context("invalid git commit url: missing query string")?;
        anyhow::ensure!(!sha.is_empty(), "invalid git commit url: missing sha");

        let repo = url::form_urlencoded::parse(query.as_bytes())
            .find_map(|(key, value)| (key == "repo").then_some(value))
            .filter(|s| !s.is_empty())
            .context("invalid git commit url: missing repo query parameter")?
            .to_string();

        self.open_paths.push(repo);

        self.kind = Some(OpenRequestKind::GitCommit {
            sha: sha.to_string(),
        });

        Ok(())
    }

    fn parse_ssh_file_path(&mut self, file: &str, cx: &App) -> Result<()> {
        let url = parse_ssh_url(file)?;
        let host = match url
            .host()
            .with_context(|| format!("missing host in ssh url: {url}"))?
        {
            url::Host::Domain(host) => host.to_string(),
            url::Host::Ipv4(host) => host.to_string(),
            url::Host::Ipv6(host) => host.to_string(),
        };
        let username = if url.username().is_empty() {
            None
        } else {
            Some(urlencoding::decode(url.username())?.into_owned())
        };
        let port = url.port();
        anyhow::ensure!(
            self.open_paths.is_empty(),
            "cannot open both local and ssh paths"
        );
        let mut connection_options =
            RemoteSettings::get_global(cx).connection_options_for(host, port, username);
        if let Some(password) = url.password() {
            connection_options.password = Some(urlencoding::decode(password)?.into_owned());
        }

        let connection_options = RemoteConnectionOptions::Ssh(connection_options);
        if let Some(ssh_connection) = &self.remote_connection {
            anyhow::ensure!(
                *ssh_connection == connection_options,
                "cannot open multiple different remote connections"
            );
        }
        self.remote_connection = Some(connection_options);
        self.parse_file_path(url.path());
        Ok(())
    }
}

fn parse_ssh_url(url: &str) -> Result<url::Url> {
    if let Ok(url) = url::Url::parse(url) {
        return Ok(url);
    }
    // SCP/git style urls use ':' to separate from Authority and Path.
    // They are unsupported by Url::parse, but can be normalized into a Url.
    //   SCPUrl("ssh://user@host:~/relpath") => Url("ssh://user@host/~/relpath")
    //   SCPUrl("ssh://user@host:/abs/path") => Url("ssh://user@host/abs/path")
    //
    // TODO: Add IPv6 support: "ssh://[2600::]:~/foo"
    let ssh_target = url
        .strip_prefix("ssh://")
        .with_context(|| format!("invalid ssh url: {url}"))?;

    let (authority, path) = if let Some((authority, path)) = ssh_target.rsplit_once(":~/") {
        (authority, format!("/~/{path}"))
    } else if let Some((authority, path)) = ssh_target.rsplit_once(":/") {
        (authority, format!("/{path}"))
    } else {
        anyhow::bail!("invalid ssh url: {url}");
    };

    let (userinfo, host) = authority
        .rsplit_once('@')
        .map_or((None, authority), |(userinfo, host)| (Some(userinfo), host));
    anyhow::ensure!(
        !host.is_empty() && !host.starts_with('[') && !host.contains(':'),
        "invalid ssh url: {url}"
    );

    let normalized_authority = if let Some(userinfo) = userinfo {
        let (username, colon_password) =
            if let Some((username, password)) = userinfo.split_once(':') {
                (
                    urlencoding::encode(&urlencoding::decode(username)?).into_owned(),
                    format!(
                        ":{}",
                        urlencoding::encode(&urlencoding::decode(password)?).into_owned()
                    ),
                )
            } else {
                (
                    urlencoding::encode(&urlencoding::decode(userinfo)?).into_owned(),
                    String::new(),
                )
            };
        format!("{username}{colon_password}@{host}")
    } else {
        authority.to_string()
    };

    Ok(url::Url::parse(&format!(
        "ssh://{normalized_authority}{path}"
    ))?)
}

#[derive(Clone)]
pub struct OpenListener(UnboundedSender<RawOpenRequest>);

#[derive(Default)]
pub struct RawOpenRequest {
    pub urls: Vec<String>,
    pub diff_paths: Vec<[String; 2]>,
    pub diff_all: bool,
    pub dev_container: bool,
    pub wsl: Option<String>,
    pub open_behavior: Option<cli::OpenBehavior>,
}

impl Global for OpenListener {}

impl OpenListener {
    pub fn new() -> (Self, UnboundedReceiver<RawOpenRequest>) {
        let (tx, rx) = mpsc::unbounded();
        (OpenListener(tx), rx)
    }

    pub fn open(&self, request: RawOpenRequest) {
        self.0
            .unbounded_send(request)
            .context("no listener for open requests")
            .log_err();
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn listen_for_cli_connections(opener: OpenListener) -> Result<()> {
    use release_channel::RELEASE_CHANNEL_NAME;
    use std::os::unix::net::UnixDatagram;

    let sock_path = paths::data_dir().join(format!("zed-{}.sock", *RELEASE_CHANNEL_NAME));
    // remove the socket if the process listening on it has died
    if let Err(e) = UnixDatagram::unbound()?.connect(&sock_path)
        && e.kind() == std::io::ErrorKind::ConnectionRefused
    {
        std::fs::remove_file(&sock_path)?;
    }
    let listener = UnixDatagram::bind(&sock_path)?;
    thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok(len) = listener.recv(&mut buf) {
            opener.open(RawOpenRequest {
                urls: vec![String::from_utf8_lossy(&buf[..len]).to_string()],
                ..Default::default()
            });
        }
    });
    Ok(())
}

fn connect_to_cli(
    server_name: &str,
) -> Result<(
    mpsc::UnboundedReceiver<CliRequest>,
    Box<dyn CliResponseSink>,
)> {
    let handshake_tx = ipc::IpcSender::<IpcHandshake>::connect(server_name.to_string())
        .context("error connecting to cli")?;
    let (request_tx, request_rx) = ipc::channel::<CliRequest>()?;
    let (response_tx, response_rx) = ipc::channel::<CliResponse>()?;

    handshake_tx
        .send(IpcHandshake {
            requests: request_tx,
            responses: response_rx,
        })
        .context("error sending ipc handshake")?;

    let (async_request_tx, async_request_rx) = futures::channel::mpsc::unbounded::<CliRequest>();
    thread::spawn(move || {
        while let Ok(cli_request) = request_rx.recv() {
            if async_request_tx.unbounded_send(cli_request).is_err() {
                break;
            }
        }
        anyhow::Ok(())
    });

    Ok((async_request_rx, Box::new(response_tx)))
}

pub async fn open_superzed_paths_with_positions(
    path_positions: &[PathWithPosition],
    diff_paths: &[[String; 2]],
    diff_all: bool,
    app_state: Arc<AppState>,
    open_options: workspace::OpenOptions,
    cx: &mut AsyncApp,
) -> Result<(
    WindowHandle<MultiWorkspace>,
    Vec<Option<Result<Box<dyn ItemHandle>>>>,
)> {
    let paths = path_positions
        .iter()
        .map(|path_with_position| path_with_position.path.clone())
        .collect::<Vec<_>>();

    let mut project_paths = paths.clone();
    for path in diff_paths.iter().flatten() {
        let path = PathBuf::from(path);
        let path = app_state.fs.canonicalize(&path).await.unwrap_or(path);
        if !project_paths.contains(&path) {
            project_paths.push(path);
        }
    }

    let target = cx.update(|cx| {
        workspace::host_workspace_identity_for_open(open_options.requesting_window, cx)
    })?;
    let OpenResult {
        window: multi_workspace,
        opened_items: mut items,
        ..
    } = cx
        .update(|cx| {
            workspace::open_superzed_paths(target, &project_paths, &paths, open_options, cx)
        })
        .await?;

    if diff_all && !diff_paths.is_empty() {
        if let Ok(diff_view) = multi_workspace.update(cx, |multi_workspace, window, cx| {
            multi_workspace.workspace().update(cx, |workspace, cx| {
                MultiDiffView::open(diff_paths.to_vec(), workspace, window, cx)
            })
        }) {
            if let Some(diff_view) = diff_view.await.log_err() {
                items.push(Some(Ok(Box::new(diff_view))));
            }
        }
    } else {
        let workspace_weak = multi_workspace.read_with(cx, |multi_workspace, _cx| {
            multi_workspace.workspace().downgrade()
        })?;
        let canonicalize = async |raw: &str| {
            app_state
                .fs
                .canonicalize(Path::new(raw))
                .await
                .with_context(|| format!("opening --diff path {raw:?}"))
        };
        for diff_pair in diff_paths {
            let (old_path, new_path) =
                match futures::join!(canonicalize(&diff_pair[0]), canonicalize(&diff_pair[1])) {
                    (Ok(old), Ok(new)) => (old, new),
                    (old, new) => {
                        for result in [old, new] {
                            if let Err(err) = result {
                                items.push(Some(Err(err)));
                            }
                        }
                        continue;
                    }
                };
            if let Ok(diff_view) = multi_workspace.update(cx, |_multi_workspace, window, cx| {
                FileDiffView::open(old_path, new_path, workspace_weak.clone(), window, cx)
            }) {
                if let Some(diff_view) = diff_view.await.log_err() {
                    items.push(Some(Ok(Box::new(diff_view))))
                }
            }
        }
    }

    for (item, path) in items.iter_mut().zip(&paths) {
        if let Some(Err(error)) = item {
            *error = anyhow!("error opening {path:?}: {error:#}");
        }
    }

    let items_for_navigation = items
        .iter()
        .map(|item| item.as_ref().and_then(|r| r.as_ref().ok()).cloned())
        .collect::<Vec<_>>();
    navigate_to_positions(&multi_workspace, items_for_navigation, path_positions, cx);

    Ok((multi_workspace, items))
}

pub async fn handle_cli_connection(
    (mut requests, responses): (
        mpsc::UnboundedReceiver<CliRequest>,
        Box<dyn CliResponseSink>,
    ),
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) {
    if let Some(request) = requests.next().await {
        match request {
            CliRequest::Open {
                urls,
                paths,
                diff_paths,
                diff_all,
                wait,
                wsl,
                mut open_behavior,
                env,
                user_data_dir: _,
                dev_container,
                cwd,
            } => {
                if !urls.is_empty() {
                    cx.update(|cx| {
                        match OpenRequest::parse(
                            RawOpenRequest {
                                urls,
                                diff_paths,
                                diff_all,
                                dev_container,
                                wsl,
                                open_behavior: Some(open_behavior),
                            },
                            cx,
                        ) {
                            Ok(open_request) => {
                                cx.activate(true);
                                handle_open_request(open_request, app_state.clone(), cx);
                                responses.send(CliResponse::Exit { status: 0 }).log_err();
                            }
                            Err(e) => {
                                responses
                                    .send(CliResponse::Stderr {
                                        message: format!("{e}"),
                                    })
                                    .log_err();
                                responses.send(CliResponse::Exit { status: 1 }).log_err();
                            }
                        };
                    });
                    return;
                }

                if open_behavior == cli::OpenBehavior::Default {
                    open_behavior = cli::OpenBehavior::ExistingWindow;
                }

                cx.update(|cx| cx.activate(true));

                let open_workspace_result = open_workspaces(
                    paths,
                    diff_paths,
                    diff_all,
                    open_behavior,
                    responses.as_ref(),
                    wait,
                    dev_container,
                    app_state.clone(),
                    env,
                    cwd,
                    cx,
                )
                .await;

                let status = if open_workspace_result.is_err() { 1 } else { 0 };
                responses.send(CliResponse::Exit { status }).log_err();
            }
        }
    }
}

pub(crate) fn open_options_for_request(
    open_behavior: Option<cli::OpenBehavior>,
    location: &SerializedWorkspaceLocation,
    cx: &App,
) -> workspace::OpenOptions {
    let open_behavior = open_behavior.unwrap_or(cli::OpenBehavior::ExistingWindow);
    open_options_for_behavior(open_behavior, location, cx)
}

pub(crate) fn open_options_for_behavior(
    open_behavior: cli::OpenBehavior,
    location: &SerializedWorkspaceLocation,
    cx: &App,
) -> workspace::OpenOptions {
    let open_behavior = if open_behavior == cli::OpenBehavior::Default {
        cli::OpenBehavior::ExistingWindow
    } else {
        open_behavior
    };

    // If reuse flag is passed, open a new workspace in an existing window.
    let requesting_window = if open_behavior == cli::OpenBehavior::Reuse {
        workspace::workspace_windows_for_location(location, cx)
            .into_iter()
            .next()
    } else {
        None
    };
    workspace::OpenOptions {
        workspace_matching: match open_behavior {
            cli::OpenBehavior::Reuse => workspace::WorkspaceMatching::None,
            cli::OpenBehavior::Add => workspace::WorkspaceMatching::MatchSubdirectory,
            _ => workspace::WorkspaceMatching::MatchExact,
        },
        add_dirs_to_sidebar: match open_behavior {
            cli::OpenBehavior::ExistingWindow => true,
            _ => false,
        },
        requesting_window,
        ..Default::default()
    }
}

async fn open_workspaces(
    paths: Vec<String>,
    diff_paths: Vec<[String; 2]>,
    diff_all: bool,
    open_behavior: cli::OpenBehavior,
    responses: &dyn CliResponseSink,
    wait: bool,
    dev_container: bool,
    app_state: Arc<AppState>,
    env: Option<collections::HashMap<String, String>>,
    cwd: Option<PathBuf>,
    cx: &mut AsyncApp,
) -> Result<()> {
    if paths.is_empty() && diff_paths.is_empty() {
        workspace::activate_any_workspace_window(cx)
            .context("mandatory Super Zed host window is unavailable")?;
        return Ok(());
    }

    if paths.is_empty() && diff_paths.is_empty() {
        let window = workspace::activate_any_workspace_window(cx)
            .context("mandatory Super Zed host window is unavailable")?;
        window.update(cx, |multi_workspace, window, cx| {
            multi_workspace.create_superzed_workspace(window, cx);
        })?;
        return Ok(());
    }

    let location = SerializedWorkspaceLocation::Remote(RemoteConnectionOptions::Local(
        remote::LocalConnectionOptions,
    ));
    let base_open_options = cx.update(|cx| open_options_for_behavior(open_behavior, &location, cx));
    let open_options = workspace::OpenOptions {
        wait,
        env,
        open_in_dev_container: dev_container,
        ..base_open_options
    };
    let errored = open_superzed_workspace(
        paths,
        diff_paths,
        diff_all,
        open_options,
        cwd,
        responses,
        &app_state,
        cx,
    )
    .await;
    anyhow::ensure!(!errored, "failed to open a workspace");

    Ok(())
}

async fn open_superzed_workspace(
    mut workspace_paths: Vec<String>,
    diff_paths: Vec<[String; 2]>,
    diff_all: bool,
    open_options: workspace::OpenOptions,
    cwd: Option<PathBuf>,
    responses: &dyn CliResponseSink,
    app_state: &Arc<AppState>,
    cx: &mut AsyncApp,
) -> bool {
    let user_provided_paths = !workspace_paths.is_empty();

    // When only diff paths are provided (no regular paths), add the CLI's
    // working directory so the workspace opens with the right context.
    // Note: must use the CLI process's cwd (forwarded via `cli_cwd`), not
    // `std::env::current_dir()`, since the Zed app process's cwd is typically
    // `/` on macOS bundles or the launch dir of an already-running instance.
    if !user_provided_paths
        && !diff_paths.is_empty()
        && let Some(cwd) = cwd
    {
        workspace_paths.push(cwd.to_string_lossy().to_string());
    }

    let paths_with_position =
        derive_paths_with_position(app_state.fs.as_ref(), workspace_paths).await;

    let (workspace, items) = match open_superzed_paths_with_positions(
        &paths_with_position,
        &diff_paths,
        diff_all,
        app_state.clone(),
        open_options.clone(),
        cx,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            let paths = paths_with_position
                .iter()
                .map(|p| p.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            log::error!("failed to open workspace [{paths}]: {error:#}");
            responses
                .send(CliResponse::Stderr {
                    message: format!("error opening [{paths}]: {error:#}"),
                })
                .log_err();
            return true;
        }
    };

    let mut errored = false;
    let mut item_release_futures = Vec::new();
    let mut subscriptions = Vec::new();
    // If --wait flag is used with no paths, or a directory, then wait until
    // the entire workspace is closed.
    if open_options.wait {
        let mut wait_for_window_close = paths_with_position.is_empty() && diff_paths.is_empty();
        if user_provided_paths {
            for path_with_position in &paths_with_position {
                if app_state.fs.is_dir(&path_with_position.path).await {
                    wait_for_window_close = true;
                    break;
                }
            }
        }

        if wait_for_window_close {
            let (release_tx, release_rx) = oneshot::channel();
            item_release_futures.push(release_rx);
            subscriptions.push(workspace.update(cx, |_, _, cx| {
                cx.on_release(move |_, _| {
                    let _ = release_tx.send(());
                })
            }));
        }
    }

    for item in items {
        match item {
            Some(Ok(item)) => {
                if open_options.wait {
                    let (release_tx, release_rx) = oneshot::channel();
                    item_release_futures.push(release_rx);
                    subscriptions.push(Ok(cx.update(|cx| {
                        item.on_release(
                            cx,
                            Box::new(move |_| {
                                release_tx.send(()).ok();
                            }),
                        )
                    })));
                }
            }
            Some(Err(err)) => {
                log::error!("{err:#}");
                responses
                    .send(CliResponse::Stderr {
                        message: format!("{err:#}"),
                    })
                    .log_err();
                errored = true;
            }
            None => {}
        }
    }

    if open_options.wait {
        let wait = async move {
            let _subscriptions = subscriptions;
            let _ = future::try_join_all(item_release_futures).await;
        }
        .fuse();
        futures::pin_mut!(wait);

        let background = cx.background_executor().clone();
        loop {
            // Repeatedly check if CLI is still open to avoid wasting resources
            // waiting for files or workspaces to close.
            let mut timer = background.timer(Duration::from_secs(1)).fuse();
            futures::select_biased! {
                _ = wait => break,
                _ = timer => {
                    if responses.send(CliResponse::Ping).is_err() {
                        break;
                    }
                }
            }
        }
    }

    errored
}

pub async fn derive_paths_with_position(
    fs: &dyn Fs,
    path_strings: impl IntoIterator<Item = impl AsRef<str>>,
) -> Vec<PathWithPosition> {
    let path_strings: Vec<_> = path_strings.into_iter().collect();
    let mut result = Vec::with_capacity(path_strings.len());
    for path_str in path_strings {
        let original_path = Path::new(path_str.as_ref());
        let mut parsed = PathWithPosition::parse_str(path_str.as_ref());

        // If the unparsed path string actually points to an existing file or directory, use it
        // instead of parsing out the line/col number. This matters for paths whose final
        // component looks like a position suffix, e.g. a folder named `Test (3)` would
        // otherwise be parsed as `Test ` at row 3.
        // Colon : is not valid in NTFS file names, so skip this logic if colon on windows.
        let has_colon = original_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| name.contains(':'));

        if (!has_colon || !cfg!(windows))
            && parsed.row.is_some()
            && parsed.path != original_path
            && (fs.is_file(original_path).await || fs.is_dir(original_path).await)
        {
            parsed = PathWithPosition::from_path(original_path.to_path_buf());
        }

        if let Ok(canonicalized) = fs.canonicalize(&parsed.path).await {
            parsed.path = canonicalized;
        }

        result.push(parsed);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zed::tests::init_test;
    use gpui::TestAppContext;
    use remote::SshConnectionOptions;
    use serde_json::json;
    use util::path;

    fn assert_ssh_parse(
        cx: &mut TestAppContext,
        input: &str,
        expected_url: Option<&str>,
        host: &str,
        username: Option<&str>,
        port: Option<u16>,
        path: &str,
    ) {
        if let Some(expected_url) = expected_url {
            assert_eq!(parse_ssh_url(input).unwrap().as_str(), expected_url);
        }

        let request = cx.update(|cx| {
            let rq = RawOpenRequest {
                urls: vec![input.into()],
                ..Default::default()
            };
            OpenRequest::parse(rq, cx).unwrap()
        });
        assert_eq!(
            request.remote_connection.unwrap(),
            RemoteConnectionOptions::Ssh(SshConnectionOptions {
                host: host.into(),
                username: username.map(str::to_string),
                port,
                ..Default::default()
            })
        );
        assert_eq!(request.open_paths, vec![path]);
    }

    #[gpui::test]
    fn test_parse_ssh_urls(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);
        let cases = [
            ("ssh://me@host:/", None, "host", Some("me"), None, "/"),
            (
                "ssh://me@host:~/code",
                None,
                "host",
                Some("me"),
                None,
                "/~/code",
            ),
            (
                "ssh://me@host:22/tmp",
                None,
                "host",
                Some("me"),
                Some(22),
                "/tmp",
            ),
            (
                "ssh://user@domain.tld@host:22/tmp",
                None,
                "host",
                Some("user@domain.tld"),
                Some(22),
                "/tmp",
            ),
            (
                "ssh://domain\\user@host/dir",
                Some("ssh://domain%5Cuser@host/dir"),
                "host",
                Some("domain\\user"),
                None,
                "/dir",
            ),
            (
                r"ssh://domain\\user@localhost/project",
                Some("ssh://domain%5C%5Cuser@localhost/project"),
                "localhost",
                Some(r"domain\\user"),
                None,
                "/project",
            ),
        ];

        for (input, expected_url, host, username, port, path) in cases {
            assert_ssh_parse(cx, input, expected_url, host, username, port, path);
        }
    }

    #[gpui::test]
    async fn test_derive_paths_with_position_directory_with_position_like_name(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let fs = app_state.fs.as_fake();

        // A folder whose name ends in `(N)` or `(row,col)` would otherwise be parsed as a
        // path with a row/column suffix (e.g. the MSVC-style `file.c(22)`), truncating the name.
        fs.insert_tree(
            path!("/root"),
            json!({
                "TEST (1)": {},
                "Project (2,3)": {},
                "test 123": {},
            }),
        )
        .await;

        let inputs = vec![
            path!("/root/TEST (1)").to_string(),
            path!("/root/Project (2,3)").to_string(),
            path!("/root/test 123").to_string(),
        ];
        let result = derive_paths_with_position(fs.as_ref(), inputs).await;

        let paths: Vec<_> = result
            .iter()
            .map(|p| (p.path.to_string_lossy().to_string(), p.row, p.column))
            .collect();
        assert_eq!(
            paths,
            vec![
                (path!("/root/TEST (1)").to_string(), None, None),
                (path!("/root/Project (2,3)").to_string(), None, None),
                (path!("/root/test 123").to_string(), None, None),
            ]
        );
    }

    // Test file with colon (`:`) in the name on non-Windows platforms,
    // as it is valid for file names on Unix-like systems.
    #[cfg(not(target_os = "windows"))]
    #[gpui::test]
    async fn test_derive_paths_with_position_colon_in_name_reverts_on_unix(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let fs = app_state.fs.as_fake();

        fs.insert_tree(path!("/root"), json!({ "test.txt:10": "" }))
            .await;

        let result =
            derive_paths_with_position(fs.as_ref(), vec![path!("/root/test.txt:10").to_string()])
                .await;

        let paths: Vec<_> = result
            .iter()
            .map(|p| (p.path.to_string_lossy().to_string(), p.row, p.column))
            .collect();
        assert_eq!(
            paths,
            vec![(path!("/root/test.txt:10").to_string(), None, None)]
        );
    }

    // On Windows `:` is used to delimit NTFS alternate data streams,
    // `notes.txt:10` should be parsed as `notes.txt` at row 10
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn test_derive_paths_with_position_colon_in_name_parsed_as_position_on_windows(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let fs = app_state.fs.as_fake();

        fs.insert_tree(path!("/root"), json!({ "test.txt": "" }))
            .await;

        let result =
            derive_paths_with_position(fs.as_ref(), vec![path!("/root/test.txt:10").to_string()])
                .await;

        let paths: Vec<_> = result
            .iter()
            .map(|p| (p.path.to_string_lossy().to_string(), p.row, p.column))
            .collect();
        assert_eq!(
            paths,
            vec![(path!("/root/test.txt").to_string(), Some(10), None)]
        );
    }

    #[gpui::test]
    fn test_reject_ssh_urls(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        for input in [
            "ssh://me@localhost:code/vibes/mine-bot",
            "ssh://me@localhost:2222:~/project",
            "ssh://me@[2001:db8::1]:~/project",
        ] {
            let result = cx.update(|cx| {
                OpenRequest::parse(
                    RawOpenRequest {
                        urls: vec![input.into()],
                        ..Default::default()
                    },
                    cx,
                )
            });
            assert!(result.is_err(), "{input} should be rejected");
        }
    }

    #[gpui::test]
    fn test_parse_agent_url(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec!["zed://agent".into()],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind {
            Some(OpenRequestKind::AgentPanel {
                external_source_prompt,
            }) => {
                assert_eq!(external_source_prompt, None);
            }
            _ => panic!("Expected AgentPanel kind"),
        }
    }

    #[gpui::test]
    fn test_parse_skill_install_url(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        let content =
            "---\nname: my-skill\ndescription: Does a thing.\n---\n\nDo the thing.\n".to_string();
        let link = agent_skills::encode_skill_share_link(&content);

        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec![link],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind {
            Some(OpenRequestKind::InstallSkill {
                content: parsed_content,
            }) => {
                assert_eq!(parsed_content, content);
            }
            _ => panic!("Expected InstallSkill kind"),
        }
    }

    #[gpui::test]
    fn test_parse_malformed_skill_install_url_errors(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        let result = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec!["zed://skill?data=!!!notbase64".into()],
                    ..Default::default()
                },
                cx,
            )
        });

        assert!(result.is_err());
    }

    fn agent_url_with_prompt(prompt: &str) -> String {
        let mut serializer = url::form_urlencoded::Serializer::new("zed://agent?".to_string());
        serializer.append_pair("prompt", prompt);
        serializer.finish()
    }

    #[gpui::test]
    fn test_parse_agent_url_with_prompt(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);
        let prompt = "Write me a script\nThanks";

        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec![agent_url_with_prompt(prompt)],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind {
            Some(OpenRequestKind::AgentPanel {
                external_source_prompt,
            }) => {
                assert_eq!(
                    external_source_prompt
                        .as_ref()
                        .map(ExternalSourcePrompt::as_str),
                    Some("Write me a script\nThanks")
                );
            }
            _ => panic!("Expected AgentPanel kind"),
        }
    }

    #[gpui::test]
    fn test_parse_agent_url_with_trailing_slash(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec!["zed://agent/?prompt=hello".into()],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind {
            Some(OpenRequestKind::AgentPanel {
                external_source_prompt,
            }) => {
                assert_eq!(
                    external_source_prompt
                        .as_ref()
                        .map(ExternalSourcePrompt::as_str),
                    Some("hello")
                );
            }
            _ => panic!("Expected AgentPanel kind"),
        }
    }

    #[gpui::test]
    fn test_parse_focus_app_url(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        for url in ["zed://", "zed://open", "zed://open/"] {
            let request = cx.update(|cx| {
                OpenRequest::parse(
                    RawOpenRequest {
                        urls: vec![url.into()],
                        ..Default::default()
                    },
                    cx,
                )
                .unwrap()
            });
            assert!(
                matches!(request.kind, Some(OpenRequestKind::FocusApp)),
                "expected FocusApp for {url}, got {:?}",
                request.kind
            );
            assert!(
                request.is_focus_app_only(),
                "expected is_focus_app_only for {url}"
            );
        }
    }

    #[gpui::test]
    fn test_parse_agent_url_with_empty_prompt(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec![agent_url_with_prompt("")],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind {
            Some(OpenRequestKind::AgentPanel {
                external_source_prompt,
            }) => {
                assert_eq!(external_source_prompt, None);
            }
            _ => panic!("Expected AgentPanel kind"),
        }
    }

    #[gpui::test]
    fn test_parse_git_commit_url(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        // Test basic git commit URL
        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec!["zed://git/commit/abc123?repo=path/to/repo".into()],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind.unwrap() {
            OpenRequestKind::GitCommit { sha } => {
                assert_eq!(sha, "abc123");
            }
            _ => panic!("expected GitCommit variant"),
        }
        // Verify path was added to open_paths for workspace routing
        assert_eq!(request.open_paths, vec!["path/to/repo"]);

        // Test with URL encoded path
        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec!["zed://git/commit/def456?repo=path%20with%20spaces".into()],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind.unwrap() {
            OpenRequestKind::GitCommit { sha } => {
                assert_eq!(sha, "def456");
            }
            _ => panic!("expected GitCommit variant"),
        }
        assert_eq!(request.open_paths, vec!["path with spaces"]);

        // Test with empty path
        cx.update(|cx| {
            assert!(
                OpenRequest::parse(
                    RawOpenRequest {
                        urls: vec!["zed://git/commit/abc123?repo=".into()],
                        ..Default::default()
                    },
                    cx,
                )
                .unwrap_err()
                .to_string()
                .contains("missing repo")
            );
        });

        // Test error case: missing SHA
        let result = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec!["zed://git/commit/abc123?foo=bar".into()],
                    ..Default::default()
                },
                cx,
            )
        });
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing repo query parameter")
        );
    }

    #[gpui::test]
    fn test_parse_git_clone_url(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec![
                        "zed://git/clone/?repo=https://github.com/zed-industries/zed.git".into(),
                    ],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind {
            Some(OpenRequestKind::GitClone { repo_url }) => {
                assert_eq!(repo_url, "https://github.com/zed-industries/zed.git");
            }
            _ => panic!("Expected GitClone kind"),
        }
    }

    #[gpui::test]
    fn test_parse_git_clone_url_without_slash(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec![
                        "zed://git/clone?repo=https://github.com/zed-industries/zed.git".into(),
                    ],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind {
            Some(OpenRequestKind::GitClone { repo_url }) => {
                assert_eq!(repo_url, "https://github.com/zed-industries/zed.git");
            }
            _ => panic!("Expected GitClone kind"),
        }
    }

    #[gpui::test]
    fn test_parse_git_clone_url_with_encoding(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec![
                        "zed://git/clone/?repo=https%3A%2F%2Fgithub.com%2Fzed-industries%2Fzed.git"
                            .into(),
                    ],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind {
            Some(OpenRequestKind::GitClone { repo_url }) => {
                assert_eq!(repo_url, "https://github.com/zed-industries/zed.git");
            }
            _ => panic!("Expected GitClone kind"),
        }
    }
}
