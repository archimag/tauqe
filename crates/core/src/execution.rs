//! Technical execution of a single, frozen model request.
//!
//! Callers own context, history, transcripts, admission and continuation policy.
//! Preparation does not call a provider or mutate those inputs. Execution consumes
//! the prepared request once and reports facts rather than choosing another round.

pub mod cache;
pub mod executor;
pub mod outcome;
pub(crate) mod request;
pub mod structured;
pub mod support;
pub mod transcript;

pub use executor::{PreparedPrompt, PromptExecutor, PromptInput, PromptSequence};
pub use outcome::{ExecutionError, PromptOutcome, PromptResult, ResponseEnvelope};
pub use support::{CancellationToken, EventSink};
