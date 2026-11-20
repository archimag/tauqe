//! Modular capability facade for the LLM interaction protocol.
//!
//! Provides the single source of truth for protocol capabilities: JSON schema
//! composition, structured response decoding, and contract rule/instruction
//! rendering.
//!
//! Lives in `crates/core` because decoding and sanitization touch domain
//! parsers and edit state. Operational trees and normalized model commands live
//! in `crate::llm`, separate from the TUI RPC DTOs.

use crate::config::project::ProjectConfig;
use crate::workflow::PatchStrategy;

mod decode;
mod preview;
mod regime;
mod rules;
mod schema;

#[allow(unused_imports)] // The preview filter will consume this projection in a later step.
pub(crate) use preview::{preview_routes, PreviewCondition, PreviewRoute, PreviewRouteKind};

pub(crate) struct PromptRenderContext<'a> {
    _editable_paths: &'a [String],
    has_eager_repo_map: bool,
    project: Option<&'a ProjectConfig>,
    patch_strategy: PatchStrategy,
}

impl<'a> PromptRenderContext<'a> {
    pub(crate) fn new(editable_paths: &'a [String], _marker: Option<&'a str>) -> Self {
        Self {
            _editable_paths: editable_paths,
            has_eager_repo_map: false,
            project: None,
            patch_strategy: PatchStrategy::SearchReplace,
        }
    }

    pub(crate) fn with_eager_repo_map(mut self, has_eager_repo_map: bool) -> Self {
        self.has_eager_repo_map = has_eager_repo_map;
        self
    }

    pub(crate) fn with_project(mut self, project: Option<&'a ProjectConfig>) -> Self {
        self.project = project;
        self
    }

    pub(crate) fn with_patch_strategy(mut self, patch_strategy: PatchStrategy) -> Self {
        self.patch_strategy = patch_strategy;
        self
    }
}

pub(crate) use rules::{
    compose_contract_rules, compose_structured_protocol_instructions, ContractContext,
};

#[cfg(test)]
pub(crate) use rules::render_contract_rules;

#[allow(unused_imports)]
pub(crate) use decode::decode_capability_expr;
pub(crate) use schema::capability_expr_schema;
