use std::sync::Arc;

use anyhow::Result;
use collections::HashMap;
use context_server::ContextServerCommand;
use extension::ContextServerConfiguration;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Global, Task};

use crate::worktree_store::WorktreeStore;

pub trait ContextServerDescriptor {
    fn command(
        &self,
        project_context: Option<Entity<WorktreeStore>>,
        cx: &AsyncApp,
    ) -> Task<Result<ContextServerCommand>>;
    fn configuration(
        &self,
        project_context: Option<Entity<WorktreeStore>>,
        cx: &AsyncApp,
    ) -> Task<Result<Option<ContextServerConfiguration>>>;
}

struct GlobalContextServerDescriptorRegistry(Entity<ContextServerDescriptorRegistry>);

impl Global for GlobalContextServerDescriptorRegistry {}

#[derive(Default)]
pub struct ContextServerDescriptorRegistry {
    context_servers: HashMap<Arc<str>, Arc<dyn ContextServerDescriptor>>,
    descriptor_revisions: HashMap<Arc<str>, u64>,
}

impl ContextServerDescriptorRegistry {
    /// Returns the global [`ContextServerDescriptorRegistry`].
    ///
    /// Inserts a default [`ContextServerDescriptorRegistry`] if one does not yet exist.
    pub fn default_global(cx: &mut App) -> Entity<Self> {
        if !cx.has_global::<GlobalContextServerDescriptorRegistry>() {
            let registry = cx.new(|_| Self::new());
            cx.set_global(GlobalContextServerDescriptorRegistry(registry));
        }
        cx.global::<GlobalContextServerDescriptorRegistry>()
            .0
            .clone()
    }

    pub fn new() -> Self {
        Self {
            context_servers: HashMap::default(),
            descriptor_revisions: HashMap::default(),
        }
    }

    pub fn descriptor_revision(&self, id: &str) -> u64 {
        self.descriptor_revisions.get(id).copied().unwrap_or(0)
    }

    pub fn context_server_descriptors(&self) -> Vec<(Arc<str>, Arc<dyn ContextServerDescriptor>)> {
        self.context_servers
            .iter()
            .map(|(id, factory)| (id.clone(), factory.clone()))
            .collect()
    }

    pub fn context_server_descriptor(&self, id: &str) -> Option<Arc<dyn ContextServerDescriptor>> {
        self.context_servers.get(id).cloned()
    }

    /// Registers the provided [`ContextServerDescriptor`].
    pub fn register_context_server_descriptor(
        &mut self,
        id: Arc<str>,
        descriptor: Arc<dyn ContextServerDescriptor>,
        cx: &mut Context<Self>,
    ) {
        self.context_servers.insert(id.clone(), descriptor);
        let revision = self.descriptor_revisions.entry(id).or_default();
        *revision = revision.wrapping_add(1);
        cx.notify();
    }

    /// Unregisters the [`ContextServerDescriptor`] for the server with the given ID.
    pub fn unregister_context_server_descriptor_by_id(
        &mut self,
        server_id: &str,
        cx: &mut Context<Self>,
    ) {
        self.context_servers.remove(server_id);
        let revision = self
            .descriptor_revisions
            .entry(server_id.into())
            .or_default();
        *revision = revision.wrapping_add(1);
        cx.notify();
    }
}
