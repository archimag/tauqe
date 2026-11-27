//! Exclusive owner of developer-selected context paths and permissions.
use super::{
    ContextAccess, ContextItem, ContextLayer, ContextManager, ContextState, PatternAddOutcome,
};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// The only public entry to User-layer mutations. Detached scenario contexts
/// cannot be converted back to this owner.
#[derive(Default)]
pub struct ManualContext {
    manager: ContextManager,
}

impl ManualContext {
    pub fn new(root: PathBuf) -> Self {
        Self {
            manager: ContextManager::new(root),
        }
    }

    pub fn scenario(&self) -> ContextManager {
        self.manager.clone()
    }

    pub fn state(&self) -> ContextState {
        self.manager.get_state()
    }

    pub fn set_listener(&mut self, listener: Option<super::ContextListener>) {
        self.manager.set_listener(listener);
    }

    pub fn repo_root(&self) -> &Path {
        self.manager.repo_root()
    }

    pub fn add_file(
        &mut self,
        path: &str,
        access: ContextAccess,
        layer: ContextLayer,
    ) -> Result<ContextItem> {
        self.manager.add_file_to_layer(path, access, layer)
    }

    pub fn add_pattern(
        &mut self,
        pattern: &str,
        access: ContextAccess,
        files: &[String],
    ) -> Result<PatternAddOutcome> {
        self.manager.add_files_by_pattern(pattern, access, files)
    }

    pub fn remove_file(&mut self, path: &str) -> Result<bool> {
        self.manager.remove_file(path)
    }

    pub fn set_access(&mut self, path: &str, access: ContextAccess) -> Result<bool> {
        self.manager.set_access(path, access)
    }

    pub fn clear_user(&mut self) {
        self.manager.clear_user();
    }

    /// Full clearing is reserved for the owner of developer-selected paths.
    pub fn clear(&mut self) -> bool {
        self.manager.clear()
    }

    pub fn clear_auto(&mut self) -> bool {
        self.manager.clear_auto()
    }

    pub fn clear_outer_docs(&mut self) -> bool {
        self.manager.clear_outer_docs()
    }

    pub fn clear_transient(&mut self) -> bool {
        self.manager.clear_transient()
    }

    pub fn set_repo_root(&mut self, root: PathBuf) {
        self.manager.set_repo_root(root);
    }

    pub fn merge_turn_context(&mut self, turn: &ContextManager) {
        self.manager.merge_turn_context(turn);
    }

    pub fn scenario_mut<R>(&mut self, f: impl FnOnce(&mut ContextManager) -> R) -> R {
        f(&mut self.manager)
    }
}
