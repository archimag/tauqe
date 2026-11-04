//! Typed refusal of one plan mutation.
//!
//! A plan mutation is refused before it reaches disk, and the refusal is born
//! typed: the code that produced it declares the category, so no lower layer and
//! no client ever derives one from the text of a reason. The message travels
//! beside the category and is the developer-facing wording a surface renders
//! verbatim.

use serde::{Deserialize, Serialize};

/// Why one plan mutation was refused.
///
/// The category belongs to the code that produced the refusal: the storage
/// barrier states whether the payload itself is defective or whether the turn
/// regime forbade the mutation, while the turn engine declares a payload it never
/// delivered to storage as abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanRejectionKind {
    /// The payload cannot be applied: it breaks the planning contract or the
    /// structural validation of the stored plan.
    PayloadDefect,
    /// The turn regime forbids the mutation, whatever the payload carries.
    ModeForbidden,
    /// The payload never reached plan storage, so the stored plan is untouched.
    Abandoned,
}

/// One refusal a plan mutation met, carried typed as far as the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRejection {
    /// Category of the refusal, decided where the refusal was produced.
    pub kind: PlanRejectionKind,
    /// Developer-facing wording of the refusal, rendered verbatim by a surface.
    pub message: String,
}

impl PlanRejection {
    /// Creates a rejection of `kind` carrying the wording it reports.
    pub fn new(kind: PlanRejectionKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Creates a rejection of a payload the planning contract refuses.
    pub fn payload_defect(message: impl Into<String>) -> Self {
        Self::new(PlanRejectionKind::PayloadDefect, message)
    }

    /// Creates a rejection of a mutation the turn regime forbids.
    pub fn mode_forbidden(message: impl Into<String>) -> Self {
        Self::new(PlanRejectionKind::ModeForbidden, message)
    }

    /// Creates a rejection of a payload that never reached plan storage.
    pub fn abandoned(message: impl Into<String>) -> Self {
        Self::new(PlanRejectionKind::Abandoned, message)
    }
}
