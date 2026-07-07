use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use collections::HashSet;
use fs::FakeFs;
use futures::StreamExt;
use gpui::{Entity, TestAppContext};
use language::{
    CodeLabel, FakeLspAdapter, HighlightId, Language, LanguageConfig, LanguageMatcher, rust_lang,
};
use lsp::Uri;
use project::{Project, ProjectPath, lsp_store::*};
use serde_json::json;
use util::{path, rel_path::rel_path};

use crate::init_test;

#[gpui::test]
async fn test_language_server_demand_tracks_open_buffers(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({
            "a.rs": "fn a() {}",
            "b.rs": "fn b() {}",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());
    assert_no_language_servers_running(&project, cx);

    let (_buffer_a, handle_a) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    let server_id = LanguageServerId(0);
    assert_language_server_demand_count(&project, server_id, 1, cx);
    assert_no_idle_shutdown_task(&project, server_id, cx);

    let (_buffer_b, handle_b) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/b.rs"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    assert_language_server_demand_count(&project, server_id, 2, cx);
    assert_no_idle_shutdown_task(&project, server_id, cx);

    drop(handle_a);
    cx.run_until_parked();

    assert_language_server_demand_count(&project, server_id, 1, cx);
    assert_no_idle_shutdown_task(&project, server_id, cx);

    drop(handle_b);
    cx.run_until_parked();

    assert_language_server_demand_count(&project, server_id, 0, cx);
    assert_idle_shutdown_task(&project, server_id, cx);
    assert_language_server_running(&project, server_id, cx);

    let (_buffer_a, handle_a) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    assert_language_server_demand_count(&project, server_id, 1, cx);
    assert_no_idle_shutdown_task(&project, server_id, cx);
    assert_language_server_running(&project, server_id, cx);

    drop(handle_a);
    cx.run_until_parked();

    assert_idle_shutdown_task(&project, server_id, cx);
    cx.executor()
        .advance_clock(TEST_IDLE_LANGUAGE_SERVER_SHUTDOWN_TIMEOUT + Duration::from_secs(1));
    cx.run_until_parked();

    assert_language_server_stopped(&project, server_id, cx);
}

#[gpui::test]
async fn test_last_language_server_demand_cancels_disk_diagnostics(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let progress_token = "the-progress-token";
    let disk_diagnostics_cancellations = Arc::new(AtomicUsize::new(0));

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({
            "a.rs": "fn a() {}",
            "b.rs": "fn b() {}",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            disk_based_diagnostics_progress_token: Some(progress_token.into()),
            disk_based_diagnostics_cancellations: Some(disk_diagnostics_cancellations.clone()),
            ..Default::default()
        },
    );

    let (_buffer_a, handle_a) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    let (_buffer_b, handle_b) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/b.rs"), cx)
        })
        .await
        .unwrap();
    let fake_server = fake_servers.next().await.unwrap();
    cx.run_until_parked();

    fake_server.start_progress(progress_token).await;
    cx.run_until_parked();
    assert_language_server_demand_count(&project, LanguageServerId(0), 2, cx);
    assert_language_servers_running_disk_based_diagnostics(&project, [LanguageServerId(0)], cx);

    drop(handle_a);
    cx.run_until_parked();

    assert_language_server_demand_count(&project, LanguageServerId(0), 1, cx);
    assert_no_idle_shutdown_task(&project, LanguageServerId(0), cx);
    assert_eq!(disk_diagnostics_cancellations.load(Ordering::SeqCst), 0);
    assert_language_servers_running_disk_based_diagnostics(&project, [LanguageServerId(0)], cx);

    drop(handle_b);
    cx.run_until_parked();

    assert_language_server_demand_count(&project, LanguageServerId(0), 0, cx);
    assert_idle_shutdown_task(&project, LanguageServerId(0), cx);
    assert_eq!(disk_diagnostics_cancellations.load(Ordering::SeqCst), 1);
    assert_language_servers_running_disk_based_diagnostics(&project, [], cx);
}

#[gpui::test]
async fn test_language_change_moves_language_server_demand(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/dir"), json!({ "a.rs": "fn a() {}" }))
        .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    let javascript = js_lang();
    language_registry.add(rust_lang());
    language_registry.add(javascript.clone());
    let mut fake_rust_servers =
        language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());
    let mut fake_javascript_servers =
        language_registry.register_fake_lsp("JavaScript", FakeLspAdapter::default());

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    fake_rust_servers.next().await.unwrap();
    cx.run_until_parked();

    assert_language_server_demand_count(&project, LanguageServerId(0), 1, cx);

    project.update(cx, |project, cx| {
        project.set_language_for_buffer(&buffer, javascript, cx);
    });
    fake_javascript_servers.next().await.unwrap();
    cx.run_until_parked();

    assert_language_server_demand_count(&project, LanguageServerId(0), 0, cx);
    assert_idle_shutdown_task(&project, LanguageServerId(0), cx);
    assert_language_server_demand_count(&project, LanguageServerId(1), 1, cx);
    assert_no_idle_shutdown_task(&project, LanguageServerId(1), cx);
}

#[gpui::test]
async fn test_file_path_change_moves_language_server_demand(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/dir"), json!({ "a.rs": "fn a() {}" }))
        .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    language_registry.add(js_lang());
    let mut fake_rust_servers =
        language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());
    let mut fake_javascript_servers =
        language_registry.register_fake_lsp("JavaScript", FakeLspAdapter::default());

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    fake_rust_servers.next().await.unwrap();
    cx.run_until_parked();

    assert_language_server_demand_count(&project, LanguageServerId(0), 1, cx);

    project
        .update(cx, |project, cx| {
            let worktree_id = project.worktrees(cx).next().unwrap().read(cx).id();
            project.save_buffer_as(
                buffer.clone(),
                ProjectPath {
                    worktree_id,
                    path: rel_path("a.js").into(),
                },
                cx,
            )
        })
        .await
        .unwrap();
    fake_javascript_servers.next().await.unwrap();
    cx.run_until_parked();

    assert_language_server_demand_count(&project, LanguageServerId(0), 0, cx);
    assert_idle_shutdown_task(&project, LanguageServerId(0), cx);
    assert_language_server_demand_count(&project, LanguageServerId(1), 1, cx);
    assert_no_idle_shutdown_task(&project, LanguageServerId(1), cx);
}

#[gpui::test]
async fn test_worktree_removal_cleans_language_server_demand(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/dir"), json!({ "a.rs": "fn a() {}" }))
        .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());

    let (_buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    assert_language_server_demand_count(&project, LanguageServerId(0), 1, cx);

    let worktree_id = project.read_with(cx, |project, cx| {
        project.worktrees(cx).next().unwrap().read(cx).id()
    });
    project.update(cx, |project, cx| {
        project.remove_worktree(worktree_id, cx);
    });
    cx.run_until_parked();

    assert_language_server_demand_count(&project, LanguageServerId(0), 0, cx);
    assert_no_idle_shutdown_task(&project, LanguageServerId(0), cx);
    assert_language_server_stopped(&project, LanguageServerId(0), cx);
}

#[gpui::test]
async fn test_manual_stop_and_restart_do_not_leave_stale_language_server_demand(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/dir"), json!({ "a.rs": "fn a() {}" }))
        .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    assert_language_server_demand_count(&project, LanguageServerId(0), 1, cx);

    project
        .update(cx, |project, cx| {
            project.stop_language_servers_for_buffers(vec![buffer.clone()], HashSet::default(), cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();

    assert_language_server_demand_count(&project, LanguageServerId(0), 0, cx);
    assert_no_idle_shutdown_task(&project, LanguageServerId(0), cx);
    assert_language_server_stopped(&project, LanguageServerId(0), cx);

    project.update(cx, |project, cx| {
        project.restart_language_servers_for_buffers(vec![buffer], HashSet::default(), true, cx);
    });
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    assert_language_server_demand_count(&project, LanguageServerId(1), 1, cx);
    assert_no_idle_shutdown_task(&project, LanguageServerId(1), cx);
}

#[gpui::test]
async fn test_removing_invisible_worktree_cleans_reused_lsp_bookkeeping(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/the-root"), json!({ "main.rs": "fn main() {}" }))
        .await;
    fs.insert_tree(
        path!("/the-registry"),
        json!({ "dep": { "src": { "dep.rs": "pub fn dep() {}" } } }),
    )
    .await;

    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());

    let (_visible_buffer, _visible_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/main.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    let server_id = project.read_with(cx, |project, cx| {
        project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .next()
            .unwrap()
            .0
    });
    let external_buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(
                Uri::from_file_path(path!("/the-registry/dep/src/dep.rs")).unwrap(),
                server_id,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let invisible_worktree_id =
        external_buffer.read_with(cx, |buffer, cx| buffer.file().unwrap().worktree_id(cx));
    project.read_with(cx, |project, cx| {
        let worktree = project.worktree_for_id(invisible_worktree_id, cx).unwrap();
        assert!(!worktree.read(cx).is_visible());
        assert!(
            project
                .lsp_store()
                .read(cx)
                .has_language_server_seed_for_worktree(invisible_worktree_id)
        );
    });

    project.update(cx, |project, cx| {
        project.remove_worktree(invisible_worktree_id, cx);
    });
    cx.run_until_parked();

    project.read_with(cx, |project, cx| {
        let lsp_store = project.lsp_store();
        let lsp_store = lsp_store.read(cx);
        assert!(
            lsp_store
                .language_server_statuses()
                .any(|(status_server_id, _)| status_server_id == server_id)
        );
        assert!(!lsp_store.has_language_server_seed_for_worktree(invisible_worktree_id));
    });
}

#[test]
fn test_glob_literal_prefix() {
    assert_eq!(glob_literal_prefix(Path::new("**/*.js")), Path::new(""));
    assert_eq!(
        glob_literal_prefix(Path::new("node_modules/**/*.js")),
        Path::new("node_modules")
    );
    assert_eq!(
        glob_literal_prefix(Path::new("foo/{bar,baz}.js")),
        Path::new("foo")
    );
    assert_eq!(
        glob_literal_prefix(Path::new("foo/bar/baz.js")),
        Path::new("foo/bar/baz.js")
    );

    #[cfg(target_os = "windows")]
    {
        assert_eq!(glob_literal_prefix(Path::new("**\\*.js")), Path::new(""));
        assert_eq!(
            glob_literal_prefix(Path::new("node_modules\\**/*.js")),
            Path::new("node_modules")
        );
        assert_eq!(
            glob_literal_prefix(Path::new("foo/{bar,baz}.js")),
            Path::new("foo")
        );
        assert_eq!(
            glob_literal_prefix(Path::new("foo\\bar\\baz.js")),
            Path::new("foo/bar/baz.js")
        );
    }
}

#[test]
fn test_multi_len_chars_normalization() {
    let mut label = CodeLabel::new(
        "myElˇ (parameter) myElˇ: {\n    foo: string;\n}".to_string(),
        0..6,
        vec![(0..6, HighlightId::new(1))],
    );
    ensure_uniform_list_compatible_label(&mut label);
    assert_eq!(
        label,
        CodeLabel::new(
            "myElˇ (parameter) myElˇ: { foo: string; }".to_string(),
            0..6,
            vec![(0..6, HighlightId::new(1))],
        )
    );
}

#[test]
fn test_trailing_newline_in_completion_documentation() {
    let doc =
        lsp::Documentation::String("Inappropriate argument value (of correct type).\n".to_string());
    let completion_doc: CompletionDocumentation = doc.into();
    assert!(
        matches!(completion_doc, CompletionDocumentation::SingleLine(s) if s == "Inappropriate argument value (of correct type).")
    );

    let doc = lsp::Documentation::String("  some value  \n".to_string());
    let completion_doc: CompletionDocumentation = doc.into();
    assert!(matches!(
        completion_doc,
        CompletionDocumentation::SingleLine(s) if s == "some value"
    ));
}

fn assert_language_server_demand_count(
    project: &Entity<Project>,
    server_id: LanguageServerId,
    expected_count: usize,
    cx: &TestAppContext,
) {
    project.read_with(cx, |project, cx| {
        assert_eq!(
            project
                .lsp_store()
                .read(cx)
                .language_server_demand_count(server_id),
            expected_count
        );
    });
}

fn assert_idle_shutdown_task(
    project: &Entity<Project>,
    server_id: LanguageServerId,
    cx: &TestAppContext,
) {
    project.read_with(cx, |project, cx| {
        assert!(
            project
                .lsp_store()
                .read(cx)
                .has_idle_language_server_shutdown_task(server_id)
        );
    });
}

fn assert_no_idle_shutdown_task(
    project: &Entity<Project>,
    server_id: LanguageServerId,
    cx: &TestAppContext,
) {
    project.read_with(cx, |project, cx| {
        assert!(
            !project
                .lsp_store()
                .read(cx)
                .has_idle_language_server_shutdown_task(server_id)
        );
    });
}

fn assert_language_server_running(
    project: &Entity<Project>,
    server_id: LanguageServerId,
    cx: &TestAppContext,
) {
    project.read_with(cx, |project, cx| {
        assert!(
            project
                .language_server_statuses(cx)
                .any(|(status_server_id, _)| status_server_id == server_id)
        );
    });
}

fn assert_no_language_servers_running(project: &Entity<Project>, cx: &TestAppContext) {
    project.read_with(cx, |project, cx| {
        assert!(project.language_server_statuses(cx).next().is_none());
    });
}

fn assert_language_server_stopped(
    project: &Entity<Project>,
    server_id: LanguageServerId,
    cx: &TestAppContext,
) {
    project.read_with(cx, |project, cx| {
        assert!(
            !project
                .language_server_statuses(cx)
                .any(|(status_server_id, _)| status_server_id == server_id)
        );
    });
}

fn assert_language_servers_running_disk_based_diagnostics<const N: usize>(
    project: &Entity<Project>,
    expected_server_ids: [LanguageServerId; N],
    cx: &mut TestAppContext,
) {
    project.update(cx, |project, cx| {
        assert_eq!(
            project
                .language_servers_running_disk_based_diagnostics(cx)
                .collect::<Vec<_>>(),
            expected_server_ids
        );
    });
}

fn js_lang() -> Arc<Language> {
    Arc::new(Language::new(
        LanguageConfig {
            name: "JavaScript".into(),
            matcher: LanguageMatcher {
                path_suffixes: vec!["js".to_string()],
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    ))
}
