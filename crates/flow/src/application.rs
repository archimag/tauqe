//! Application scenarios behind the public facade.

pub(crate) mod discussion_prompt;

mod infrastructure;
pub(crate) mod operation;
pub(crate) mod session;

pub(crate) mod shutdown;

pub(crate) mod dialogue;

pub(crate) mod configuration;

pub(crate) mod configuration_files;

pub(crate) mod system_status;

pub(crate) mod configuration_operation;

pub(crate) mod history;

mod history_compaction;

pub(crate) mod history_operation;

pub(crate) mod context;

pub(crate) mod context_operation;

pub(crate) mod outer_docs_cache;

pub(crate) mod outer_docs_preload;

mod implementation;
mod implementation_executor;
mod implementation_persistence;
mod implementation_pipeline;

pub(crate) mod implementation_refinement;

pub(crate) mod implementation_step;

mod review_execution;
mod review_sequence;
mod review_step;
pub(crate) mod step_dispatch;
mod step_execution;

pub(crate) mod turn_setup;

pub(crate) mod interactive_history;

pub(crate) mod interactive_completion;

pub(crate) mod interactive_dialogue;

pub(crate) mod interactive_interruption;

pub(crate) mod interactive_task;

pub(crate) mod usage;

pub(crate) mod usage_totals;

pub(crate) mod repository_startup;

pub(crate) mod review_analysis;

pub(crate) mod task_exit;

pub(crate) mod review_analysis_response;

pub(crate) mod review_analysis_completion;

pub(crate) mod review_analysis_execution;

pub(crate) mod review_analysis_load;

pub(crate) mod review_synthesis_completion;

pub(crate) mod review_synthesis_preparation;

pub(crate) mod review_synthesis_request;

pub(crate) mod review_synthesis_response;

pub(crate) mod review_observer;
pub(crate) mod review_synthesis_execution;

pub(crate) mod review_groups;

pub(crate) mod review_preset;

pub(crate) mod locale_generation;

pub(crate) mod git_squash;

pub(crate) mod plan_horizon;

pub(crate) mod preflight;

pub(crate) mod plan_batch;

pub(crate) mod plan_two_phase;

pub(crate) mod architect;
mod architect_synthesis;

pub(crate) mod review_batch;

pub(crate) mod review_preview;

pub(crate) mod plan_execution;

pub(crate) mod interactive_updates;
pub(crate) mod step_completion;

pub(crate) mod turn_execution;

pub(crate) mod interactive_finalization;

pub(crate) mod documents;
pub(crate) mod repository;

pub(crate) mod ask;
pub(crate) mod edit_cycle;
pub(crate) mod events;
mod implementation_host;
pub(crate) mod review_commands;
pub(crate) mod settings;
pub(crate) mod step_turn;

#[cfg(test)]
mod scenario_tests;

#[cfg(test)]
mod step_edit_completion_tests;

#[cfg(test)]
fn outer_doc_entry(id: &str, title: &str, source: &str) -> tauqe_core::config::OuterDocEntry {
    serde_json::from_value(serde_json::json!({ "id": id, "title": title, "source": source }))
        .unwrap()
}
