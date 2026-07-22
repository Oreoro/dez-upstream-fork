use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result, anyhow, bail};
use askpass::EncryptedPassword;
use futures::channel::oneshot;
use gpui::{AsyncApp, Task, TestAppContext};
use release_channel::ReleaseChannel;
use remote::{
    ConnectionIdentifier, Interactive, RemoteClient, RemoteClientDelegate, RemoteConnectionOptions,
    RemotePlatform, SshConnectionOptions,
};
use semver::Version;

struct NonInteractiveSshDelegate;

impl RemoteClientDelegate for NonInteractiveSshDelegate {
    fn ask_password(
        &self,
        _prompt: String,
        sender: oneshot::Sender<EncryptedPassword>,
        _cx: &mut AsyncApp,
    ) {
        if let Ok(password) = EncryptedPassword::try_from("") {
            sender.send(password).ok();
        }
    }

    fn get_download_url(
        &self,
        _platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        _version: Option<Version>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<Option<String>>> {
        Task::ready(Err(anyhow!(
            "the real SSH acceptance test builds the remote server from this checkout"
        )))
    }

    fn download_server_binary_locally(
        &self,
        _platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        _version: Option<Version>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<PathBuf>> {
        Task::ready(Err(anyhow!(
            "the real SSH acceptance test builds the remote server from this checkout"
        )))
    }

    fn set_status(&self, _status: Option<&str>, _cx: &mut AsyncApp) {}
}

async fn attach(
    connection_options: RemoteConnectionOptions,
    cx: &mut TestAppContext,
) -> Result<(
    gpui::Entity<RemoteClient>,
    superzed_session::HostSessionSnapshot,
)> {
    let delegate = Arc::new(NonInteractiveSshDelegate);
    let remote_connection = {
        let mut async_cx = cx.to_async();
        remote::connect(connection_options, delegate.clone(), &mut async_cx).await?
    };
    let (cancellation_guard, cancellation) = oneshot::channel();
    let connect = cx.update(|cx| {
        RemoteClient::new(
            ConnectionIdentifier::Host,
            remote_connection,
            cancellation,
            delegate,
            cx,
        )
    });
    let client = connect
        .await?
        .ok_or_else(|| anyhow!("real SSH host connection was cancelled"))?;
    drop(cancellation_guard);
    let response = client
        .read_with(cx, |client, _| {
            client
                .proto_client()
                .request(client::proto::GetSuperzedSession {})
        })
        .await?;
    let snapshot: superzed_session::HostSessionSnapshot =
        serde_json::from_str(&response.snapshot_json)?;
    snapshot.validate()?;
    Ok((client, snapshot))
}

async fn persistent_host_pid_files(
    client: &gpui::Entity<RemoteClient>,
    cx: &mut TestAppContext,
) -> Result<String> {
    let script = r#"
for root in "${XDG_DATA_HOME:-$HOME/.local/share}/superzed/server_state" "$HOME/Library/Application Support/SuperZed/server_state"; do
    [ -d "$root" ] || continue
    for file in "$root"/*superzed-*/server.pid; do
        [ -f "$file" ] || continue
        printf "%s " "$file"
        cat "$file"
        printf "\n"
    done
done | sort
"#;
    let command_template = client.read_with(cx, |client, _| {
        client.build_command(
            Some("sh".to_string()),
            &["-c".to_string(), script.to_string()],
            &HashMap::default(),
            None,
            None,
            Interactive::No,
        )
    })?;
    let mut command = util::command::new_command(&command_template.program);
    command.args(&command_template.args);
    command.envs(&command_template.env);
    let output = command
        .output()
        .await
        .context("read persistent Super Zed server PID over SSH")?;
    if !output.status.success() {
        bail!(
            "failed to read persistent Super Zed server PID over SSH: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let pid_files = String::from_utf8(output.stdout)
        .context("persistent Super Zed server PID output was not UTF-8")?;
    if pid_files.trim().is_empty() {
        bail!("the SSH host has no persistent Super Zed host server PID file");
    }
    Ok(pid_files)
}

#[gpui::test]
#[ignore = "requires SUPERZED_MILESTONE_1_SSH_TARGET"]
async fn real_ssh_host_detaches_and_restores_the_same_session(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let target = std::env::var("SUPERZED_MILESTONE_1_SSH_TARGET")
        .expect("SUPERZED_MILESTONE_1_SSH_TARGET must name a configured Unix SSH target");
    cx.update(|cx| release_channel::init(Version::new(0, 0, 0), cx));
    let mut options = SshConnectionOptions::parse_command_line(&target)
        .expect("parse SUPERZED_MILESTONE_1_SSH_TARGET");
    options.upload_binary_over_ssh = true;
    let connection_options = RemoteConnectionOptions::Ssh(options);

    let (first_client, first_snapshot) = attach(connection_options.clone(), cx)
        .await
        .expect("attach the real SSH host");
    let first_pid_files = persistent_host_pid_files(&first_client, cx)
        .await
        .expect("read the real SSH host server PID");
    drop(first_client);
    cx.run_until_parked();

    let (second_client, second_snapshot) = attach(connection_options, cx)
        .await
        .expect("reattach the real SSH host");
    let second_pid_files = persistent_host_pid_files(&second_client, cx)
        .await
        .expect("read the reattached SSH host server PID");
    assert_eq!(second_pid_files, first_pid_files);
    assert_eq!(second_snapshot, first_snapshot);
    drop(second_client);
}
