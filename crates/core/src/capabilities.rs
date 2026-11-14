//! Modular capability facade for the LLM interaction protocol.
//!
//! Provides the single source of truth for protocol capabilities: JSON schema
//! composition, structured response decoding, and contract rule/instruction
//! rendering.
//!
//! Lives in `crates/core` because decoding and sanitization touch domain
//! parsers and edit state, while the wire DTOs stay in `crates/protocol`.

use tauqe_protocol::{PatchStrategy, ProjectConfig};

#[allow(dead_code)]
mod decode;
mod regime;
mod rules;
mod schema;

pub struct PromptRenderContext<'a> {
    pub editable_paths: &'a [String],
    pub has_eager_repo_map: bool,
    pub project: Option<&'a ProjectConfig>,
    pub patch_strategy: PatchStrategy,
}

impl<'a> PromptRenderContext<'a> {
    pub fn new(editable_paths: &'a [String], _marker: Option<&'a str>) -> Self {
        Self {
            editable_paths,
            has_eager_repo_map: false,
            project: None,
            patch_strategy: PatchStrategy::SearchReplace,
        }
    }

    pub fn with_eager_repo_map(mut self, has_eager_repo_map: bool) -> Self {
        self.has_eager_repo_map = has_eager_repo_map;
        self
    }

    pub fn with_project(mut self, project: Option<&'a ProjectConfig>) -> Self {
        self.project = project;
        self
    }

    pub fn with_patch_strategy(mut self, patch_strategy: PatchStrategy) -> Self {
        self.patch_strategy = patch_strategy;
        self
    }
}

pub use rules::{
    compose_contract_rules, compose_structured_protocol_instructions, contract_rule, delta_rule,
    render_contract_rules, structured_instruction, ContractContext, ContractRule,
};

// The regime trunk composes as two typed lists, and an integration test must
// prove that the dialogue contours render that trunk byte-identically. The two
// composers are re-exported so the proof stays reachable without exposing the
// internal module layout.
pub use regime::{intent_delta_rules, shared_trunk_rules};

#[allow(unused_imports)]
pub(crate) use decode::decode_capability_expr;
pub(crate) use schema::capability_expr_schema;
