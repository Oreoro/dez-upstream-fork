use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Result;
use collections::HashMap;
use futures::{FutureExt, future::Shared};
use gpui::{Entity, Task};
use language::Buffer;
use parking_lot::Mutex;
use util::{paths::SanitizedPath, rel_path::RelPath};
use worktree::{Worktree, WorktreeId};

use crate::{
    context_server_store::HostContextServerRegistry,
    git_store::{Repository, RepositoryId},
    lsp_store::HostLspPool,
};

type SharedWorktree = Shared<Task<Result<Entity<Worktree>, Arc<anyhow::Error>>>>;
type SharedBuffer = Shared<Task<Result<Entity<Buffer>, Arc<anyhow::Error>>>>;

struct SharedResource<T> {
    value: T,
    users: usize,
}

#[derive(Clone)]
pub struct HostResourceRegistry {
    worktrees: Arc<Mutex<HashMap<Arc<SanitizedPath>, SharedResource<SharedWorktree>>>>,
    buffers: Arc<Mutex<HashMap<(WorktreeId, Arc<RelPath>), SharedResource<SharedBuffer>>>>,
    repositories: Arc<Mutex<HashMap<PathBuf, SharedResource<Entity<Repository>>>>>,
    lsp_pool: HostLspPool,
    context_servers: HostContextServerRegistry,
    next_repository_id: Arc<AtomicU64>,
}

impl Default for HostResourceRegistry {
    fn default() -> Self {
        Self {
            worktrees: Default::default(),
            buffers: Default::default(),
            repositories: Default::default(),
            lsp_pool: Default::default(),
            context_servers: Default::default(),
            next_repository_id: Arc::new(AtomicU64::new(1)),
        }
    }
}

impl HostResourceRegistry {
    #[cfg(feature = "test-support")]
    pub fn without_shared_context_server_runtimes_for_test() -> Self {
        Self {
            context_servers: HostContextServerRegistry::without_shared_runtimes_for_test(),
            ..Default::default()
        }
    }

    pub fn context_servers(&self) -> HostContextServerRegistry {
        self.context_servers.clone()
    }

    pub fn acquire_worktree(
        &self,
        path: Arc<SanitizedPath>,
        create: impl FnOnce() -> Task<Result<Entity<Worktree>, Arc<anyhow::Error>>>,
    ) -> SharedWorktree {
        let mut worktrees = self.worktrees.lock();
        worktrees
            .entry(path)
            .and_modify(|resource| resource.users += 1)
            .or_insert_with(|| SharedResource {
                value: create().shared(),
                users: 1,
            })
            .value
            .clone()
    }

    pub fn release_worktree(&self, path: &SanitizedPath) {
        release_resource(&mut self.worktrees.lock(), path);
    }

    pub fn acquire_buffer(
        &self,
        worktree_id: WorktreeId,
        path: Arc<RelPath>,
        create: impl FnOnce() -> Task<Result<Entity<Buffer>, Arc<anyhow::Error>>>,
    ) -> SharedBuffer {
        let mut buffers = self.buffers.lock();
        buffers
            .entry((worktree_id, path))
            .and_modify(|resource| resource.users += 1)
            .or_insert_with(|| SharedResource {
                value: create().shared(),
                users: 1,
            })
            .value
            .clone()
    }

    pub fn release_buffer(&self, worktree_id: WorktreeId, path: &RelPath) {
        release_resource(&mut self.buffers.lock(), &(worktree_id, path.into()));
    }

    pub fn repository(&self, common_directory: &Path) -> Option<Entity<Repository>> {
        let mut repositories = self.repositories.lock();
        let repository = repositories.get_mut(common_directory)?;
        repository.users += 1;
        Some(repository.value.clone())
    }

    pub fn insert_repository(
        &self,
        common_directory: &Path,
        repository: Entity<Repository>,
    ) -> Entity<Repository> {
        self.repositories
            .lock()
            .entry(common_directory.to_path_buf())
            .and_modify(|resource| resource.users += 1)
            .or_insert(SharedResource {
                value: repository,
                users: 1,
            })
            .value
            .clone()
    }

    pub fn release_repository(&self, common_directory: &Path) {
        release_resource(&mut self.repositories.lock(), common_directory);
    }

    pub fn next_repository_id(&self) -> RepositoryId {
        RepositoryId(self.next_repository_id.fetch_add(1, Ordering::Relaxed))
    }

    pub fn repository_id_counter(&self) -> Arc<AtomicU64> {
        self.next_repository_id.clone()
    }

    pub(crate) fn lsp_pool(&self) -> HostLspPool {
        self.lsp_pool.clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn resource_user_counts_for_test(&self) -> (usize, usize, usize) {
        (
            self.worktrees
                .lock()
                .values()
                .map(|resource| resource.users)
                .sum(),
            self.buffers
                .lock()
                .values()
                .map(|resource| resource.users)
                .sum(),
            self.repositories
                .lock()
                .values()
                .map(|resource| resource.users)
                .sum(),
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn lsp_user_count_for_test(&self, server_id: lsp::LanguageServerId) -> usize {
        self.lsp_pool.language_server_user_count(server_id)
    }
}

fn release_resource<Key, Query, Value>(
    resources: &mut HashMap<Key, SharedResource<Value>>,
    key: &Query,
) where
    Key: std::borrow::Borrow<Query> + std::hash::Hash + Eq,
    Query: std::hash::Hash + Eq + ?Sized,
{
    let remove = resources.get_mut(key).is_some_and(|resource| {
        resource.users = resource.users.saturating_sub(1);
        resource.users == 0
    });
    if remove {
        resources.remove(key);
    }
}
