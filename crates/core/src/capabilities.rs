//! Modular capability registry for the LLM interaction protocol.
//!
//! Every protocol capability is an autonomous [`CapabilityModule`]: a single
//! source of truth for its tag schema, its prompt fragments, its output
//! decoder, and its sanitizer. The [`CapabilityRegistry`] composes the modules
//! that a [`CapabilitySet`](tauqe_protocol::CapabilitySet) enables, so a
//! capability can never drift between its tags, its prompt text, and its
//! decoder.
//!
//! The registry lives in `crates/core` because decoding and sanitization touch
//! domain parsers and edit state, while the wire DTOs stay in `crates/protocol`.

mod builtin;
mod regime;
mod registry;
mod schema;

pub use registry::{
    render_contract_rules, CapabilityModule, CapabilityRegistry, ContractContext, ContractRule,
    PromptRenderContext,
};

// Protocol-to-command mapping helpers are owned by the capability modules that
// define the corresponding capability. Re-exporting them keeps a single source
// of truth for the edit-protocol-to-command conversion.
pub(crate) use builtin::{
    abort_invocations, custom_tool_invocations, map_verify_on_success, map_verify_target,
};

// The tag schema is a single source of truth shared with the parametric XML
// engine. Re-exporting it here keeps the capability-module contract nameable
// without leaking the engine's internal module layout.
pub use crate::edits::protocol::xml::engine::{MarkerPolicy, TagCloseMode, XmlTagSpec};
