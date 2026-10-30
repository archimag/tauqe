//! Post-analysis prompt builders for the Architect role.
//!
//! Post-analysis is the Architect role's answer over the immutable findings of
//! a whole review session: it reads what every isolated review group reported
//! and names the systemic root causes no single group could observe. The decoded
//! payload, its strict schema, and the identity validation live in
//! [`crate::review::analysis`], so this module owns only the text the model
//! reads and keeps rendering exactly the field names the schema enforces.

use crate::escape::single_line;
use tauqe_protocol::review::{ReviewGroup, ReviewIssue, ReviewSession, DEFAULT_REVIEW_GROUP_ID};

/// Builds the system prompt for the cross-group review post-analysis.
///
/// The prompt is dispatched to the Architect role once the isolated review
/// groups of a session finish: the role reads every finding of the session
/// together and names the systemic root causes that span them, without editing
/// code and without planning their resolution.
pub fn build_post_analysis_system_prompt() -> String {
    let mut out = String::new();
    out.push_str("You are a Principal Software Architect in TAUQE (The Answer to the Ultimate Question of Engineering).\n\n");
    out.push_str("Your mission is Review Post-Analysis: reading the complete set of findings that several isolated review groups produced over one codebase, and identifying the systemic root causes running across them.\n\n");

    out.push_str("## Core Invariants & Rules:\n\n");

    out.push_str("1. Global Cross-Group Mandate:\n");
    out.push_str("   - Every group audited its own slice of the repository in isolation, so a defect repeated across slices stays invisible inside any single group.\n");
    out.push_str("   - Your exclusive value is the whole-session view: read all findings together and decide which ones are local defects and which ones are manifestations of one shared architectural root cause.\n");
    out.push_str("   - Every finding that carries an identity receives exactly one classification: it is either independent or a member of one architectural cluster, never both and never omitted.\n\n");

    out.push_str("2. Two-Level Classification Contract:\n");
    out.push_str("   - `independent_issues`: findings that stand on their own, because their defect shares no root cause with another finding and resolving them separately is correct.\n");
    out.push_str("   - `architectural_groups`: clusters of findings that manifest one shared architectural root cause. Each cluster states that root cause in `title`, explains in `description` why exactly these findings belong together, and lists the findings that manifest it in `issue_ids`.\n");
    out.push_str("   - Cluster by a root cause that one coherent fix would resolve together, never by superficial similarity: findings that merely mention the same module, type, or symptom do not share a root cause.\n\n");

    out.push_str("3. Identity Reference Invariant:\n");
    out.push_str("   - Findings are referenced exclusively by the `uuid` rendered for each of them: a 36-character UUIDv7 in canonical `8-4-4-4-12` hyphenated form, copied exactly as given.\n");
    out.push_str("   - The identity is exactly those 36 characters. Never wrap it in backticks or quotes, never prefix it with `uuid:`, and never shorten, re-case, re-spell, or otherwise rewrite it.\n");
    out.push_str("   - Never substitute another reference for it. The `#id` numbers are display labels of the review interface, not identities: the classification never uses them, and a finding title is prose rather than an identity.\n");
    out.push_str("   - Never invent an identifier, and never reference a finding the session does not list. The classification is validated against the identities the session actually holds, and a reference that resolves to nothing rejects the whole classification.\n\n");

    out.push_str("4. Analysis-Only Mandate:\n");
    out.push_str("   - You produce the classification layer and nothing else.\n");
    out.push_str("   - Do not modify code, do not describe patches, do not propose file-level changes, and do not create or extend any plan: each finding is resolved later by its own workflow.\n");
    out.push_str("   - State the root cause and the consequences that tie the findings to it; remediation design is out of scope.\n\n");

    out.push_str("5. Strict Language Consistency Invariant:\n");
    out.push_str(
        "   - Detect the primary natural language used in the findings of this session.\n",
    );
    out.push_str("   - You MUST formulate `summary`, cluster titles, cluster descriptions, and every explanation strictly in that SAME natural language, while keeping code identifiers, symbols, and file paths exact.\n\n");

    out.push_str("6. Output Format:\n");
    out.push_str("   - Output strictly according to the specified JSON schema, which contains exactly three fields: `summary`, `independent_issues`, and `architectural_groups`.\n");
    out.push_str("   - `summary` is the high-level Markdown narrative of the systemic picture of the session.\n");
    out.push_str("   - A session whose findings share no root cause returns empty classification lists; a session with no findings returns its summary together with empty lists.\n");
    out.push_str("   - Final Content Invariant: every text field carries only the final distilled result. The reasoning process itself is never classification content: do not record deliberation, self-questioning, exploratory analysis, or intermediate reasoning steps in any field.\n");

    out
}

/// Builds the user prompt carrying the whole session under post-analysis.
///
/// The prompt renders the session header, the groups it audited, and every
/// finding of every group, so the Architect classifies the session globally
/// instead of one group at a time.
pub fn build_post_analysis_user_prompt(session: &ReviewSession) -> String {
    let groups = session.effective_groups();
    let issues = session.issues();

    let session_id = single_line(&session.id);
    let clean_title = single_line(&session.title);
    let title = if clean_title.is_empty() {
        session.id.as_str()
    } else {
        clean_title.as_str()
    };

    let mut out = String::new();
    out.push_str(&format!(
        "# Review Post-Analysis: [{}] {}\n\n",
        session_id, title
    ));

    out.push_str("## Target Review Session:\n");
    out.push_str(&format!("- **Session**: {} ({})\n", title, session_id));
    if let Some(description) = &session.description {
        let trimmed = description.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("- **Scope / Summary**: {}\n", trimmed));
        }
    }
    out.push_str(&format!("- **Audited Groups**: {}\n", groups.len()));
    out.push_str(&format!(
        "- **Findings Under Post-Analysis**: {}\n\n",
        issues.len()
    ));

    out.push_str("## Audited Review Groups:\n");
    for group in &groups {
        out.push_str(&render_group_line(group));
    }
    out.push('\n');

    out.push_str("## Findings Under Post-Analysis:\n\n");
    if issues.is_empty() {
        out.push_str("No review group reported a finding, so the classification is empty.\n\n");
    } else {
        // Sections follow the order in which findings were recorded, so a
        // finding whose group is no longer declared still reaches the prompt
        // instead of silently dropping out of the classification.
        let mut sections: Vec<&str> = Vec::new();
        for issue in &issues {
            let group_id = issue.group.as_deref().unwrap_or(DEFAULT_REVIEW_GROUP_ID);
            if !sections.contains(&group_id) {
                sections.push(group_id);
            }
        }

        for group_id in sections {
            let declared = groups.iter().find(|group| group.id == group_id);
            let label = group_label(group_id, declared.map(|group| group.title.as_str()));
            out.push_str(&format!("### Producing Group: {}\n\n", label));
            for issue in issues.iter().filter(|issue| {
                issue.group.as_deref().unwrap_or(DEFAULT_REVIEW_GROUP_ID) == group_id
            }) {
                render_issue(&mut out, issue, &label);
            }
        }
    }

    out.push_str("## Post-Analysis Instructions:\n");
    out.push_str("1. Read every finding above together: the groups audited different slices, so compare findings across groups before deciding what is local and what is systemic.\n");
    out.push_str("2. Classify each finding that carries a `uuid` exactly once: list it in `independent_issues` when it stands on its own, or in `issue_ids` of the `architectural_groups` cluster whose root cause it manifests.\n");
    out.push_str("3. Reference findings by their `uuid` only: copy the 36 characters rendered after `UUID` exactly as they appear, without backticks, quotes, a `uuid:` label, or any shortened or re-cased variant.\n");
    out.push_str("4. Never substitute a display label for an identity: the `#id` numbers and the finding titles identify nothing, and an identifier this prompt does not show does not exist.\n");
    out.push_str("5. Report root causes only: propose no patches, no file-level changes, and no implementation steps.\n");
    out.push_str("6. Language Directive: formulate the summary and every cluster title and description strictly in the same natural language as the findings above.\n");

    out
}

/// Renders one audited group of the session overview with the paths it covered.
fn render_group_line(group: &ReviewGroup) -> String {
    let label = group_label(&group.id, Some(group.title.as_str()));
    if group.paths.is_empty() {
        return format!("- {}\n", label);
    }
    let paths = render_paths(&group.paths);
    format!("- {} (paths: {})\n", label, paths)
}

/// Renders a group as `Title (`id`)`, falling back to the bare identifier when
/// the session no longer declares a title for it.
fn group_label(group_id: &str, title: Option<&str>) -> String {
    let clean_title = title.map(single_line).filter(|title| !title.is_empty());
    match clean_title {
        Some(title) => format!("{} (`{}`)", title, single_line(group_id)),
        None => format!("`{}`", single_line(group_id)),
    }
}

/// Renders one finding: its identity, the group that produced it, and the
/// analytical facts the classification is built from.
fn render_issue(out: &mut String, issue: &ReviewIssue, producing_group: &str) {
    out.push_str(&format!("#### Finding #{}\n", issue.id));

    let uuid = issue
        .uuid
        .as_deref()
        .map(str::trim)
        .filter(|uuid| !uuid.is_empty());
    match uuid {
        Some(uuid) => out.push_str(&format!("- **UUID**: `{}`\n", uuid)),
        None => {
            out.push_str("- **UUID**: (not assigned, so this finding cannot be referenced by the classification)\n")
        }
    }
    out.push_str(&format!("- **Producing Group**: {}\n", producing_group));
    out.push_str(&format!("- **Category**: {}\n", issue.issue_type));

    let clean_title = single_line(&issue.title);
    if !clean_title.is_empty() {
        out.push_str(&format!("- **Title**: {}\n", clean_title));
    }
    if !issue.files.is_empty() {
        out.push_str(&format!("- **Files**: {}\n", render_paths(&issue.files)));
    }
    if !issue.readonly_files.is_empty() {
        out.push_str(&format!(
            "- **Protected Contracts (readonly_files)**: {}\n",
            render_paths(&issue.readonly_files)
        ));
    }
    let description = issue.description.trim();
    if !description.is_empty() {
        out.push_str(&format!("- **Description**:\n{}\n", description));
    }
    if !issue.recommendations.is_empty() {
        out.push_str("- **Recommendations**:\n");
        for (index, recommendation) in issue.recommendations.iter().enumerate() {
            out.push_str(&format!("  {}. {}\n", index + 1, recommendation.trim()));
        }
    }
    out.push('\n');
}

/// Renders repository paths as an inline list.
fn render_paths(paths: &[String]) -> String {
    paths
        .iter()
        .map(|path| format!("`{}`", single_line(path)))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauqe_protocol::review::{IssueType, ReviewItem, ReviewItemStatus, ReviewSeverity};
    use tauqe_protocol::ModelRef;

    /// Identity of the finding the core engine group produced.
    const CORE_UUID: &str = "018f9c1e-6b3a-7c1e-8b3a-000000000001";
    /// Identity of the finding the terminal frontend group produced.
    const TUI_UUID: &str = "018f9c1e-6b3a-7c1e-8b3a-000000000002";

    fn core_group() -> ReviewGroup {
        ReviewGroup {
            id: "core-engine".to_string(),
            title: "Core Engine".to_string(),
            paths: vec!["crates/core/src".to_string()],
            prompt: None,
            ..Default::default()
        }
    }

    fn tui_group() -> ReviewGroup {
        ReviewGroup {
            id: "terminal-ui".to_string(),
            title: "Terminal UI".to_string(),
            paths: vec!["crates/tui/src".to_string()],
            prompt: None,
            ..Default::default()
        }
    }

    fn finding(id: u32, uuid: Option<&str>, group: Option<&str>, title: &str) -> ReviewItem {
        ReviewItem {
            id,
            title: title.to_string(),
            uuid: uuid.map(str::to_string),
            group: group.map(str::to_string),
            model: None,
            issue_type: IssueType::Architecture,
            severity: ReviewSeverity::from(IssueType::Architecture),
            status: ReviewItemStatus::Todo,
            recommendations: vec!["Introduce one shared transaction boundary".to_string()],
            files: vec!["crates/core/src/git.rs".to_string()],
            readonly_files: vec!["Conventions.md".to_string()],
            resolution_notes: None,
            body: "The change set reaches the worktree without one shared boundary.".to_string(),
        }
    }

    #[test]
    fn test_post_analysis_user_prompt_covers_every_group_and_finding() {
        let session = ReviewSession {
            id: "rev-multi".to_string(),
            title: "Multi-Level Audit".to_string(),
            created_at: 100,
            model: ModelRef::openrouter("model-x"),
            description: Some("Audit of the core engine and the terminal frontend".to_string()),
            items: vec![
                finding(
                    1,
                    Some(CORE_UUID),
                    Some("core-engine"),
                    "Unshared commit boundary",
                ),
                finding(
                    2,
                    Some(TUI_UUID),
                    Some("terminal-ui"),
                    "Unshared commit boundary in the frontend",
                ),
            ],
            groups: vec![core_group(), tui_group()],
            ..Default::default()
        };

        let prompt = build_post_analysis_user_prompt(&session);
        assert!(prompt.contains("# Review Post-Analysis: [rev-multi] Multi-Level Audit"));
        assert!(prompt.contains("- **Audited Groups**: 2"));
        assert!(prompt.contains("- **Findings Under Post-Analysis**: 2"));

        // Group titles and the paths they audited name the provenance of every
        // finding, so the classification sees which slice reported what.
        assert!(prompt.contains("Core Engine (`core-engine`)"));
        assert!(prompt.contains("Terminal UI (`terminal-ui`)"));
        assert!(prompt.contains("crates/core/src"));
        assert!(prompt.contains("crates/tui/src"));

        // Every finding reaches the prompt with its identity: a dropped
        // identifier would make that finding impossible to classify.
        assert!(prompt.contains(&format!("- **UUID**: `{CORE_UUID}`")));
        assert!(prompt.contains(&format!("- **UUID**: `{TUI_UUID}`")));
        assert!(prompt.contains("#### Finding #1"));
        assert!(prompt.contains("#### Finding #2"));
        assert!(prompt.contains("The change set reaches the worktree without one shared boundary."));
        assert!(prompt.contains("  1. Introduce one shared transaction boundary"));

        // The post-analysis is analysis only: it never advertises the edit,
        // discovery, or verification contours of a code-modifying turn.
        assert!(!prompt.contains("<edit"));
        assert!(!prompt.contains("<context_request"));
        assert!(!prompt.contains("<verify"));
    }

    #[test]
    fn test_post_analysis_user_prompt_resolves_the_implicit_default_group() {
        let session = ReviewSession {
            id: "rev-single".to_string(),
            title: "Single Audit".to_string(),
            created_at: 100,
            model: ModelRef::openrouter("model-x"),
            target_files: vec!["crates/core/src/review".to_string()],
            items: vec![finding(
                1,
                Some(CORE_UUID),
                None,
                "Unshared commit boundary",
            )],
            ..Default::default()
        };

        let prompt = build_post_analysis_user_prompt(&session);
        // A finding without a group belongs to the implicit default group, so
        // the prompt names that provenance instead of leaving it blank.
        let default_label = format!("Default (`{DEFAULT_REVIEW_GROUP_ID}`)");
        assert!(prompt.contains(&default_label));
        assert!(prompt.contains(&format!("- **Producing Group**: {default_label}")));
        assert!(prompt.contains(&format!("- **UUID**: `{CORE_UUID}`")));
    }

    #[test]
    fn test_post_analysis_user_prompt_reports_an_empty_session() {
        let session = ReviewSession {
            id: "rev-empty".to_string(),
            title: "Empty Audit".to_string(),
            created_at: 100,
            model: ModelRef::openrouter("model-x"),
            ..Default::default()
        };

        let prompt = build_post_analysis_user_prompt(&session);
        assert!(prompt.contains("- **Audited Groups**: 1"));
        assert!(prompt.contains("- **Findings Under Post-Analysis**: 0"));
        assert!(prompt.contains("No review group reported a finding"));
    }

    #[test]
    fn test_post_analysis_user_prompt_flags_a_finding_without_identity() {
        let session = ReviewSession {
            id: "rev-legacy".to_string(),
            title: "Legacy Audit".to_string(),
            created_at: 100,
            model: ModelRef::openrouter("model-x"),
            items: vec![finding(1, None, None, "Unshared commit boundary")],
            ..Default::default()
        };

        let prompt = build_post_analysis_user_prompt(&session);
        assert!(prompt.contains("(not assigned, so this finding cannot be referenced"));
        assert!(prompt.contains("#### Finding #1"));
    }

    #[test]
    fn test_post_analysis_system_prompt_names_the_schema_and_forbids_edits() {
        let prompt = build_post_analysis_system_prompt();
        assert!(prompt.contains("Review Post-Analysis"));
        assert!(prompt.contains("`summary`"));
        assert!(prompt.contains("`independent_issues`"));
        assert!(prompt.contains("`architectural_groups`"));
        assert!(prompt.contains("`issue_ids`"));
        assert!(prompt.contains("`uuid`"));
        assert!(prompt.contains("never uses them"));
        assert!(prompt.contains("Do not modify code"));
        assert!(prompt.contains("do not create or extend any plan"));
        assert!(!prompt.contains("<edit"));
        assert!(!prompt.contains("<context_request"));
    }
    #[test]
    fn test_post_analysis_prompts_demand_bare_uuid_identities() {
        let system = build_post_analysis_system_prompt();
        assert!(system.contains("36-character UUIDv7"));
        assert!(system.contains("8-4-4-4-12"));
        assert!(system.contains("Never wrap it in backticks or quotes"));
        assert!(system.contains("never uses them"));

        let session = ReviewSession {
            id: "rev-identities".to_string(),
            title: "Identity Audit".to_string(),
            created_at: 100,
            model: ModelRef::openrouter("model-x"),
            items: vec![finding(
                1,
                Some(CORE_UUID),
                Some("core-engine"),
                "Unshared commit boundary",
            )],
            groups: vec![core_group()],
            ..Default::default()
        };

        let prompt = build_post_analysis_user_prompt(&session);
        assert!(prompt.contains("copy the 36 characters"));
        assert!(prompt.contains("without backticks"));
        assert!(prompt.contains("the `#id` numbers and the finding titles identify nothing"));
    }
}
