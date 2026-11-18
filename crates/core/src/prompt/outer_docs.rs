//! Diagnostics appended to step prompts when declared documentation is unavailable.

/// Appends the diagnostics of the declared external documents to a step prompt.
///
/// Only the documents the harness could not mount reach the prompt: a mounted
/// document is already in the read-only OuterDocs layer, so announcing it would
/// restate the context block. The prompt tells the model to route around the
/// missing material instead of spending a round waiting for it.
pub fn append_outer_docs_diagnostics(prompt: &mut String, diagnostics: &[String]) {
    if diagnostics.is_empty() {
        return;
    }
    prompt.push_str("\n## Declared External Documentation Unavailable\n");
    prompt.push_str(
        "The plan declares the following external documents for this step, but the harness could not mount them:\n",
    );
    for diagnostic in diagnostics {
        prompt.push_str(diagnostic);
        prompt.push('\n');
    }
    prompt.push_str(
        "Work with the repository material you already hold and route around the missing document instead of waiting for it. The same document may be reachable again in a later turn.\n",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_append_outer_docs_diagnostics_renders_only_the_reported_failures() {
        let mut prompt = "Execute the step.".to_string();
        append_outer_docs_diagnostics(&mut prompt, &[]);
        assert_eq!(
            prompt, "Execute the step.",
            "a step whose documents all mounted keeps its prompt unchanged"
        );

        append_outer_docs_diagnostics(
            &mut prompt,
            &["- `vendor/api.md`: the document answered with the HTTP status 503".to_string()],
        );

        assert!(prompt.contains("## Declared External Documentation Unavailable"));
        assert!(prompt.contains("`vendor/api.md`"));
        assert!(prompt.contains("route around the missing document"));
    }
}
