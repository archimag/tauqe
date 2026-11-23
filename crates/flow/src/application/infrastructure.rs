//! Operation-bound technical dependencies for application scenarios.
//!
//! Execution bindings freeze configuration and resolve providers, model
//! parameters, protocols, and edit policy under the operation lease. Dialogue,
//! isolated edit, structured request, and Architect research units leave context,
//! history, capability grants, sequencing, and persistence with their callers.
//! Structured task supervision handles cancellation, event delivery, and bounded
//! shutdown, not the scenario's completion or fallback policy.

pub(crate) mod configuration;

pub(crate) mod execution;

pub(crate) mod structured_task;

pub(crate) mod system_status;

pub(crate) mod usage;
