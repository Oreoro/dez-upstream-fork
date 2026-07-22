use std::{path::Path, sync::Arc, time::Duration};

use fs::FakeFs;
use futures::{FutureExt, StreamExt, pin_mut, select};
use gpui::TestAppContext;
use language::{CodeLabel, FakeLspAdapter, HighlightId, LanguageRegistry, rust_lang};
use lsp::Uri;
use project::{Project, host_resource_registry::HostResourceRegistry, lsp_store::*};
use serde_json::json;
use util::path;

use crate::init_test;

#[gpui::test]
async fn overlapping_projects_share_one_language_server_and_document_lifecycle(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/shared"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let languages = Arc::new(LanguageRegistry::test(cx.executor()));
    languages.add(rust_lang());
    let mut fake_servers = languages.register_fake_lsp("Rust", FakeLspAdapter::default());
    let host_resources = HostResourceRegistry::default();
    let first_project = Project::test_with_host_resources_and_languages(
        fs.clone(),
        [Path::new(path!("/shared"))],
        host_resources.clone(),
        languages.clone(),
        cx,
    )
    .await;
    let second_project = Project::test_with_host_resources_and_languages(
        fs,
        [Path::new(path!("/shared"))],
        host_resources.clone(),
        languages,
        cx,
    )
    .await;

    let (first_buffer, first_handle) = first_project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/shared/main.rs"), cx)
        })
        .await
        .expect("first project should open the shared Rust buffer");
    let mut fake_server = {
        let next_server = fake_servers.next().fuse();
        let timeout = cx.background_executor.timer(Duration::from_secs(2)).fuse();
        pin_mut!(next_server, timeout);
        select! {
            server = next_server => server.expect("the host should start one language server"),
            _ = timeout => panic!("the shared language server did not start"),
        }
    };
    {
        let did_open = fake_server
            .receive_notification::<lsp::notification::DidOpenTextDocument>()
            .fuse();
        let timeout = cx.background_executor.timer(Duration::from_secs(2)).fuse();
        pin_mut!(did_open, timeout);
        select! {
            _ = did_open => {},
            _ = timeout => panic!("the shared language server did not receive didOpen"),
        }
    }

    let (second_buffer, second_handle) = second_project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/shared/main.rs"), cx)
        })
        .await
        .expect("second project should open the shared Rust buffer");
    assert_eq!(first_buffer.entity_id(), second_buffer.entity_id());
    cx.run_until_parked();
    let (server_id, buffer_id) = first_project.read_with(cx, |project, cx| {
        (
            project
                .lsp_store()
                .read(cx)
                .language_server_statuses()
                .next()
                .expect("the shared language server should be running")
                .0,
            first_buffer.read(cx).remote_id(),
        )
    });
    assert_eq!(
        first_project.read_with(cx, |project, cx| {
            project
                .lsp_store()
                .read(cx)
                .host_document_user_count_for_test(server_id, buffer_id)
        }),
        2
    );
    assert_eq!(
        first_project.read_with(cx, |project, cx| {
            project
                .lsp_store()
                .read(cx)
                .host_language_server_user_count_for_test(server_id)
        }),
        2
    );

    let second_server = fake_servers.next().fuse();
    let timeout = cx
        .background_executor
        .timer(Duration::from_millis(20))
        .fuse();
    pin_mut!(second_server, timeout);
    select! {
        _ = second_server => panic!("overlapping projects started a second language server"),
        _ = timeout => {}
    }

    cx.update(|_| drop(first_handle));
    cx.run_until_parked();
    assert_eq!(
        second_project.read_with(cx, |project, cx| {
            project
                .lsp_store()
                .read(cx)
                .host_document_user_count_for_test(server_id, buffer_id)
        }),
        1
    );
    {
        let premature_close = fake_server
            .receive_notification::<lsp::notification::DidCloseTextDocument>()
            .fuse();
        let timeout = cx
            .background_executor
            .timer(Duration::from_millis(20))
            .fuse();
        pin_mut!(premature_close, timeout);
        select! {
            _ = premature_close => panic!("the first project close sent didClose while the document was still shared"),
            _ = timeout => {}
        }
    }

    cx.update(|_| drop(second_handle));
    cx.run_until_parked();
    assert_eq!(
        second_project.read_with(cx, |project, cx| {
            project
                .lsp_store()
                .read(cx)
                .host_document_user_count_for_test(server_id, buffer_id)
        }),
        0
    );
    let did_close = fake_server
        .receive_notification::<lsp::notification::DidCloseTextDocument>()
        .fuse();
    let timeout = cx.background_executor.timer(Duration::from_secs(2)).fuse();
    pin_mut!(did_close, timeout);
    select! {
        _ = did_close => {},
        _ = timeout => panic!("the final project close did not send didClose"),
    }

    cx.update(|_| drop(first_project));
    cx.run_until_parked();
    assert_eq!(
        second_project.read_with(cx, |project, cx| {
            project
                .lsp_store()
                .read(cx)
                .host_language_server_user_count_for_test(server_id)
        }),
        1
    );
    cx.update(|_| drop(second_project));
    cx.run_until_parked();
    assert_eq!(host_resources.lsp_user_count_for_test(server_id), 0);
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
