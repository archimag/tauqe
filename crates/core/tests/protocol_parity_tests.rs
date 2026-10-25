//! Cross-transport parity of the unified protocol surfaces.
//!
//! The harness upholds one invariant above the wire format: the capability
//! surface of a turn follows from its declared intent and never from the
//! transport that serializes it. A change that quietly re-couples a capability
//! to XML or to Structured Output therefore has to fail here, before it reaches
//! a developer.
//!
//! Two matrices verify the invariant where it is observable end to end:
//!
//! - the *purity matrix* inspects the assembled system prompt of every
//!   production `intent × transport` combination, proving that a structured
//!   prompt advertises no operational tag while an XML prompt names no field of
//!   the structured schema;
//! - the *parity matrix* inspects the narrowed capability surface, the
//!   structured schema that surface projects, and the commands equivalent
//!   payloads decode into on both transports.

use tauqe_core::capabilities::CapabilityRegistry;
use tauqe_core::prompt::json::JsonPromptProtocol;
use tauqe_core::prompt::protocol::PromptProtocol;
use tauqe_core::prompt::xml::XmlPromptProtocol;
use tauqe_core::prompt::PromptAssembly;
use tauqe_protocol::{CapabilitySet, EditProtocolKind, LlmCapability, LlmCommand, TurnIntent};

/// Every intent the harness can start a bounded turn with.
const ALL_INTENTS: [TurnIntent; 3] = [
    TurnIntent::InteractiveRepl,
    TurnIntent::ReadonlyDiscussion,
    TurnIntent::IsolatedStep,
];

/// Operational protocol tags a structured prompt must never advertise.
///
/// The prompt-side blocks that legitimately survive a structured turn are not
/// protocol tags and therefore match none of these prefixes: the `<context>`
/// block of the core contract, its `access` attribute, and the
/// `<system_documentation>` feedback block.
const OPERATIONAL_TAGS: &[&str] = &[
    "<abort",
    "<blocked",
    "<create",
    "<move",
    "<overwrite",
    "<delete",
    "<edits",
    "<edit",
    "<plan",
    "<success",
    "<error",
    "<skip",
    "<verify",
    "<scratchpad",
    "<user_language",
    "<context_request",
    "<repo_search",
    "<repo_dirs",
    "<repo_map",
    "<dir_list",
    "<doc_request",
    "<custom_tool",
    "<search",
    "<replace",
];

// ---------------------------------------------------------------------------
// Schema ↔ DTO round-trip (generated valid JSON decodes into Rust)
// ---------------------------------------------------------------------------

/// Returns the JSON Schema `type` spellings a node admits, if it declares any.
fn schema_types(node: &serde_json::Value) -> Vec<String> {
    match node.get("type") {
        Some(serde_json::Value::String(kind)) => vec![kind.clone()],
        Some(serde_json::Value::Array(kinds)) => kinds
            .iter()
            .filter_map(|kind| kind.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Nesting depth the schema generator expands before it stops recursing.
///
/// The walk spends one level per object property it descends into, so seeding
/// it with zero would emit every `$ref`-backed collection empty and a generated
/// plan would carry no concept node and no step at all. Seeding with this bound
/// produces authored nodes at every level while the recursion still terminates.
const GENERATED_TREE_DEPTH: usize = 2;

/// Generates a minimal value that validates against `schema`.
///
/// Generation walks the very schema the model is constrained by, so a valid
/// document can only exist when the schema is satisfiable. A `$ref` is resolved
/// through the root definitions the caller hands over, which is what lets the
/// recursive plan tree be generated from its own definition instead of being
/// inlined. `depth` bounds that recursion: a nested tree is generated a few
/// levels deep and then leaves `children` empty, which keeps the generator
/// terminating while still exercising the recursive contract.
fn generate_from_schema(
    schema: &serde_json::Value,
    definitions: &serde_json::Value,
    depth: usize,
) -> serde_json::Value {
    // A reference resolves against the shared definitions, never against the
    // node that happens to carry it.
    if let Some(reference) = schema.get("$ref").and_then(|node| node.as_str()) {
        let resolved = reference
            .strip_prefix("#/$defs/")
            .and_then(|name| definitions.get(name));
        return match resolved {
            Some(target) => generate_from_schema(target, definitions, depth),
            None => serde_json::Value::Null,
        };
    }

    // A union picks the first branch that is not the idle `null`, so the
    // generated document carries a payload rather than the idle form.
    if let Some(variants) = schema.get("anyOf").and_then(|node| node.as_array()) {
        let chosen = variants
            .iter()
            .find(|variant| schema_types(variant) != vec!["null".to_string()])
            .or_else(|| variants.first());
        return match chosen {
            Some(variant) => generate_from_schema(variant, definitions, depth),
            None => serde_json::Value::Null,
        };
    }

    let types = schema_types(schema);
    let is_array = types.iter().any(|kind| kind == "array");
    if is_array {
        let items = schema
            .get("items")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        // A recursive definition is generated one level shallower; at the
        // bound the collection is emitted empty.
        if depth == 0 && items.to_string().contains("$ref") {
            return serde_json::Value::Array(Vec::new());
        }
        let element = generate_from_schema(&items, definitions, depth.saturating_sub(1));
        return serde_json::Value::Array(vec![element]);
    }

    if types.iter().any(|kind| kind == "object") {
        let mut object = serde_json::Map::new();
        let properties = schema
            .get("properties")
            .and_then(|node| node.as_object())
            .cloned()
            .unwrap_or_default();
        for (name, property) in &properties {
            object.insert(
                name.clone(),
                generate_from_schema(property, definitions, depth.saturating_sub(1)),
            );
        }
        return serde_json::Value::Object(object);
    }

    if types.iter().any(|kind| kind == "string") {
        // An enumerated string picks its first declared spelling, so the value
        // is one the Rust model accepts.
        if let Some(values) = schema.get("enum").and_then(|node| node.as_array()) {
            if let Some(first) = values.first().and_then(|value| value.as_str()) {
                return serde_json::Value::String(first.to_string());
            }
        }
        return serde_json::Value::String("generated".to_string());
    }

    if types.iter().any(|kind| kind == "boolean") {
        return serde_json::Value::Bool(false);
    }

    if types
        .iter()
        .any(|kind| kind == "number" || kind == "integer")
    {
        return serde_json::json!(0);
    }

    serde_json::Value::Null
}

#[test]
fn generated_schema_documents_decode_into_the_rust_plan_model() {
    let definitions = tauqe_protocol::discussion::schema::plan_schema_definitions();
    let payload = tauqe_protocol::discussion::schema::plan_update_schema();

    // The payload is a nullable union: the active branch is the object one.
    let active = payload["anyOf"]
        .as_array()
        .and_then(|variants| {
            variants.iter().find(|variant| {
                variant.get("type").and_then(|node| node.as_str()) == Some("object")
            })
        })
        .expect("the plan payload declares an object branch");

    let generated = generate_from_schema(active, &definitions, GENERATED_TREE_DEPTH);
    let update: tauqe_protocol::DiscussionPlanUpdate = serde_json::from_value(generated.clone())
        .expect("a document generated from the schema decodes into the plan payload");

    // The decode is not vacuous: the generated document carries the payload
    // the schema describes.
    assert_eq!(update.action, tauqe_protocol::DiscussionPlanAction::Update);
    assert!(!update.concept.is_empty());
    assert!(!update.items.is_empty());

    // Each authored node round-trips back into the generated shape, so the
    // definitions and the Rust tree agree in both directions.
    let concept = update
        .concept
        .first()
        .expect("the generated document carries a concept node");
    let encoded = serde_json::to_value(concept).expect("a concept node encodes");
    let decoded: tauqe_protocol::ConceptItem =
        serde_json::from_value(encoded).expect("an encoded concept node decodes");
    assert_eq!(&decoded, concept);

    let item = update
        .items
        .first()
        .expect("the generated document carries a plan item");
    let encoded = serde_json::to_value(item).expect("a plan item encodes");
    let decoded: tauqe_protocol::PlanItem =
        serde_json::from_value(encoded).expect("an encoded plan item decodes");
    assert_eq!(&decoded, item);
}

#[test]
fn generated_discussion_response_decodes_into_the_wire_dto() {
    let schema = tauqe_protocol::discussion_response_schema();
    let definitions = schema
        .get("$defs")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let generated = generate_from_schema(&schema, &definitions, GENERATED_TREE_DEPTH);
    let response: tauqe_protocol::DiscussionResponse = serde_json::from_value(generated.clone())
        .expect("a document generated from the discussion schema decodes into the DTO");

    // The strict schema requires every declared property, so the decoded
    // response carries the payload of each enabled capability.
    assert_eq!(response.message, "generated");
    assert!(response.plan_update.is_some());
    assert!(!response.context_requests.is_empty());
    assert!(!response.repo_searches.is_empty());
    assert_eq!(response.repo_dirs, Some(false));
    assert!(!response.repo_maps.is_empty());
    assert!(!response.dir_lists.is_empty());
    assert_eq!(response.user_language.as_deref(), Some("generated"));

    // The same document survives a round-trip through the encoder, so the
    // generated payload is a faithful instance of the contract.
    let encoded = serde_json::to_value(&response).expect("the response encodes");
    let decoded: tauqe_protocol::DiscussionResponse =
        serde_json::from_value(encoded).expect("the encoded response decodes");
    assert_eq!(decoded, response);
}

// ---------------------------------------------------------------------------
// Exhaustiveness (schema properties and DTO fields are one set)
// ---------------------------------------------------------------------------

/// Returns the field names of a serialized DTO as a sorted list.
fn dto_fields<T: serde::Serialize>(value: &T) -> Vec<String> {
    let mut fields: Vec<String> = serde_json::to_value(value)
        .ok()
        .and_then(|encoded| encoded.as_object().cloned())
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default();
    fields.sort();
    fields
}

/// Returns the declared property names of a schema node as a sorted list.
fn schema_fields(node: &serde_json::Value) -> Vec<String> {
    let mut fields: Vec<String> = node
        .get("properties")
        .and_then(|properties| properties.as_object())
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();
    fields.sort();
    fields
}

#[test]
fn plan_definitions_declare_exactly_the_fields_of_the_rust_nodes() {
    let definitions = tauqe_protocol::discussion::schema::plan_schema_definitions();

    // Every field of the Rust node is declared by the definition, and the
    // definition declares nothing the Rust node cannot carry. This is the
    // exhaustiveness check: a field added to either side alone fails here.
    let concept = tauqe_protocol::ConceptItem {
        id: "c-1".to_string(),
        title: "Concept".to_string(),
        details: Some("details".to_string()),
        children: vec![tauqe_protocol::ConceptItem::default()],
    };
    assert_eq!(
        schema_fields(&definitions["ConceptItem"]),
        dto_fields(&concept),
        "ConceptItem definition and Rust node diverged"
    );

    let item = tauqe_protocol::PlanItem {
        id: "1".to_string(),
        title: "Step".to_string(),
        details: Some("details".to_string()),
        status: tauqe_protocol::PlanItemStatus::Todo,
        blocked_reason: None,
        files: vec!["src/a.rs".to_string()],
        readonly_files: vec!["src/b.rs".to_string()],
        concept_refs: vec!["c-1".to_string()],
        depends_on: vec!["0".to_string()],
        children: vec![tauqe_protocol::PlanItem::default()],
    };
    assert_eq!(
        schema_fields(&definitions["PlanItem"]),
        dto_fields(&item),
        "PlanItem definition and Rust node diverged"
    );

    // A strict definition requires exactly the fields it declares, so a
    // declared field can never be optional against the Rust model.
    for name in ["ConceptItem", "PlanItem"] {
        let mut required: Vec<String> = definitions[name]["required"]
            .as_array()
            .map(|required| {
                required
                    .iter()
                    .filter_map(|field| field.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        required.sort();
        assert_eq!(
            required,
            schema_fields(&definitions[name]),
            "{name} must require exactly the fields it declares"
        );
    }
}

#[test]
fn discussion_schema_declares_exactly_the_fields_of_the_response_dto() {
    let schema = tauqe_protocol::discussion_response_schema();
    let response = tauqe_protocol::DiscussionResponse {
        message: "message".to_string(),
        plan_update: Some(tauqe_protocol::DiscussionPlanUpdate::default()),
        context_requests: vec![tauqe_protocol::DiscussionContextRequest::read_only(
            "src/a.rs",
        )],
        repo_searches: vec![tauqe_protocol::RepoSearchRequest::new("src", "regex")],
        repo_dirs: Some(true),
        repo_maps: vec!["crates/core".to_string()],
        dir_lists: vec![tauqe_protocol::DirListRequest::new("crates/core", true)],
        user_language: Some("Russian".to_string()),
    };

    assert_eq!(
        schema_fields(&schema),
        dto_fields(&response),
        "discussion schema and DiscussionResponse diverged"
    );

    let mut required: Vec<String> = schema["required"]
        .as_array()
        .map(|required| {
            required
                .iter()
                .filter_map(|field| field.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    required.sort();
    assert_eq!(required, schema_fields(&schema));

    // The plan payload embedded in the discussion contract is the very
    // definition the plan contour publishes, so one shape serves both.
    assert_eq!(
        schema["properties"]["plan_update"],
        tauqe_protocol::discussion::schema::plan_update_schema()
    );
    assert_eq!(
        schema["$defs"],
        tauqe_protocol::discussion::schema::plan_schema_definitions()
    );
}

// ---------------------------------------------------------------------------
// Purity matrix (typed protocol knowledge reaches the prompt)
// ---------------------------------------------------------------------------

/// One production combination of the purity matrix.
struct Combination {
    /// Declared intent of the turn.
    intent: TurnIntent,
    /// Transport that renders and decodes the turn.
    protocol: Box<dyn PromptProtocol>,
}

impl Combination {
    /// Returns the wire format the combination serializes.
    fn kind(&self) -> EditProtocolKind {
        self.protocol.kind()
    }

    /// Assembles the system prompt of the combination.
    ///
    /// Capability narrowing happens inside the assembly, so the rendered text
    /// is exactly what the same turn would carry in production.
    fn prompt(&self) -> String {
        PromptAssembly::new(None, 1, Vec::new(), "git", self.protocol.kind(), None, None)
            .with_intent(self.intent)
            .build_system_prompt(self.protocol.as_ref())
    }
}

/// Production `intent × transport` combinations of the purity matrix.
///
/// `(ReadonlyDiscussion, xml)` is absent on purpose: the server always binds a
/// plan discussion to the structured transport, so no XML discussion body
/// exists in production.
fn purity_matrix() -> Vec<Combination> {
    vec![
        Combination {
            intent: TurnIntent::InteractiveRepl,
            protocol: Box::new(XmlPromptProtocol),
        },
        Combination {
            intent: TurnIntent::InteractiveRepl,
            protocol: Box::new(JsonPromptProtocol::edits()),
        },
        Combination {
            intent: TurnIntent::IsolatedStep,
            protocol: Box::new(XmlPromptProtocol),
        },
        Combination {
            intent: TurnIntent::IsolatedStep,
            protocol: Box::new(JsonPromptProtocol::edits()),
        },
        Combination {
            intent: TurnIntent::ReadonlyDiscussion,
            protocol: Box::new(JsonPromptProtocol::discussion()),
        },
    ]
}

/// Returns the contract and protocol-instruction body of a system prompt.
///
/// The section starts at the `Core System Contract` heading and ends right
/// before the project metadata, so it covers exactly the rules and the protocol
/// instructions that describe the wire surface to the model.
fn contract_and_instruction_section(prompt: &str) -> &str {
    let start = prompt
        .find("## Core System Contract")
        .expect("every system prompt carries the core contract");
    let tail = &prompt[start..];
    let end = tail
        .find("## Project Metadata")
        .expect("every system prompt describes its surface before the metadata");
    &tail[..end]
}

#[test]
fn structured_prompts_never_advertise_an_operational_tag() {
    let mut checked = 0;
    for combination in &purity_matrix() {
        if !matches!(combination.kind(), EditProtocolKind::Structured) {
            continue;
        }
        checked += 1;
        let prompt = combination.prompt();
        for tag in OPERATIONAL_TAGS {
            assert!(
                !prompt.contains(tag),
                "{:?} advertises {tag}",
                combination.intent
            );
        }
        // The check is scoped to protocol tags: the prompt-side blocks stay.
        assert!(prompt.contains("<context>"));
    }
    assert_eq!(checked, 3, "the matrix lost a structured combination");
}

#[test]
fn xml_prompts_never_name_a_structured_schema_field() {
    let fields = structured_property_names();
    assert!(fields.contains(&"plan_update".to_string()));

    let mut checked = 0;
    for combination in &purity_matrix() {
        if !matches!(combination.kind(), EditProtocolKind::Xml) {
            continue;
        }
        checked += 1;
        let prompt = combination.prompt();
        let section = contract_and_instruction_section(&prompt);
        assert!(section.contains("## Code Modification Protocol (XML Edits)"));
        for field in &fields {
            let quoted = format!("`{field}`");
            assert!(
                !section.contains(&quoted),
                "{:?} names {quoted}",
                combination.intent
            );
        }
    }
    assert_eq!(checked, 2, "the matrix lost an xml combination");
}

// ---------------------------------------------------------------------------
// Parity matrix (capability surface, schema projection, decoding)
// ---------------------------------------------------------------------------

/// Narrows `protocol` to the surface its intent permits.
fn narrowed_surface(protocol: &dyn PromptProtocol, intent: TurnIntent) -> CapabilitySet {
    let narrowed = intent.narrow_capabilities(&protocol.capability_set());
    protocol.with_capability_set(narrowed).capability_set()
}

/// Returns the top-level property names of a composed response schema.
fn property_names(schema: &serde_json::Value) -> Vec<String> {
    schema["properties"]
        .as_object()
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default()
}

/// Returns the `required` entries of a composed response schema.
fn required_names(schema: &serde_json::Value) -> Vec<String> {
    schema["required"]
        .as_array()
        .map(|required| {
            required
                .iter()
                .filter_map(|field| field.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Returns the properties the enabled capability modules declare.
fn expected_properties(surface: &CapabilitySet) -> Vec<String> {
    let registry = CapabilityRegistry::global();
    let mut names: Vec<String> = Vec::new();
    for capability in surface.iter() {
        let Some(module) = registry.module(*capability) else {
            continue;
        };
        let Some(fragment) = module.json_schema_fragment() else {
            continue;
        };
        let Some(fields) = fragment.as_object() else {
            continue;
        };
        for name in fields.keys() {
            if !names.iter().any(|existing| existing == name) {
                names.push(name.clone());
            }
        }
    }
    names.sort();
    names
}

/// Returns the top-level properties of the fully advertised structured schema.
fn structured_property_names() -> Vec<String> {
    let schema = CapabilityRegistry::global()
        .compose_structured_schema(&CapabilitySet::from_slice(LlmCapability::ALL));
    property_names(&schema)
}

#[test]
fn both_transports_narrow_to_the_same_capability_surface() {
    let xml = XmlPromptProtocol;
    let structured = JsonPromptProtocol::edits();
    for intent in ALL_INTENTS {
        let xml_surface = narrowed_surface(&xml, intent);
        let structured_surface = narrowed_surface(&structured, intent);
        assert_eq!(xml_surface, structured_surface, "{intent:?} surface");
    }
}

#[test]
fn the_discussion_transport_binds_the_discussion_surface() {
    let discussion = JsonPromptProtocol::discussion();
    let surface = narrowed_surface(&discussion, TurnIntent::ReadonlyDiscussion);

    // A plan discussion always runs on the structured transport, and the
    // narrowing leaves its specialized surface untouched.
    assert_eq!(surface, CapabilitySet::discussion());
    assert!(!surface.contains(LlmCapability::FileEdits));
}

#[test]
fn structured_schema_is_the_projection_of_the_narrowed_surface() {
    let registry = CapabilityRegistry::global();
    let structured = JsonPromptProtocol::edits();

    for intent in ALL_INTENTS {
        let surface = narrowed_surface(&structured, intent);
        let schema = registry.compose_structured_schema(&surface);

        let mut declared = property_names(&schema);
        declared.sort();
        assert_eq!(declared, expected_properties(&surface), "{intent:?} schema");

        let mut required = required_names(&schema);
        required.sort();
        assert_eq!(required, declared, "{intent:?} strict schema");

        assert_eq!(schema["additionalProperties"], serde_json::json!(false));

        // A nested plan payload travels as shared definitions; a surface that
        // cannot author plans carries no definition to lift.
        let authors_plans = surface.contains(LlmCapability::PlanCreation)
            || surface.contains(LlmCapability::PlanUpdate);
        assert_eq!(schema.get("$defs").is_some(), authors_plans);
    }
}

#[test]
fn the_plan_abort_and_custom_tool_fields_follow_the_surface() {
    let registry = CapabilityRegistry::global();
    let structured = JsonPromptProtocol::edits();

    let repl = registry
        .compose_structured_schema(&narrowed_surface(&structured, TurnIntent::InteractiveRepl));
    for field in ["plan_update", "abort", "custom_tools"] {
        assert!(
            repl["properties"].get(field).is_some(),
            "repl lacks {field}"
        );
    }

    let step = registry
        .compose_structured_schema(&narrowed_surface(&structured, TurnIntent::IsolatedStep));
    assert!(step["properties"].get("plan_update").is_none());
    assert!(step["properties"].get("abort").is_some());

    let discussion = registry.compose_structured_schema(&narrowed_surface(
        &structured,
        TurnIntent::ReadonlyDiscussion,
    ));
    assert!(discussion["properties"].get("plan_update").is_some());
    assert!(discussion["properties"].get("changes").is_none());
    assert!(discussion["properties"].get("abort").is_none());
}

/// One payload expressed in both wire formats with identical semantics.
struct PayloadPair {
    /// Diagnostic name of the payload.
    name: &'static str,
    /// Payload serialized as XML control tags and edit blocks.
    xml: String,
    /// Payload serialized as a structured response object.
    structured: String,
}

/// Returns the equivalent payloads of the decode-parity matrix.
///
/// The terminal outcome and the semantic blocker travel in separate payloads
/// because a single XML text carries exactly one terminal outcome, so both
/// vehicles stay faithful to what each transport can express.
fn equivalent_payloads() -> Vec<PayloadPair> {
    vec![
        PayloadPair {
            name: "success",
            xml: r#"Applying the change.

<edits summary="Fix the bug">
  <edit path="src/a.rs">
    <search>old();</search>
    <replace>new();</replace>
  </edit>
</edits>

<context_request path="src/a.rs" access="editable" />
<repo_search path="crates/core" regex="fn main" />
<doc_request topic="git" />
<verify target="check" on_success="silent" />
<plan action="save" id="p1" title="Plan"><concept><concept_item id="c-1" title="Concept" /></concept><item id="1" title="Step" status="todo" /></plan>
<scratchpad>Verified: parser works.</scratchpad>
<custom_tool name="check" />
<abort reason="Toolchain missing" recommendation="rustup component add clippy" />
<user_language>Russian</user_language>
<success summary="All checks pass" />
"#
            .to_string(),
            structured: r#"{
  "message": "Applying the change.",
  "changes": [
    {"op": "replace", "path": "src/a.rs", "old_text": "old();", "new_text": "new();"}
  ],
  "context_requests": ["editable:src/a.rs"],
  "repo_searches": [{"path": "crates/core", "regex": "fn main"}],
  "doc_requests": ["git"],
  "verify": {"target": "check", "on_success": "silent"},
  "plan_update": {
    "action": "save",
    "id": "p1",
    "title": "Plan",
    "concept": [{"id": "c-1", "title": "Concept"}],
    "items": [{"id": "1", "title": "Step", "status": "todo"}]
  },
  "scratchpad": "Verified: parser works.",
  "outcome": {"status": "success", "summary": "All checks pass"},
  "abort": {
    "is_aborted": true,
    "reason": "Toolchain missing",
    "recommendation": "rustup component add clippy"
  },
  "user_language": "Russian",
  "custom_tools": [{"name": "check"}]
}"#
            .to_string(),
        },
        PayloadPair {
            name: "blocked",
            xml: r#"The precondition failed.

<blocked reason="Precondition failed" details="missing file" />
"#
            .to_string(),
            structured: r#"{
  "message": "The precondition failed.",
  "blocked": {
    "is_blocked": true,
    "reason": "Precondition failed",
    "details": "missing file"
  }
}"#
            .to_string(),
        },
    ]
}

/// Serializes decoded commands into sorted, comparable signatures.
fn signatures(commands: &[LlmCommand]) -> Vec<String> {
    let mut rendered: Vec<String> = commands
        .iter()
        .map(|command| serde_json::to_string(command).expect("a decoded command serializes"))
        .collect();
    rendered.sort();
    rendered
}

/// Decodes every equivalent payload of `intent` on `protocol`.
fn decode_payloads(protocol: &dyn PromptProtocol, intent: TurnIntent) -> Vec<LlmCommand> {
    let narrowed = intent.narrow_capabilities(&protocol.capability_set());
    let scoped = protocol.with_capability_set(narrowed);
    let xml = matches!(protocol.kind(), EditProtocolKind::Xml);
    equivalent_payloads()
        .into_iter()
        .flat_map(|payload| {
            let raw = if xml { payload.xml } else { payload.structured };
            scoped.decode(&raw, &[])
        })
        .collect()
}

/// Collects the sorted, de-duplicated capability names behind decoded commands.
fn capability_names(commands: &[LlmCommand]) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = commands
        .iter()
        .filter_map(|command| command.capability())
        .map(LlmCapability::as_str)
        .collect();
    names.sort();
    names.dedup();
    names
}

#[test]
fn equivalent_payloads_decode_to_the_same_commands() {
    let xml = XmlPromptProtocol;
    let structured = JsonPromptProtocol::edits();
    for intent in ALL_INTENTS {
        let xml_surface = intent.narrow_capabilities(&xml.capability_set());
        let structured_surface = intent.narrow_capabilities(&structured.capability_set());
        let xml_protocol = xml.with_capability_set(xml_surface);
        let structured_protocol = structured.with_capability_set(structured_surface);
        for payload in equivalent_payloads() {
            let from_xml = signatures(&xml_protocol.decode(&payload.xml, &[]));
            let from_structured = signatures(&structured_protocol.decode(&payload.structured, &[]));
            assert_eq!(from_xml, from_structured, "{} {intent:?}", payload.name);
        }
    }
}

#[test]
fn intent_narrowing_filters_the_same_commands_on_both_transports() {
    let xml = XmlPromptProtocol;
    let structured = JsonPromptProtocol::edits();

    for intent in ALL_INTENTS {
        let from_xml = decode_payloads(&xml, intent);
        let from_structured = decode_payloads(&structured, intent);
        assert_eq!(
            signatures(&from_xml),
            signatures(&from_structured),
            "{intent:?}"
        );
        assert!(!from_xml
            .iter()
            .any(|command| matches!(command, LlmCommand::ProtocolError { .. })));
        assert_eq!(
            capability_names(&from_xml),
            capability_names(&from_structured)
        );
    }

    // An interactive dialogue reaches the whole operational surface, plan
    // creation included.
    let repl = decode_payloads(&xml, TurnIntent::InteractiveRepl);
    assert_eq!(
        capability_names(&repl),
        vec![
            "abort",
            "blocked",
            "custom_tools",
            "discovery",
            "doc_request",
            "file_edits",
            "outcome",
            "plan_creation",
            "scratchpad",
            "text_message",
            "toolchain_verify",
            "user_language",
        ]
    );

    // The plan is immutable and verification is harness-owned while a step
    // runs, so neither surface may reach the model there.
    let step = decode_payloads(&xml, TurnIntent::IsolatedStep);
    for command in &step {
        assert!(
            !matches!(command, LlmCommand::Plan { .. }),
            "step authored a plan"
        );
        assert!(
            !matches!(command, LlmCommand::Verify { .. }),
            "step verified"
        );
    }
    assert_eq!(
        capability_names(&step),
        vec![
            "abort",
            "blocked",
            "custom_tools",
            "discovery",
            "doc_request",
            "file_edits",
            "outcome",
            "scratchpad",
            "text_message",
            "user_language",
        ]
    );

    // A read-only discussion implements nothing and verifies nothing.
    let discussion = decode_payloads(&xml, TurnIntent::ReadonlyDiscussion);
    assert_eq!(
        capability_names(&discussion),
        vec![
            "discovery",
            "plan_creation",
            "text_message",
            "user_language"
        ]
    );
}

// ---------------------------------------------------------------------------
// Command translation parity (plan creation and file edits)
// ---------------------------------------------------------------------------

/// Returns the plan payload a decoded command list carries.
fn plan_payload(commands: &[LlmCommand]) -> tauqe_protocol::DiscussionPlanUpdate {
    commands
        .iter()
        .find_map(|command| match command {
            LlmCommand::Plan { update } => Some(update.clone()),
            _ => None,
        })
        .expect("the payload authors a plan")
}

/// Returns the file edit operations a decoded command list carries, in order.
fn edit_operations(commands: &[LlmCommand]) -> Vec<tauqe_protocol::EditOperation> {
    commands
        .iter()
        .filter_map(|command| match command {
            LlmCommand::Edit { operation } => Some(operation.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn plan_creation_and_file_edits_translate_identically_on_both_transports() {
    let payload = equivalent_payloads()
        .into_iter()
        .find(|payload| payload.name == "success")
        .expect("the parity matrix carries the editing payload");

    let xml = XmlPromptProtocol;
    let structured = JsonPromptProtocol::edits();
    let intent = TurnIntent::InteractiveRepl;
    let xml_protocol = xml.with_capability_set(intent.narrow_capabilities(&xml.capability_set()));
    let structured_protocol =
        structured.with_capability_set(intent.narrow_capabilities(&structured.capability_set()));

    let from_xml = xml_protocol.decode(&payload.xml, &[]);
    let from_structured = structured_protocol.decode(&payload.structured, &[]);

    // Plan creation: both transports project one semantic plan payload, so the
    // action, the identifier, the concept node, and the step all reach the
    // engine identically regardless of the encoding that carried them.
    let xml_plan = plan_payload(&from_xml);
    assert_eq!(xml_plan, plan_payload(&from_structured));
    assert_eq!(xml_plan.action, tauqe_protocol::DiscussionPlanAction::Save);
    assert_eq!(xml_plan.id.as_deref(), Some("p1"));
    assert_eq!(xml_plan.concept.len(), 1);
    assert_eq!(xml_plan.items.len(), 1);

    // File edits: the same ordered replace operation reaches the engine.
    let xml_edits = edit_operations(&from_xml);
    assert_eq!(xml_edits, edit_operations(&from_structured));
    match xml_edits.as_slice() {
        [tauqe_protocol::EditOperation::Replace {
            path,
            old_text,
            new_text,
        }] => {
            assert_eq!(path, "src/a.rs");
            assert_eq!(old_text, "old();");
            assert_eq!(new_text, "new();");
        }
        other => panic!("expected one replace operation, got {other:?}"),
    }
}
