//! Layout policies that compose the message sequence of a bounded turn.
//!
//! Prompt composition is a policy rather than a fixed block order. A
//! [`PromptLayout`] receives an assembly that already rendered every
//! deterministic block and decides in which order those blocks become messages.
//! Implementations are strictly interchangeable: a caller selects one through
//! the typed [`PromptLayoutKind`] of the turn, never through a policy name.
//!
//! Every policy reads the same [`ContextBlockCache`], so the rendering of a
//! `<file>` element is layout-agnostic and a policy reproduces byte for byte the
//! element another policy already sent to the provider.
//!
//! Two policies exist:
//!
//! - [`SemanticLayout`] groups the material by kind: the whole context block and
//!   the whole history block each become one stable section of the system
//!   message.
//! - [`ChronologicalLayout`] turns the same material into a transcript: a context
//!   file appears between the turn records that observed its arrival, so the
//!   prompt grows by appending to its tail instead of rewriting a section in the
//!   middle.
//!
//! ## Chronological invalidation points
//!
//! The chronological layout keeps the provider's cached prefix warm only while
//! the transcript stays append-only. Four structural mutations legitimately move
//! an earlier byte, and each is a documented — never silent — boundary:
//!
//! - **Repository outline change.** The frozen symbol map (or the compact
//!   directory outline) closes the head. It is frozen for the whole turn, but a
//!   later turn generates it again and excludes the files that entered the
//!   context, so the head itself changes and invalidates the whole tail. That is
//!   the deliberate price of keeping the large outline in the stable head
//!   instead of paying a full reprocessing pass on every round.
//! - **Access promotion.** Promoting a file from `read_only` to `editable`
//!   rewrites the `access` attribute of that file's own element in place. The
//!   element keeps its position and its neighbors keep their bytes, exactly as
//!   the semantic layout documents.
//! - **Prune and rollback.** Removing a missing file or a rolled-back turn
//!   removes an element from the transcript and therefore shifts every message
//!   after it. The removal is the purpose of the operation, so the invalidation
//!   is accepted rather than avoided.
//! - **Compaction.** Replacing a summarized prefix with a single summary record
//!   rewrites the opening of the transcript. The dialogue session identifier is
//!   rotated together with it, because the provider must not be asked to reuse a
//!   node cache whose prefix the dialogue no longer sends.

use tauqe_protocol::PromptLayoutKind;

use crate::history::HistoryEntry;
use crate::prompt::protocol::PromptProtocol;
use crate::prompt::{ContextBlockCache, ContextFileContent, PromptAssembly};
use crate::providers::ChatMessage;

/// Policy that turns a rendered [`PromptAssembly`] into a message sequence.
pub trait PromptLayout: Send + Sync {
    /// Composes the full message sequence of a bounded turn.
    ///
    /// `current_prompt` is the developer message that closes the sequence, and
    /// `context_cache` is the turn-scoped memo shared by every policy, so a file
    /// rendered by one policy is reproduced verbatim by another.
    fn assemble(
        &self,
        assembly: &PromptAssembly,
        current_prompt: &str,
        protocol: &dyn PromptProtocol,
        context_cache: &mut ContextBlockCache,
    ) -> Vec<ChatMessage>;
}

/// Semantic block-order policy.
///
/// The stable prefix is rendered in caching order — system contract, repository
/// outline, context files in insertion order, active plan, language directive,
/// and session history — and the genuinely round-varying scratchpad closes the
/// sequence. The `<context>` block lists every file of the turn in insertion
/// order, so a file that discovery appends extends the block at its tail.
pub struct SemanticLayout;

/// The semantic block-order policy shared by every turn that selects it.
const SEMANTIC_LAYOUT: SemanticLayout = SemanticLayout;

impl PromptLayout for SemanticLayout {
    fn assemble(
        &self,
        assembly: &PromptAssembly,
        current_prompt: &str,
        protocol: &dyn PromptProtocol,
        context_cache: &mut ContextBlockCache,
    ) -> Vec<ChatMessage> {
        // Stable prefix in caching order: the system contract, the repository
        // outline, the context files in insertion order, the active plan, the
        // language directive, and the session history. Every block here is
        // identical from one round of a bounded turn to the next, so the
        // provider reuses the cached prefix instead of reprocessing it.
        let mut system_text = assembly.build_system_prompt(protocol);

        // The eager symbol map and the compact directory outline are mutually
        // exclusive: the model reads either symbol contours or the folder
        // layout, never both, so the prefix stays as small as one of them.
        if let Some(outline_block) = assembly.format_repository_outline_block() {
            system_text.push_str(&outline_block);
        }

        if let Some(context_block) = assembly.format_context_block_with(context_cache) {
            system_text.push_str("## Project Context\n");
            system_text.push_str(&context_block);
            system_text.push_str("\n\n");
        }

        if let Some(plan_block) = assembly.format_active_plan_block() {
            system_text.push_str(&plan_block);
        }

        if let Some(language_block) = assembly.format_language_directive_block() {
            system_text.push_str(&language_block);
        }

        // The session history is the stable block that changes most often: it
        // grows after every completed turn, and a changed byte invalidates
        // every cached token that follows it. Rendering it after the context
        // files, the active plan, and the language directive keeps the
        // divergence point of two consecutive turns as late as possible, so
        // the context warmed by the previous turn stays inside the provider's
        // cached prefix.
        // The block is rendered from the frozen records rather than carried as
        // a pre-rendered string, so the chronological policy reads the identical
        // structural material instead of re-parsing text.
        if let Some(history_block) = assembly
            .history_snapshot()
            .and_then(|snapshot| snapshot.render_block())
        {
            system_text.push_str("## Session History\n");
            system_text.push_str(&history_block);
            system_text.push_str("\n\n");
        }

        // Volatile tail: the turn scratchpad is rewritten on every round, so it
        // is appended after the whole stable prefix. Round-specific diagnostics
        // (patch retry reports, verification failures) travel separately as
        // append-only extensions of the user message.
        if let Some(scratchpad_block) = assembly.format_scratchpad_section() {
            system_text.push_str(&scratchpad_block);
        }

        // The regime delta opens the developer message while the task closes it,
        // so the whole stable prefix stays byte-identical and only the tail states
        // what the declared regime authorizes.
        let mut user_text = String::new();
        if let Some(delta) = assembly.format_regime_delta_section(protocol) {
            user_text.push_str(&delta);
        }
        user_text.push_str(current_prompt);

        vec![
            ChatMessage::system(system_text),
            ChatMessage::user(user_text),
        ]
    }
}

/// Chronological transcript policy.
///
/// The sequence is three parts: a byte-stable head, an interleaved transcript,
/// and a volatile tail.
///
/// The head renders the introduction, the system contract, the protocol
/// instructions, the project metadata, the language directive, and the frozen
/// repository outline. The outline stays in the head on purpose: a turn spends
/// most of its rounds exploring and planning, where the map hardly changes, and
/// the map is large enough that placing it in the tail would pay a full
/// reprocessing pass on every round.
///
/// The transcript renders one message per interaction record and emits the
/// context files a record had already observed right after that record. A file
/// that no record observed yet is emitted after the last record, so a turn that
/// discovers a file appends it at the tail of the transcript and never rewrites
/// an element the provider already cached.
///
/// The tail compensates for the lost semantic grouping with a compact manifest
/// of the current context — paths and access, never content — and keeps the
/// active plan and the turn scratchpad after the transcript. The tail is the
/// only place for genuinely round-varying content, exactly as in the semantic
/// policy.
pub struct ChronologicalLayout;

/// The chronological transcript policy shared by every turn that selects it.
const CHRONOLOGICAL_LAYOUT: ChronologicalLayout = ChronologicalLayout;

impl PromptLayout for ChronologicalLayout {
    fn assemble(
        &self,
        assembly: &PromptAssembly,
        current_prompt: &str,
        protocol: &dyn PromptProtocol,
        context_cache: &mut ContextBlockCache,
    ) -> Vec<ChatMessage> {
        let mut head = assembly.build_system_prompt(protocol);
        if let Some(outline_block) = assembly.format_repository_outline_block() {
            head.push_str(&outline_block);
        }
        if let Some(language_block) = assembly.format_language_directive_block() {
            head.push_str(&language_block);
        }

        let mut messages = vec![ChatMessage::system(head)];

        // Every context file is placed after the first turn record that already
        // observed it, which is the water-mark material the persisted records
        // carry: a turn records the context paths it started with, so a file
        // that entered the context between two turns appears between them. A
        // file no record observed yet is the newest arrival and closes the
        // transcript.
        let records: &[HistoryEntry] = assembly
            .history_snapshot()
            .map(|snapshot| snapshot.entries())
            .unwrap_or(&[]);
        let mut arrivals: Vec<Vec<&ContextFileContent>> = vec![Vec::new(); records.len() + 1];
        for file in &assembly.context_files {
            let slot = observed_by(file.path.as_str(), records).unwrap_or(records.len());
            arrivals[slot].push(file);
        }

        for (record, slot) in records.iter().zip(arrivals.iter()) {
            messages.push(record_message(record));
            push_arrived_files(&mut messages, slot, assembly, context_cache);
        }
        if let Some(slot) = arrivals.last() {
            push_arrived_files(&mut messages, slot, assembly, context_cache);
        }

        // The tail is the only volatile part of the sequence: it grows at the
        // end of the transcript, so a round that carries more working memory or
        // a wider context never moves a byte the provider already cached.
        let mut tail = String::new();
        // The regime delta is the leading block of the trailing message: the
        // transcript keeps every byte it already sent while the regime states the
        // invariants only its own turn carries.
        if let Some(delta) = assembly.format_regime_delta_section(protocol) {
            tail.push_str(&delta);
        }
        if let Some(plan_block) = assembly.format_active_plan_block() {
            tail.push_str(&plan_block);
        }
        if let Some(manifest_block) = assembly.format_context_manifest_section() {
            tail.push_str(&manifest_block);
        }
        if let Some(scratchpad_block) = assembly.format_scratchpad_section() {
            tail.push_str(&scratchpad_block);
        }
        if !tail.is_empty() {
            messages.push(ChatMessage::user(tail));
        }

        messages.push(ChatMessage::user(current_prompt));
        messages
    }
}

/// Returns the index of the first turn record that already observed `path`.
///
/// The observed context of a turn is recorded on disk with the turn itself, so
/// the placement is derived from durable material instead of from the current
/// layout state: the same session always yields the same transcript.
fn observed_by(path: &str, records: &[HistoryEntry]) -> Option<usize> {
    records.iter().position(|record| match record {
        HistoryEntry::Turn { context, .. } => context.iter().any(|observed| observed == path),
        _ => false,
    })
}

/// Renders one interaction record as a transparent transcript message.
///
/// A developer prompt and a model answer keep their own roles, so the transcript
/// reads like an ordinary dialogue. Records the harness produced — a compacted
/// summary and a review report — arrive as developer-side context, because they
/// describe the session rather than continue it.
fn record_message(record: &HistoryEntry) -> ChatMessage {
    match record {
        HistoryEntry::Turn { prompt, .. } => ChatMessage::user(prompt.clone()),
        HistoryEntry::Response { message, .. } => ChatMessage::assistant(message.clone()),
        HistoryEntry::Summary { text, .. } => {
            ChatMessage::user(format!("## Compacted Session Summary\n{}", text.trim()))
        }
        HistoryEntry::Review {
            model,
            findings_count,
            summary,
            ..
        } => {
            let mut text = format!("## Code Review\nModel: {model}\nFindings: {findings_count}");
            if let Some(summary) = summary
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
            {
                text.push('\n');
                text.push_str(summary);
            }
            ChatMessage::user(text)
        }
    }
}

/// Emits the context files that arrived at one point of the transcript.
///
/// The element rendering is the one the context block uses, so the memo hands
/// back an element byte for byte and an unchanged file never moves.
fn push_arrived_files(
    messages: &mut Vec<ChatMessage>,
    files: &[&ContextFileContent],
    assembly: &PromptAssembly,
    context_cache: &mut ContextBlockCache,
) {
    if files.is_empty() {
        return;
    }
    let mut out = String::from("<context>\n");
    for file in files {
        let access = assembly.context_file_access(file);
        out.push_str(context_cache.element(file, access, assembly.patch_strategy));
    }
    out.push_str("</context>");
    messages.push(ChatMessage::user(out));
}

/// Returns the layout policy that serves `kind`.
///
/// The typed kind is the only thing a caller may branch on: the resolution from
/// configuration and turn intent happens once, and every policy behind this
/// function stays interchangeable.
pub fn layout_for(kind: PromptLayoutKind) -> &'static dyn PromptLayout {
    match kind {
        PromptLayoutKind::Semantic => &SEMANTIC_LAYOUT,
        PromptLayoutKind::Chronological => &CHRONOLOGICAL_LAYOUT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextFileContent;
    use crate::history::{HistoryEntry, HistorySnapshot};
    use crate::prompt::xml::XmlPromptProtocol;
    use tauqe_protocol::{ContextAccess, EditProtocolKind};

    fn file(path: &str, access: ContextAccess, content: &str) -> ContextFileContent {
        ContextFileContent {
            path: path.to_string(),
            access,
            content: content.to_string(),
        }
    }

    fn turn(id: u64, prompt: &str, context: &[&str]) -> HistoryEntry {
        HistoryEntry::Turn {
            id: Some(id),
            prompt: prompt.to_string(),
            context: context.iter().map(|path| path.to_string()).collect(),
            context_watermark: None,
        }
    }

    fn response(id: u64, message: &str) -> HistoryEntry {
        HistoryEntry::Response {
            id: Some(id),
            message: message.to_string(),
            commit: None,
            summary: None,
            files: Vec::new(),
        }
    }

    fn assembly(files: Vec<ContextFileContent>, records: Vec<HistoryEntry>) -> PromptAssembly {
        PromptAssembly::new(
            None,
            1,
            files,
            "git",
            EditProtocolKind::Xml,
            Some(HistorySnapshot::new(records)),
            None,
        )
    }

    fn chronological(files: Vec<ContextFileContent>, records: Vec<HistoryEntry>) -> PromptAssembly {
        let mut assembly = assembly(files, records);
        assembly.prompt_layout = PromptLayoutKind::Chronological;
        assembly
    }

    #[test]
    fn transcript_interleaves_files_with_the_turns_that_observed_them() {
        let assembly = chronological(
            vec![
                file("pinned.rs", ContextAccess::ReadOnly, "pinned body\n"),
                file("later.rs", ContextAccess::Editable, "later body\n"),
            ],
            vec![
                turn(1, "First task", &["pinned.rs"]),
                response(2, "First done"),
                turn(3, "Second task", &["pinned.rs", "later.rs"]),
                response(4, "Second done"),
            ],
        );

        let messages = assembly.assemble_chat_messages("Third task", &XmlPromptProtocol);
        assert_eq!(messages.len(), 9);
        assert_eq!(messages[0].role, "system");

        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[1].content, "First task");
        // The file the first turn already carried lands right after it.
        assert_eq!(messages[2].role, "user");
        assert!(messages[2].content.contains("path=\"pinned.rs\""));
        assert!(!messages[2].content.contains("later.rs"));

        assert_eq!(messages[3].role, "assistant");
        assert_eq!(messages[3].content, "First done");

        // The file that arrived between the two turns lands after the turn that
        // observed it, never before it.
        assert_eq!(messages[4].content, "Second task");
        assert!(messages[5].content.contains("path=\"later.rs\""));
        assert!(!messages[5].content.contains("pinned.rs"));
        assert_eq!(messages[6].content, "Second done");

        assert!(messages[7].content.contains("<context_manifest>"));
        assert_eq!(messages[8].role, "user");
        assert_eq!(messages[8].content, "Third task");
    }

    #[test]
    fn head_carries_the_frozen_outline_and_never_a_context_body() {
        let mut assembly = chronological(
            vec![file(
                "src/main.rs",
                ContextAccess::Editable,
                "fn main() {}\n",
            )],
            vec![turn(1, "First task", &["src/main.rs"])],
        );
        assembly.repo_map = Some("crate\n  pub fn main".to_string());
        assembly.target_language = Some("Russian".to_string());

        let messages = assembly.assemble_chat_messages("Go", &XmlPromptProtocol);
        let head = &messages[0].content;

        assert!(head.contains("## Repository Map"));
        assert!(head.contains("crate\n  pub fn main"));
        assert!(head.contains("## Language Directive"));
        assert!(head.contains("Target Response Language: Russian"));

        // The context body travels through the transcript, never through the
        // byte-stable head.
        assert!(!head.contains("path=\"src/main.rs\""));
        assert!(!head.contains("fn main() {}"));
        assert!(messages
            .iter()
            .skip(1)
            .any(|message| message.content.contains("fn main() {}")));
    }

    #[test]
    fn growth_between_turns_appends_and_confines_divergence_to_the_tail() {
        let before = chronological(
            vec![file("pinned.rs", ContextAccess::ReadOnly, "pinned\n")],
            vec![
                turn(1, "First task", &["pinned.rs"]),
                response(2, "First done"),
            ],
        )
        .assemble_chat_messages("Second task", &XmlPromptProtocol);

        let after = chronological(
            vec![
                file("pinned.rs", ContextAccess::ReadOnly, "pinned\n"),
                file("fresh.rs", ContextAccess::Editable, "fresh\n"),
            ],
            vec![
                turn(1, "First task", &["pinned.rs"]),
                response(2, "First done"),
                turn(3, "Second task", &["pinned.rs"]),
                response(4, "Second done"),
                turn(5, "Third task", &["pinned.rs", "fresh.rs"]),
            ],
        )
        .assemble_chat_messages("Fourth task", &XmlPromptProtocol);

        assert!(after.len() > before.len());
        // The head is byte-identical and the transcript keeps every earlier
        // message as a prefix; the first divergence is the volatile tail.
        assert_eq!(after[0], before[0]);
        assert_eq!(&after[..4], &before[..4]);
        assert!(before[4].content.contains("<context_manifest>"));
        assert_eq!(
            after.last().map(|message| message.content.as_str()),
            Some("Fourth task")
        );
    }

    #[test]
    fn tail_manifest_lists_paths_and_access_without_content() {
        let assembly = chronological(
            vec![
                file("src/a.rs", ContextAccess::ReadOnly, "alpha body\n"),
                file("src/b.rs", ContextAccess::Editable, "beta body\n"),
            ],
            vec![turn(1, "Task", &["src/a.rs", "src/b.rs"])],
        );

        let messages = assembly.assemble_chat_messages("Go", &XmlPromptProtocol);
        let tail = &messages[messages.len() - 2].content;

        assert!(tail.contains("<context_manifest>"));
        assert!(tail.contains("path=\"src/a.rs\" access=\"read_only\" />"));
        assert!(tail.contains("path=\"src/b.rs\" access=\"editable\" />"));
        assert!(!tail.contains("alpha body"));
        assert!(!tail.contains("beta body"));
    }

    #[test]
    fn chronological_kind_resolves_to_the_transcript_policy() {
        let mut assembly = assembly(
            vec![file("src/a.rs", ContextAccess::Editable, "alpha\n")],
            Vec::new(),
        );

        // The semantic policy keeps the whole context inside the system message.
        let semantic = assembly.assemble_chat_messages("Go", &XmlPromptProtocol);
        assert!(semantic[0].content.contains("alpha\n"));
        assert_eq!(semantic.len(), 2);

        // The chronological policy hands the body through the transcript and
        // closes the sequence with the developer message.
        assembly.prompt_layout = PromptLayoutKind::Chronological;
        let transcript = assembly.assemble_chat_messages("Go", &XmlPromptProtocol);
        assert!(!transcript[0].content.contains("alpha\n"));
        assert!(transcript[1].content.contains("alpha\n"));
        assert!(transcript.len() > 2);
        assert_eq!(
            transcript.last().map(|message| message.content.as_str()),
            Some("Go")
        );
    }
    #[test]
    fn regime_delta_opens_the_trailing_user_message_in_both_policies() {
        let build = |layout: PromptLayoutKind| {
            let mut assembly = assembly(
                vec![file("src/a.rs", ContextAccess::Editable, "alpha\n")],
                vec![turn(1, "Task", &["src/a.rs"])],
            );
            assembly.prompt_layout = layout;
            assembly
        };

        for layout in [PromptLayoutKind::Semantic, PromptLayoutKind::Chronological] {
            let assembly = build(layout);
            let delta = assembly
                .format_regime_delta_section(&XmlPromptProtocol)
                .expect("the interactive regime states a delta");
            assert!(delta.starts_with("## Regime Delta\n"));
            assert!(delta.contains("Action Authorization"));
            assert!(delta.contains("Plan-First Proposal"));

            let messages = assembly.assemble_chat_messages("Go", &XmlPromptProtocol);
            assert!(!messages[0].content.contains("Action Authorization"));
            match layout {
                PromptLayoutKind::Semantic => {
                    assert_eq!(messages.len(), 2);
                    let user = &messages[1].content;
                    assert!(user.starts_with(&delta));
                    assert!(user.ends_with("Go"));
                }
                PromptLayoutKind::Chronological => {
                    let tail = &messages[messages.len() - 2].content;
                    assert!(tail.starts_with(&delta));
                    assert!(tail.contains("<context_manifest>"));
                    assert_eq!(
                        messages.last().map(|message| message.content.as_str()),
                        Some("Go")
                    );
                }
            }
        }
    }
}
