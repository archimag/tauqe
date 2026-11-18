//! Domain model identity, configured role entries, selection, and effort.
mod catalog;
mod effort;
mod entry;
mod reference;
mod role;
mod selection;
pub use catalog::ModelCatalogEntry;
pub use effort::{ModelRolesEffort, ReasoningEffort};
pub use entry::{ArchitectTier, ModelEntry, ModelRolesEntries, ModelRolesSummary};
pub use reference::{ModelRef, ProviderKind};
pub use role::ModelRole;
pub use selection::ModelSelection;

pub(crate) use reference::deserialize_model_ref_lenient;
