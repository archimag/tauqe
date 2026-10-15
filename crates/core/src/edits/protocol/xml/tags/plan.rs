//! Parser and types for the structured `<plan>` control tag.

mod concept;
mod item;

use super::{is_inside_markdown_code, strip_tags};
use crate::edits::protocol::xml::{extract_attribute, find_base_tag_start};

pub use item::ParsedPlanItem;
use concept::parse_concept_items_with_diagnostics;
use item::{next_item_tag_bounds, parse_plan_items_with_diagnostics};

/// Locates the next complete `<plan ...>...</plan>` or `<plan .../>` tag.
pub(crate) fn next_plan_tag(text: &str) -> Option<(usize, usize)> {
    const OPEN: &str = "<plan";
    let mut from = 0;
    while let Some(rel) = text[from..].find(OPEN) {
        let start = from + rel;
        let after = start + OPEN.len();
        let bytes = &text.as_bytes()[after..];
        let mut idx = 0;
        if idx < bytes.len() && (bytes[idx] == b'_' || bytes[idx] == b'-') {
            idx += 1;
            while idx < bytes.len()
                && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-')
            {
                idx += 1;
            }
        }
        let tag_name = &text[start + 1..after + idx];
        let rest = &text[after + idx..];
        let boundary = rest
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if boundary {
            if is_inside_markdown_code(text, start) {
                from = after;
                continue;
            }
            let gt = rest.find('>')?;
            let end_open = after + idx + gt + 1;
            if rest[..gt].trim_end().ends_with('/') {
                return Some((start, end_open));
            }
            let close_tag = format!("</{}>", tag_name);
            let close_offset = text[end_open..].find(&close_tag)?;
            let end = end_open + close_offset + close_tag.len();
            return Some((start, end));
        }
        from = after;
    }
    None
}

/// Removes all plan tags from text.
pub fn strip_plan_tags(text: &str) -> String {
    strip_tags(text, next_plan_tag)
}

/// Structured representation of a parsed `<plan>` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPlanTag {
    pub action: String,
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub concept: Vec<tauqe_protocol::ConceptItem>,
    pub items: Vec<ParsedPlanItem>,
}

fn find_closing_inner_tag(
    slice: &str,
    search_pos: usize,
    base_name: &str,
    open_tag_name: &str,
) -> Option<(usize, usize)> {
    let prefix = format!("</{base_name}");
    let mut pos = search_pos;
    while let Some(rel) = slice[pos..].find(&prefix) {
        let close_start = pos + rel;
        let after_prefix = &slice[close_start + prefix.len()..];
        if let Some(gt) = after_prefix.find('>') {
            let suffix = after_prefix[..gt].trim();
            let open_suffix = &open_tag_name[base_name.len()..];
            let is_match = suffix.is_empty()
                || suffix == open_suffix
                || (open_suffix.is_empty()
                    && !suffix.starts_with("_item")
                    && !suffix.starts_with("-item")
                    && (suffix.starts_with('_') || suffix.starts_with('-'))
                    && suffix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));

            if is_match {
                let close_end = close_start + prefix.len() + gt + 1;
                return Some((close_start, close_end));
            }
        }
        pos = close_start + prefix.len();
    }
    None
}

fn extract_and_strip_inner_tag(text: &str, base_name: &str) -> (Option<String>, String) {
    if let Some((start, tag_name)) = find_base_tag_start(text, base_name) {
        let slice = &text[start..];
        if let Some(open_end) = slice.find('>') {
            if slice[..open_end].trim_end().ends_with('/') {
                let full_end = start + open_end + 1;
                let mut remaining = String::with_capacity(text.len() - (full_end - start));
                remaining.push_str(&text[..start]);
                remaining.push_str(&text[full_end..]);
                return (None, remaining);
            }
            if let Some((close_start_in_slice, close_end_in_slice)) =
                find_closing_inner_tag(slice, open_end + 1, base_name, &tag_name)
            {
                let content = slice[open_end + 1..close_start_in_slice].trim().to_string();
                let full_end = start + close_end_in_slice;
                let mut remaining = String::with_capacity(text.len() - (full_end - start));
                remaining.push_str(&text[..start]);
                remaining.push_str(&text[full_end..]);
                return (Some(content), remaining);
            }
        }
    }
    (None, text.to_string())
}

/// Parses `<plan ...>...</plan>` blocks and collects syntax and structural diagnostics.
pub fn parse_plan_tags_with_diagnostics(text: &str) -> (Vec<ParsedPlanTag>, Vec<String>) {
    let mut plans = Vec::new();
    let mut errors = Vec::new();
    let mut pos = 0;

    while let Some(rel) = text[pos..].find("<plan") {
        let tag_start = pos + rel;
        let after_plan = tag_start + "<plan".len();
        let bytes = text.as_bytes();
        let mut idx = after_plan;
        if idx < bytes.len() && (bytes[idx] == b'_' || bytes[idx] == b'-') {
            idx += 1;
            while idx < bytes.len() && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'-') {
                idx += 1;
            }
        }
        let tag_name = &text[tag_start + 1..idx];
        let rest = &text[idx..];
        let boundary = rest.chars().next().is_some_and(|c| c.is_whitespace() || c == '>' || c == '/');
        if !boundary {
            pos = after_plan;
            continue;
        }

        if is_inside_markdown_code(text, tag_start) {
            pos = after_plan;
            continue;
        }

        let Some(gt) = rest.find('>') else {
            errors.push(format!("Malformed <{tag_name}> tag: unclosed opening tag"));
            break;
        };

        let header_end = idx + gt + 1;
        let header = &text[tag_start..header_end];
        let is_self_closing = rest[..gt].trim_end().ends_with('/');

        let raw_action_opt = extract_attribute(header, "action");
        let raw_id_opt = extract_attribute(header, "id");
        let raw_title_opt = extract_attribute(header, "title");

        let has_plan_attributes = raw_action_opt.is_some() || raw_id_opt.is_some() || raw_title_opt.is_some();

        let (body, next_pos) = if is_self_closing {
            ("", header_end)
        } else {
            let close_tag = format!("</{tag_name}>");
            if let Some(close_offset) = text[header_end..].find(&close_tag) {
                let body_slice = &text[header_end..header_end + close_offset];
                (body_slice, header_end + close_offset + close_tag.len())
            } else {
                if has_plan_attributes {
                    errors.push(format!("Malformed <{tag_name}> tag: missing closing tag '{close_tag}'"));
                }
                pos = header_end;
                continue;
            }
        };

        let raw_action = raw_action_opt.unwrap_or_else(|| "save".to_string());
        let action = match raw_action.trim().to_lowercase().as_str() {
            "save" | "create" => "save".to_string(),
            "update" | "modify" | "edit" => "update".to_string(),
            "delete" | "remove" => "delete".to_string(),
            other => {
                errors.push(format!("Unknown plan action '{other}' in <{tag_name}> (supported actions: save, update, delete)"));
                other.to_string()
            }
        };

        let id = raw_id_opt
            .unwrap_or_default()
            .trim()
            .trim_matches('\\')
            .trim_matches('"')
            .trim_matches('\'')
            .trim()
            .to_string();

        let (concept_block, body_without_concept) = extract_and_strip_inner_tag(body, "concept");
        let (concept, concept_errors) = if let Some(ref c_str) = concept_block {
            parse_concept_items_with_diagnostics(c_str)
        } else {
            (Vec::new(), Vec::new())
        };
        errors.extend(concept_errors);

        let plan_meta = strip_tags(&body_without_concept, next_item_tag_bounds);

        let title = raw_title_opt
            .or_else(|| {
                let (t, _) = extract_and_strip_inner_tag(&plan_meta, "title");
                t
            })
            .map(|s| s.trim().to_string());

        let description = extract_attribute(header, "description")
            .or_else(|| {
                let (d, _) = extract_and_strip_inner_tag(&plan_meta, "description");
                d.or_else(|| {
                    let (s, _) = extract_and_strip_inner_tag(&plan_meta, "summary");
                    s
                })
            });

        let (mut items, item_errors) = parse_plan_items_with_diagnostics(&body_without_concept);
        errors.extend(item_errors);

        if action == "save" {
            for item in &mut items {
                if item.title.is_empty() {
                    item.title = item.id.clone();
                }
            }
        } else if action == "update" && items.is_empty() && title.is_none() && description.is_none() && has_plan_attributes {
            errors.push(format!("Plan update for '{}' contains no items or fields to update", if id.is_empty() { tag_name } else { id.as_str() }));
        }

        if id.is_empty() && items.is_empty() && concept.is_empty() {
            if has_plan_attributes {
                errors.push(format!("Malformed <{tag_name}> tag: both 'id' and items are missing"));
            }
        } else if action == "save" && concept.is_empty() {
            if concept_block.is_some() {
                errors.push(format!(
                    "Plan '{}': <concept> section is present but empty or invalid; define architectural concepts using <concept_item id=\"c-...\" title=\"...\"> or bullet points",
                    if id.is_empty() { tag_name } else { id.as_str() }
                ));
            } else {
                errors.push(format!(
                    "Plan '{}' must contain a <concept> section defining architectural intent and invariants",
                    if id.is_empty() { tag_name } else { id.as_str() }
                ));
            }
        } else {
            plans.push(ParsedPlanTag {
                action,
                id: if id.is_empty() { "default".to_string() } else { id },
                title,
                description,
                concept,
                items,
            });
        }

        pos = next_pos;
    }

    (plans, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_plan_tags(text: &str) -> Vec<ParsedPlanTag> {
        parse_plan_tags_with_diagnostics(text).0
    }

    #[test]
    fn test_strip_plan_tags() {
        let text = "Intro\n<plan id=\"test\" title=\"T\">\n<item id=\"1\" title=\"Step 1\" />\n</plan>\nOutro";
        assert_eq!(strip_plan_tags(text).trim(), "Intro\n\nOutro");
    }

    #[test]
    fn test_parse_plan_tags() {
        let text = r#"
Here is our plan:
<plan action="save" id="jwt-auth" title="JWT Auth">
  <summary>Migrate to tokens</summary>
  <concept>
    <concept_item id="c-1" title="Token Security" />
  </concept>
  <item id="1" title="Data models" status="done">Define User and Claims</item>
  <item id="2" title="Handlers" status="in_progress">
    <item id="2.1" title="Refresh handler" status="todo" />
  </item>
</plan>
"#;
        let parsed = parse_plan_tags(text);
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.action, "save");
        assert_eq!(p.id, "jwt-auth");
        assert_eq!(p.title.as_deref(), Some("JWT Auth"));
        assert_eq!(p.description.as_deref(), Some("Migrate to tokens"));
        assert_eq!(p.concept.len(), 1);
        assert_eq!(p.concept[0].id, "c-1");
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].id, "1");
        assert_eq!(p.items[0].status, Some(tauqe_protocol::PlanItemStatus::Done));
        assert_eq!(p.items[0].details.as_deref(), Some("Define User and Claims"));
        assert_eq!(p.items[1].children.len(), 1);
        assert_eq!(p.items[1].children[0].id, "2.1");
    }

    #[test]
    fn test_parse_plan_tags_item_attributes_details() {
        let text = r#"
<plan action="save" id="test-attr" title="Test Attr">
  <concept>
    <concept_item id="c-1" title="Attribute Concept" />
  </concept>
  <item id="1" title="Self closing" status="todo" details="Attribute details here" />
  <item id="2" title="Paired tag" status="discussion" details="Paired with attr"></item>
</plan>
"#;
        let parsed = parse_plan_tags(text);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].concept.len(), 1);
        assert_eq!(parsed[0].items[0].details.as_deref(), Some("Attribute details here"));
        assert_eq!(parsed[0].items[1].details.as_deref(), Some("Paired with attr"));
    }

    #[test]
    fn test_parse_plan_tags_requires_concept_on_save() {
        let text = r#"
<plan action="save" id="flat-plan" title="Flat Plan">
  <item id="1" title="Step without concept" status="todo" />
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(plans.is_empty(), "Plan without concept should not be accepted on save");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("must contain a <concept> section"));
    }

    #[test]
    fn test_parse_concept_fallback_long_cyrillic_no_panic() {
        let text = r#"
<plan action="save" id="cyrillic-plan" title="Тест">
  <concept>
    Это очень длинная строка с кириллическим текстом, которая превышает лимит восемьдесят символов и раньше вызывала панику
  </concept>
  <item id="1" title="Шаг 1" status="todo" />
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(errors.is_empty(), "Errors: {:?}", errors);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].concept.len(), 1);
        assert!(plans[0].concept[0].title.ends_with("..."));
    }

    #[test]
    fn test_parse_plan_tags_with_markdown_concept_fallback() {
        let text = r#"
<plan action="save" id="md-concept" title="Markdown Concept">
  <concept>
    - #c-1: Verification Symmetry
      Preflight verification runs the exact same checks.
    - #c-2 Fail-Early: Abort immediately on broken build.
  </concept>
  <item id="1" title="Execute check" status="todo" />
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(errors.is_empty(), "Unexpected errors: {:?}", errors);
        assert_eq!(plans.len(), 1);
        let p = &plans[0];
        assert_eq!(p.concept.len(), 2);
        assert_eq!(p.concept[0].id, "c-1");
        assert_eq!(p.concept[0].title, "Verification Symmetry");
        assert_eq!(
            p.concept[0].details.as_deref(),
            Some("Preflight verification runs the exact same checks.")
        );
        assert_eq!(p.concept[1].id, "c-2");
        assert_eq!(p.concept[1].title, "Fail-Early");
        assert_eq!(
            p.concept[1].details.as_deref(),
            Some("Abort immediately on broken build.")
        );
    }

    #[test]
    fn test_parse_plan_tags_turn_marker_tolerance_on_inner_tag() {
        let text = r#"
<plan action="save" id="suffixed" title="Suffixed">
  <concept>
    - #c-1 Invariant One
  </concept>
  <item id="1" title="Step 1" status="todo" />
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(errors.is_empty(), "Unexpected errors: {:?}", errors);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].concept.len(), 1);
        assert_eq!(plans[0].concept[0].id, "c-1");
        assert_eq!(plans[0].concept[0].title, "Invariant One");
    }

    #[test]
    fn test_parse_plan_tags_concept_present_but_empty_diagnostic() {
        let text = r#"
<plan action="save" id="empty-concept" title="Empty Concept">
  <concept>   </concept>
  <item id="1" title="Step" status="todo" />
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(plans.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("is present but empty or invalid"));
    }

    #[test]
    fn test_parse_plan_tags_ignores_bare_mentions() {
        let text = "Here we discuss <plan> in text without closing tags or attributes.";
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(plans.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn test_parse_plan_tags_ignores_markdown_fences() {
        let text = r#"
Here is an example plan in markdown:
```markdown
<plan action="save" id="jwt-auth" title="JWT Auth">
  <concept>
    <concept_item id="c-1" title="Ex Concept" />
  </concept>
  <item id="1" title="Example item" />
</plan>
```
And outside markdown:
<plan action="save" id="real-plan" title="Real Plan">
  <concept>
    <concept_item id="c-1" title="Real Concept" />
  </concept>
  <item id="1" title="Real item" />
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(errors.is_empty(), "Errors: {:?}", errors);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, "real-plan");
    }

    #[test]
    fn test_strip_plan_tags_preserves_markdown_fences() {
        let text = "Example:\n```xml\n<plan id=\"example\" title=\"Ex\" />\n```\nReal:\n<plan id=\"real\" title=\"Real\" />\nDone.";
        let stripped = strip_plan_tags(text);
        assert!(stripped.contains("<plan id=\"example\""));
        assert!(!stripped.contains("<plan id=\"real\""));
    }

    #[test]
    fn test_parse_plan_tags_with_suffixed_tags_and_description() {
        let text = r#"
<plan_18DCD1DA6EB762E1 action="save" id="plan-exec" title="Execution Engine">
  <description_18DCD1DA6EB762E1>
    Architectural plan for isolated step execution.
    Contains multiple paragraphs of context.
  </description_18DCD1DA6EB762E1>
  <concept_18DCD1DA6EB762E1>
    <concept_item_18DCD1DA6EB762E1 id="c-1" title="Execution Harness" />
  </concept_18DCD1DA6EB762E1>
  <item_18DCD1DA6EB762E1 id="1" status="done">
    <title_18DCD1DA6EB762E1>Protocol tags</title_18DCD1DA6EB762E1>
    <details_18DCD1DA6EB762E1>Step done marker</details_18DCD1DA6EB762E1>
  </item_18DCD1DA6EB762E1>
  <item_18DCD1DA6EB762E1 id="2" title="UI Integration" status="todo" />
</plan_18DCD1DA6EB762E1>
"#;
        let (parsed, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(errors.is_empty(), "Unexpected errors: {:?}", errors);
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.id, "plan-exec");
        assert_eq!(p.title.as_deref(), Some("Execution Engine"));
        assert!(p.description.as_ref().unwrap().contains("Architectural plan"));
        assert_eq!(p.concept.len(), 1);
        assert_eq!(p.concept[0].id, "c-1");
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].id, "1");
        assert_eq!(p.items[0].status, Some(tauqe_protocol::PlanItemStatus::Done));
        assert_eq!(p.items[0].title, "Protocol tags");
        assert_eq!(p.items[1].id, "2");
        assert_eq!(p.items[1].status, Some(tauqe_protocol::PlanItemStatus::Todo));
    }

    #[test]
    fn test_parse_plan_tags_with_concept_and_files() {
        let text = r#"
<plan action="save" id="two-section" title="Two-Section Plan">
  <concept>
    <concept_item id="c-1" title="Architectural intent">
      <details>Core invariant details</details>
      <concept_item id="c-1.1" title="Sub invariant" />
    </concept_item>
  </concept>
  <item id="1" title="Implementation step" status="todo" files="crates/core/src/lib.rs,crates/protocol/src/plan.rs" concept_refs="c-1,c-1.1" />
</plan>
"#;
        let (parsed, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(errors.is_empty(), "Errors: {:?}", errors);
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.concept.len(), 1);
        assert_eq!(p.concept[0].id, "c-1");
        assert_eq!(p.concept[0].details.as_deref(), Some("Core invariant details"));
        assert_eq!(p.concept[0].children.len(), 1);
        assert_eq!(p.concept[0].children[0].id, "c-1.1");
        assert_eq!(p.items.len(), 1);
        assert_eq!(
            p.items[0].files,
            vec!["crates/core/src/lib.rs", "crates/protocol/src/plan.rs"]
        );
        assert_eq!(p.items[0].concept_refs, vec!["c-1", "c-1.1"]);
    }

    #[test]
    fn test_parse_plan_tags_clears_parent_files() {
        let text = r#"
<plan action="save" id="parent-files" title="Parent Files">
  <concept>
    <concept_item id="c-1" title="Concept" />
  </concept>
  <item id="1" title="Parent Step" status="todo" files="crates/core/src/parent.rs">
    <item id="1.1" title="Child Step" status="todo" files="crates/core/src/child.rs" />
  </item>
</plan>
"#;
        let (parsed, errors) = parse_plan_tags_with_diagnostics(text);
        assert!(errors.is_empty(), "Errors: {:?}", errors);
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].items[0].files.is_empty(), "Parent files must be cleared");
        assert_eq!(parsed[0].items[0].children[0].files, vec!["crates/core/src/child.rs"]);
    }

    #[test]
    fn test_parse_plan_tags_nested_elements() {
        let text = r#"
<plan action="save" id="rust-hello-world">
  <title>Rust Hello World</title>
  <description>Complete binary creation</description>
  <concept>
    <concept_item id="c-1" title="Binary Entrypoint Invariant" />
  </concept>
  <item id="1" status="todo">
    <title>Create package manifest and binary entrypoint</title>
    <details>Setup Cargo.toml and src/main.rs entrypoint for the application</details>
  </item>
  <item id="2" status="todo">
    <title>Compile and run</title>
    Run cargo run to verify
  </item>
</plan>
"#;
        let parsed = parse_plan_tags(text);
        assert_eq!(parsed.len(), 1);
        let p = &parsed[0];
        assert_eq!(p.id, "rust-hello-world");
        assert_eq!(p.title.as_deref(), Some("Rust Hello World"));
        assert_eq!(p.description.as_deref(), Some("Complete binary creation"));
        assert_eq!(p.concept.len(), 1);
        assert_eq!(p.concept[0].id, "c-1");
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].id, "1");
        assert_eq!(p.items[0].title, "Create package manifest and binary entrypoint");
        assert_eq!(p.items[0].details.as_deref(), Some("Setup Cargo.toml and src/main.rs entrypoint for the application"));
        assert_eq!(p.items[1].id, "2");
        assert_eq!(p.items[1].title, "Compile and run");
        assert_eq!(p.items[1].details.as_deref(), Some("Run cargo run to verify"));
    }

    #[test]
    fn test_parse_plan_tags_ignores_legacy_item_tier() {
        let text = r#"
<plan action="save" id="tiers" title="Tiers">
  <concept>
    <concept_item id="c-1" title="Tiers Concept" />
  </concept>
  <item id="1" title="Cheap" tier="junior" />
  <item id="2" title="Default" status="todo">Body</item>
  <item id="3" title="Hard" tier="Senior"></item>
</plan>
"#;
        let (plans, errors) = parse_plan_tags_with_diagnostics(text);
        let items = &plans[0].items;
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].id, "1");
        assert_eq!(items[1].id, "2");
        assert_eq!(items[2].id, "3");
        assert!(errors.is_empty(), "Unexpected errors: {:?}", errors);
    }
}
