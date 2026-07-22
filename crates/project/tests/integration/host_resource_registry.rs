use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Result, anyhow};
use context_server::{ContextServer, ContextServerId, test::create_fake_transport};
use fs::FakeFs;
use gpui::{AppContext as _, AsyncApp, Entity, Task, TestAppContext};
use project::{
    Project, ProjectPath,
    buffer_store::BufferStore,
    context_server_store::registry::{ContextServerDescriptor, ContextServerDescriptorRegistry},
    host_resource_registry::HostResourceRegistry,
    worktree_store::{WorktreeIdCounter, WorktreeStore},
};
use serde_json::json;
use settings::ContextServerCommand;
use util::rel_path::rel_path;

use super::init_test;

struct MissingCredentialContextServerDescriptor {
    resolution_count: Arc<AtomicUsize>,
}

struct SuccessfulContextServerDescriptor;

impl ContextServerDescriptor for SuccessfulContextServerDescriptor {
    fn command(
        &self,
        _project_context: Option<Entity<WorktreeStore>>,
        _cx: &AsyncApp,
    ) -> Task<Result<ContextServerCommand>> {
        Task::ready(Ok(ContextServerCommand {
            path: "shared-context-server".into(),
            args: Vec::new(),
            env: None,
            timeout: None,
        }))
    }

    fn configuration(
        &self,
        _project_context: Option<Entity<WorktreeStore>>,
        _cx: &AsyncApp,
    ) -> Task<Result<Option<extension::ContextServerConfiguration>>> {
        Task::ready(Ok(None))
    }
}

impl ContextServerDescriptor for MissingCredentialContextServerDescriptor {
    fn command(
        &self,
        _project_context: Option<Entity<WorktreeStore>>,
        _cx: &AsyncApp,
    ) -> Task<Result<ContextServerCommand>> {
        self.resolution_count.fetch_add(1, Ordering::SeqCst);
        Task::ready(Err(anyhow!("missing field `github_personal_access_token`")))
    }

    fn configuration(
        &self,
        _project_context: Option<Entity<WorktreeStore>>,
        _cx: &AsyncApp,
    ) -> Task<Result<Option<extension::ContextServerConfiguration>>> {
        Task::ready(Ok(None))
    }
}

#[gpui::test]
async fn overlapping_projects_share_worktrees_and_buffers(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        Path::new("/shared"),
        json!({
            "main.rs": "fn main() {}"
        }),
    )
    .await;

    let host_resources = HostResourceRegistry::default();
    let (first_worktree_store, second_worktree_store) = cx.update(|cx| {
        let first_worktree_store = cx.new(|cx| {
            WorktreeStore::local_with_host_resources(
                false,
                fs.clone(),
                WorktreeIdCounter::get(cx),
                host_resources.clone(),
            )
        });
        let second_worktree_store = cx.new(|cx| {
            WorktreeStore::local_with_host_resources(
                false,
                fs.clone(),
                WorktreeIdCounter::get(cx),
                host_resources,
            )
        });
        (first_worktree_store, second_worktree_store)
    });

    let first_worktree = first_worktree_store
        .update(cx, |store, cx| store.create_worktree("/shared", true, cx))
        .await
        .expect("first worktree should open");
    let second_worktree = second_worktree_store
        .update(cx, |store, cx| store.create_worktree("/shared", true, cx))
        .await
        .expect("second worktree should open");
    assert_eq!(first_worktree.entity_id(), second_worktree.entity_id());

    let (first_buffer_store, second_buffer_store) = cx.update(|cx| {
        (
            cx.new(|cx| BufferStore::local(first_worktree_store, cx)),
            cx.new(|cx| BufferStore::local(second_worktree_store, cx)),
        )
    });
    let project_path = ProjectPath {
        worktree_id: first_worktree.read_with(cx, |worktree, _| worktree.id()),
        path: rel_path("main.rs").into(),
    };
    let first_buffer = first_buffer_store
        .update(cx, |store, cx| store.open_buffer(project_path.clone(), cx))
        .await
        .expect("first buffer should open");
    let second_buffer = second_buffer_store
        .update(cx, |store, cx| store.open_buffer(project_path, cx))
        .await
        .expect("second buffer should open");
    assert_eq!(first_buffer.entity_id(), second_buffer.entity_id());
}

#[gpui::test]
async fn overlapping_project_scopes_share_runtime_resources_but_not_roots(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        Path::new("/code"),
        json!({
            "shared": {
                ".git": {},
                "main.rs": "fn main() {}"
            },
            "second": {
                "other.rs": "pub fn other() {}"
            }
        }),
    )
    .await;
    fs.set_index_for_repo(
        Path::new("/code/shared/.git"),
        &[("main.rs", "fn main() {}".into())],
    );

    let host_resources = HostResourceRegistry::default();
    let first_project = Project::test_with_host_resources(
        fs.clone(),
        [Path::new("/code/shared")],
        host_resources.clone(),
        cx,
    )
    .await;
    let second_project = Project::test_with_host_resources(
        fs,
        [Path::new("/code/shared"), Path::new("/code/second")],
        host_resources.clone(),
        cx,
    )
    .await;
    cx.run_until_parked();

    let first_worktree = first_project
        .read_with(cx, |project, cx| project.worktrees(cx).next())
        .expect("first project should expose its shared root");
    let second_worktree = second_project
        .read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .find(|worktree| worktree.read(cx).abs_path().as_ref() == Path::new("/code/shared"))
        })
        .expect("second project should expose its shared root");
    assert_eq!(first_worktree.entity_id(), second_worktree.entity_id());
    assert_eq!(
        first_project.read_with(cx, |project, cx| project.worktrees(cx).count()),
        1
    );
    assert_eq!(
        second_project.read_with(cx, |project, cx| project.worktrees(cx).count()),
        2
    );

    let worktree_id = first_worktree.read_with(cx, |worktree, _| worktree.id());
    let first_buffer = first_project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("main.rs")), cx)
        })
        .await
        .expect("first project should open the shared buffer");
    let second_buffer = second_project
        .update(cx, |project, cx| {
            project.open_buffer((worktree_id, rel_path("main.rs")), cx)
        })
        .await
        .expect("second project should open the shared buffer");
    assert_eq!(first_buffer.entity_id(), second_buffer.entity_id());
    first_buffer.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "// shared\n")], None, cx)
    });
    assert!(
        second_buffer
            .read_with(cx, |buffer, _| buffer.text())
            .starts_with("// shared")
    );

    cx.run_until_parked();
    let first_repository = first_project
        .read_with(cx, |project, cx| {
            project
                .git_store()
                .read(cx)
                .repositories()
                .values()
                .next()
                .cloned()
        })
        .expect("first project should discover the Git repository");
    let second_repository = second_project
        .read_with(cx, |project, cx| {
            project
                .git_store()
                .read(cx)
                .repositories()
                .values()
                .next()
                .cloned()
        })
        .expect("second project should discover the Git repository");
    assert_eq!(first_repository.entity_id(), second_repository.entity_id());
    assert_eq!(host_resources.resource_user_counts_for_test(), (3, 2, 2));

    cx.update(|_| drop(first_project));
    cx.run_until_parked();
    assert_eq!(host_resources.resource_user_counts_for_test(), (2, 1, 1));

    cx.update(|_| drop(second_project));
    cx.run_until_parked();
    assert_eq!(host_resources.resource_user_counts_for_test(), (0, 0, 0));
}

#[gpui::test]
async fn workspace_project_scopes_share_host_context_server_initialization(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        Path::new("/code"),
        json!({
            "first": { "first.rs": "pub fn first() {}" },
            "second": { "second.rs": "pub fn second() {}" }
        }),
    )
    .await;

    let host_resources = HostResourceRegistry::default();
    let first_project = Project::test_with_host_resources(
        fs.clone(),
        [Path::new("/code/first")],
        host_resources.clone(),
        cx,
    )
    .await;
    let second_project =
        Project::test_with_host_resources(fs, [Path::new("/code/second")], host_resources, cx)
            .await;
    cx.run_until_parked();

    let first_roots = first_project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect::<Vec<PathBuf>>()
    });
    let second_roots = second_project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect::<Vec<PathBuf>>()
    });
    assert_eq!(first_roots, [PathBuf::from("/code/first")]);
    assert_eq!(second_roots, [PathBuf::from("/code/second")]);

    let resolution_count = Arc::new(AtomicUsize::new(0));
    cx.update(|cx| {
        let registry = ContextServerDescriptorRegistry::default_global(cx);
        registry.update(cx, |registry, cx| {
            registry.register_context_server_descriptor(
                "github-mcp".into(),
                Arc::new(MissingCredentialContextServerDescriptor {
                    resolution_count: resolution_count.clone(),
                }),
                cx,
            );
        });
    });
    cx.run_until_parked();

    assert_eq!(
        resolution_count.load(Ordering::SeqCst),
        1,
        "one host-owned context-server definition must resolve global credentials once, regardless of workspace project count"
    );
}

#[gpui::test]
async fn workspace_project_scopes_share_compatible_host_context_server_runtime(
    cx: &mut TestAppContext,
) {
    const GLOBAL_SERVER_ID: &str = "global-shared-mcp";
    const SCOPED_SERVER_ID: &str = "project-scoped-mcp";

    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        Path::new("/code"),
        json!({
            "first": {
                ".zed": {
                    "settings.json": serde_json::to_string(&json!({
                        "context_servers": {
                            (SCOPED_SERVER_ID): {
                                "command": "first-project-server",
                                "args": [],
                                "enabled": false
                            }
                        }
                    })).expect("serialize first project settings")
                },
                "first.rs": "pub fn first() {}"
            },
            "second": {
                ".zed": {
                    "settings.json": serde_json::to_string(&json!({
                        "context_servers": {
                            (SCOPED_SERVER_ID): {
                                "command": "second-project-server",
                                "args": [],
                                "enabled": false
                            }
                        }
                    })).expect("serialize second project settings")
                },
                "second.rs": "pub fn second() {}"
            }
        }),
    )
    .await;

    let host_resources = HostResourceRegistry::default();
    let first_project = Project::test_with_host_resources(
        fs.clone(),
        [Path::new("/code/first")],
        host_resources.clone(),
        cx,
    )
    .await;
    let second_project =
        Project::test_with_host_resources(fs, [Path::new("/code/second")], host_resources, cx)
            .await;
    cx.run_until_parked();

    let first_store = first_project.read_with(cx, |project, _| project.context_server_store());
    let second_store = second_project.read_with(cx, |project, _| project.context_server_store());
    let scoped_id = ContextServerId(SCOPED_SERVER_ID.into());
    let first_scoped_settings = first_store.read_with(cx, |store, _| {
        store.settings_for_server(&scoped_id).cloned()
    });
    let second_scoped_settings = second_store.read_with(cx, |store, _| {
        store.settings_for_server(&scoped_id).cloned()
    });
    assert_ne!(first_scoped_settings, second_scoped_settings);

    let runtime_creation_count = Arc::new(AtomicUsize::new(0));
    for store in [&first_store, &second_store] {
        let executor = cx.executor();
        let runtime_creation_count = runtime_creation_count.clone();
        store.update(cx, |store, _| {
            store.set_context_server_factory(Box::new(move |id, _| {
                runtime_creation_count.fetch_add(1, Ordering::SeqCst);
                Arc::new(ContextServer::new(
                    id.clone(),
                    Arc::new(create_fake_transport(id.0.to_string(), executor.clone())),
                ))
            }));
        });
    }

    cx.update(|cx| {
        ContextServerDescriptorRegistry::default_global(cx).update(cx, |registry, cx| {
            registry.register_context_server_descriptor(
                GLOBAL_SERVER_ID.into(),
                Arc::new(SuccessfulContextServerDescriptor),
                cx,
            );
        });
    });
    cx.run_until_parked();

    let global_id = ContextServerId(GLOBAL_SERVER_ID.into());
    let first_runtime = first_store
        .read_with(cx, |store, _| store.get_server(&global_id))
        .expect("first project should expose the host context server");
    let second_runtime = second_store
        .read_with(cx, |store, _| store.get_server(&global_id))
        .expect("second project should expose the host context server");
    assert_eq!(
        runtime_creation_count.load(Ordering::SeqCst),
        1,
        "one host must create one compatible context-server runtime"
    );
    assert!(
        Arc::ptr_eq(&first_runtime, &second_runtime),
        "project facades on one host must expose the same context-server runtime"
    );

    drop(first_runtime);
    drop(first_store);
    drop(first_project);
    cx.run_until_parked();
    assert!(
        second_runtime.client().is_some(),
        "dropping one project facade must not stop a runtime still used by another"
    );
}
