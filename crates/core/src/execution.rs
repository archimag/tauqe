//! Technical execution of a single, frozen model request.
//!
//! Callers own context, history, transcripts, admission and continuation policy.
//! Preparation does not call a provider or mutate those inputs. Execution consumes
//! the prepared request once and reports facts rather than choosing another round.

mod accepted_rounds;
pub mod budget;
pub mod cache;
pub mod executor;
pub mod outcome;
pub(crate) mod request;
pub mod structured;
pub mod support;
pub mod task;
mod transcript;
pub mod usage;

pub use accepted_rounds::AcceptedRoundId;
pub use cache::PromptEnvironment;
pub use executor::{PromptExecutor, ReadyPromptExecutor};
pub use outcome::{ExecutionError, ResponseEnvelope};
pub use support::{CancellationToken, EventSink};
pub use transcript::ResearchEvidence;
