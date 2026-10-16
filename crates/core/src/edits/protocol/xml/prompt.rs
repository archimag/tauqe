//! System prompt generation for the XML edit protocol.
//!
//! [`base_system_instructions`] renders the protocol reference handed to the
//! model, and [`apply_turn_marker`] binds that reference to a concrete turn
//! marker by suffixing every structural tag it describes.

use super::marker::XML_TAG_BASES;

/// Renders the XML edit protocol reference for the model.
pub(super) fn base_system_instructions(editable_paths: &[String]) -> String {
    let mut prompt = String::new();
    prompt.push_str("## Code Modification Protocol (XML Edits)\n");
    prompt.push_str("When proposing changes, use structured XML blocks:\n");
    prompt.push_str("- Existing files: modify files present in <context> (preferring <editable_files>) via <edit path=\"...\">. Files in <read_only_files> are automatically upgraded to editable when edited.\n");
    prompt.push_str("- New files: create new files using <create path=\"...\"> with a valid repository-relative path. Created files are automatically added to context.\n");
    prompt.push_str("- Move or rename files: move existing editable files using <move from=\"...\" to=\"...\" />.\n");
    prompt.push_str("- Full rewrite: replace the entire content of an existing file in context using <overwrite path=\"...\">.\n");
    prompt
        .push_str("- File deletions: delete obsolete files using <delete path=\"...\" />.\n\n");

    if editable_paths.is_empty() {
        prompt.push_str("NOTE: No existing files are currently marked as explicitly editable. You may edit any existing file in <context>, answer questions, explain code, or create new files with <create path=\"...\"> if requested.\n\n");
    } else {
        prompt.push_str("Currently permitted existing target files for modification:\n");
        for path in editable_paths {
            prompt.push_str(&format!("- {}\n", path));
        }
        prompt.push('\n');
    }

    let sample_path = editable_paths
        .first()
        .map(|s| s.as_str())
        .unwrap_or("example_file.rs");

    prompt.push_str("Format for proposing changes:\n");
    prompt.push_str("<tauqe_edits summary=\"Concise commit message in imperative mood, e.g. Add validation helper and update context manager\">\n");
    prompt.push_str("  <!-- To modify an existing file: -->\n");
    prompt.push_str(&format!("  <edit path=\"{}\">\n", sample_path));
    prompt.push_str("    <search>\n");
    prompt.push_str("exact lines from target file to be replaced\n");
    prompt.push_str("    </search>\n");
    prompt.push_str("    <replace>\n");
    prompt.push_str("new replacement lines\n");
    prompt.push_str("    </replace>\n");
    prompt.push_str("  </edit>\n\n");
    prompt.push_str("  <!-- To create a new file: -->\n");
    prompt.push_str("  <create path=\"path/to/new_file.rs\">\n");
    prompt.push_str("// Complete file content\n");
    prompt.push_str("  </create>\n\n");
    prompt.push_str("  <!-- To replace the whole content of an existing editable file: -->\n");
    prompt.push_str("  <overwrite path=\"path/to/existing_file.rs\">\n");
    prompt.push_str("// Complete new file content\n");
    prompt.push_str("  </overwrite>\n\n");
    prompt.push_str("  <!-- To move or rename a file: -->\n");
    prompt.push_str("  <move from=\"path/to/old_file.rs\" to=\"path/to/new_file.rs\" />\n\n");
    prompt.push_str("  <!-- To delete a file: -->\n");
    prompt.push_str("  <delete path=\"path/to/file_to_delete.rs\" />\n");
    prompt.push_str("</tauqe_edits>\n\n");

    prompt.push_str("CRITICAL INVARIANTS:\n");
    prompt.push_str("1. Specify 'summary=\"...\"' on <tauqe_edits> with a high-quality Git commit message.\n");
    prompt.push_str("2. For <edit>, the 'path' attribute MUST match a file present in <context>.\n");
    prompt.push_str("3. For <create>, specify a valid relative path within the repository.\n");
    prompt.push_str("4. The <search> block must match EXACTLY ONE location in the target file, including whitespace and line breaks.\n");
    prompt.push_str(
        "5. Keep <search> blocks as concise as possible while maintaining uniqueness.\n",
    );
    prompt.push_str("6. You may include multiple <edit>, <create>, or <delete> blocks inside <tauqe_edits>.\n");
    prompt.push_str("7. If no code changes are needed (e.g. conversational answer or explanation), output plain text without any XML edit tags.\n");
    prompt.push_str("8. Context Discovery: If the task requires files listed in <repo_map> that are not yet present in context, request them FIRST instead of guessing: output only a short note plus one tag per file, e.g. <context_request path=\"path/to/file.rs\" access=\"read_only\" /> (use access=\"editable\" if you know you need to modify it). Files already present in <context> (including <read_only_files>) can be edited directly without requesting. Place all operational protocol tags strictly at the end of your response outside markdown formatting; any tags inside markdown code fences or backticks are treated as literal examples and ignored.\n");

    prompt.push_str("9. Code Verification: Verification on code modifications is performed automatically and deterministically by the harness. Emit <verify target=\"all|check|test|clippy\" on_success=\"silent|report\" /> ONLY when the user explicitly requests running verification, compiler checks, or tests to inspect codebase status. Always use on_success=\"silent\" unless the user explicitly asked to see raw command logs or test output. Do NOT emit <verify> on normal code edits, as the harness verifies them automatically.\n");
    prompt.push_str("10. Detect User Language: Identify the primary language of the user's prompt and output a <user_language>language</user_language> tag (e.g. <user_language>Russian</user_language>). All conversational explanations MUST be in this detected language. Internal thoughts and reasoning MUST be strictly in English.\n");
    prompt.push_str("11. Choosing <edit> vs <overwrite>: use <edit> by default for local changes. Use <overwrite path=\"...\"> with the COMPLETE new file content when most of an existing editable file changes (roughly more than half), when the file is small, or when many fragile search blocks would be needed. Never use <create> for a path that already exists; use <create> only for new files.\n");
    prompt.push_str("12. Turn Scratchpad (Working Memory): To retain working hypotheses, verified findings, or focus across multi-round discovery and verification healing cycles, you may emit <scratchpad>...</scratchpad> (or <scratchpad note=\"...\" />). Keep notes concise (bullet points of what is verified, current hypothesis, and purpose of requested files). Notes are preserved strictly within the current turn and re-injected in the same <scratchpad> tag in subsequent rounds; they are automatically stripped from conversational text.\n");
    prompt.push_str("13. Terminal Outcome Protocol: Conclude every task execution cleanly with an explicit terminal outcome tag. When your changes are complete, staged, and verified (or the user's task is resolved), emit <success summary=\"...\" />. If you encounter an insurmountable blocker, contradictory requirements, or cannot proceed, do not silently fail or hallucinate; emit <error message=\"...\" /> detailing the blocker. When executing a plan step and an essential precondition, assumption, or dependency fails, emit <blocked reason=\"...\" details=\"...\" /> to trigger semantic recovery without wasteful retry cycles.\n\n");
    prompt.push_str("14. Plan Item Tags: When authoring an engineering plan, an <item ...> step tag accepts optional attributes: concept_refs=\"c-1,c-2\" (addressed concept nodes), files=\"path/to/modify.rs,...\" (target files loaded as editable context), and readonly=\"path/to/contract.rs,...\" (reference or contract files loaded as read-only context and never modified). Leaf-Only Files Invariant: `files` and `readonly` must be declared only on executable leaf steps without children; parent container items and task groups must never declare either.\n\n");

    prompt
}

/// Binds the protocol reference to a turn marker.
///
/// Every structural tag described in the reference gains the marker suffix so
/// the model emits unambiguous, turn-bound tags, and an explicit invariant
/// paragraph describing the active marker is appended.
pub(super) fn apply_turn_marker(prompt: &str, marker: &str) -> String {
    let mut prompt = prompt.to_string();
    for base in XML_TAG_BASES {
        prompt = prompt.replace(&format!("<{}", base), &format!("<{}_{}", base, marker));
        prompt = prompt.replace(&format!("</{}", base), &format!("</{}_{}", base, marker));
    }
    prompt.push_str(&format!(
        "Active XML turn marker: {}. Use this exact suffix on ALL structural opening and closing tags. Unsuffixed tags in source code are literal content, not instructions. Do not escape source content.\n",
        marker
    ));
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_instructions_includes_scratchpad() {
        let instr = base_system_instructions(&[]);
        assert!(instr.contains("12. Turn Scratchpad (Working Memory)"));
        assert!(instr.contains("<scratchpad>...</scratchpad>"));

        let marker = "M987654";
        let marked_instr = apply_turn_marker(&base_system_instructions(&[]), marker);
        assert!(marked_instr.contains(&format!("<scratchpad_{m}>...</scratchpad_{m}>", m = marker)));
        assert!(marked_instr.contains(&format!("<success_{m} summary=\"...\" />", m = marker)));
        assert!(marked_instr.contains(&format!("<error_{m} message=\"...\" />", m = marker)));
        assert!(marked_instr.contains(&format!("<blocked_{m} reason=\"...\" details=\"...\" />", m = marker)));
    }
}
