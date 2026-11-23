//! Exact, branch-scoped permissions for speculative structured-output previews.
//!
//! These routes describe where a preview may look, not whether a response is
//! valid. The final decoder remains the authority for commands and edits.

use crate::edits::stream::json::events::PathPart;
use crate::edits::EditFileOp;
use crate::llm::{CapabilityTree, LlmCapability};
use crate::workflow::PatchStrategy;

/// A discriminator that must select a branch before its route is previewed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreviewCondition {
    pub path: Vec<PathPart>,
    pub expected: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreviewRouteKind {
    Text,
    Changes { allowed_ops: Vec<EditFileOp> },
}

/// One field of the composed schema, with its full chain of branch permissions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreviewRoute {
    pub path: Vec<PathPart>,
    pub conditions: Vec<PreviewCondition>,
    pub kind: PreviewRouteKind,
}

fn child_path(path: &[PathPart], key: &str) -> Vec<PathPart> {
    let mut child = path.to_vec();
    child.push(PathPart::Key(key.to_owned()));
    child
}

fn edit_op(cap: LlmCapability, strategy: PatchStrategy) -> Option<EditFileOp> {
    if strategy == PatchStrategy::ApplyPatch {
        return None;
    }
    match cap {
        LlmCapability::EditReplace => Some(EditFileOp::Replace),
        LlmCapability::EditCreate => Some(EditFileOp::Create),
        LlmCapability::EditDelete => Some(EditFileOp::Delete),
        LlmCapability::EditMove => Some(EditFileOp::Move),
        LlmCapability::EditMoveDir => Some(EditFileOp::MoveDir),
        LlmCapability::EditOverwrite if strategy != PatchStrategy::LineAnchor => {
            Some(EditFileOp::Overwrite)
        }
        _ => None,
    }
}

fn collect(
    tree: &CapabilityTree,
    path: &[PathPart],
    conditions: &[PreviewCondition],
    strategy: PatchStrategy,
    routes: &mut Vec<PreviewRoute>,
) {
    match tree {
        CapabilityTree::Leaf(LlmCapability::TextMessage) => {
            let field = child_path(path, "message");
            if !routes.iter().any(|route| {
                route.path == field
                    && route.conditions == conditions
                    && route.kind == PreviewRouteKind::Text
            }) {
                routes.push(PreviewRoute {
                    path: field,
                    conditions: conditions.to_vec(),
                    kind: PreviewRouteKind::Text,
                });
            }
        }
        CapabilityTree::Leaf(cap) => {
            if let Some(op) = edit_op(*cap, strategy) {
                let field = child_path(path, "changes");
                if let Some(PreviewRoute {
                    kind: PreviewRouteKind::Changes { allowed_ops },
                    ..
                }) = routes.iter_mut().find(|route| {
                    route.path == field
                        && route.conditions == conditions
                        && matches!(route.kind, PreviewRouteKind::Changes { .. })
                }) {
                    if !allowed_ops.contains(&op) {
                        allowed_ops.push(op);
                    }
                } else {
                    routes.push(PreviewRoute {
                        path: field,
                        conditions: conditions.to_vec(),
                        kind: PreviewRouteKind::Changes {
                            allowed_ops: vec![op],
                        },
                    });
                }
            }
        }
        CapabilityTree::Set(_, items) => {
            for item in items {
                collect(item, path, conditions, strategy, routes);
            }
        }
        CapabilityTree::AnyOf(label, branches) => {
            let named = branches
                .iter()
                .filter(|branch| branch.label().is_some_and(|name| !name.is_empty()))
                .count();
            for branch in branches {
                let Some(name) = branch.label().filter(|name| !name.is_empty()) else {
                    collect(branch, path, conditions, strategy, routes);
                    continue;
                };
                if named < 2 {
                    collect(branch, path, conditions, strategy, routes);
                } else {
                    let branch_path = child_path(path, label);
                    let mut guarded = conditions.to_vec();
                    guarded.push(PreviewCondition {
                        path: child_path(&branch_path, "kind"),
                        expected: name.to_owned(),
                    });
                    collect(branch, &branch_path, &guarded, strategy, routes);
                }
            }
        }
    }
}

/// Project previewable fields from the same tree and strategy as the wire schema.
/// No route is added for a field that the tree does not declare.
pub(crate) fn preview_routes(tree: &CapabilityTree, strategy: PatchStrategy) -> Vec<PreviewRoute> {
    let mut routes = Vec::new();
    collect(tree, &[], &[], strategy, &mut routes);
    routes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{capability_expr_schema, PromptRenderContext};
    use serde_json::Value;

    fn leaf(cap: LlmCapability) -> CapabilityTree {
        CapabilityTree::leaf(cap)
    }

    // Walk the actual composed schema, not a separately reconstructed shape.
    fn assert_in_schema(schema: &Value, route: &PreviewRoute) {
        let mut current = schema;
        let mut prefix = Vec::new();
        for part in &route.path {
            let PathPart::Key(key) = part else {
                panic!("field routes only contain keys");
            };
            if let Some(guard) = route.conditions.iter().find(|guard| {
                guard.path == {
                    let mut kind_path = prefix.clone();
                    kind_path.push(PathPart::Key("kind".into()));
                    kind_path
                }
            }) {
                current = current["anyOf"]
                    .as_array()
                    .expect("branch union")
                    .iter()
                    .find(|variant| variant["properties"]["kind"]["enum"][0] == guard.expected)
                    .expect("selected branch");
            }
            current = &current["properties"][key];
            assert!(!current.is_null(), "missing field {key} in {route:?}");
            prefix.push(part.clone());
        }
        match &route.kind {
            PreviewRouteKind::Text => assert_eq!(current["type"], "string"),
            PreviewRouteKind::Changes { allowed_ops } => {
                assert_eq!(current["type"], "array");
                let names = current["items"]["properties"]["op"]["enum"]
                    .as_array()
                    .expect("operation enum");
                for op in allowed_ops {
                    assert!(names.iter().any(|name| name == &op.to_string()));
                }
            }
        }
    }

    fn check(tree: &CapabilityTree, strategy: PatchStrategy) -> Vec<PreviewRoute> {
        let ctx = PromptRenderContext::new(&[], None).with_patch_strategy(strategy);
        let schema = capability_expr_schema(tree, &ctx);
        let routes = preview_routes(tree, strategy);
        for route in &routes {
            assert_in_schema(&schema, route);
        }
        routes
    }

    #[test]
    fn flat_and_transparent_unions_share_only_declared_fields() {
        let flat = CapabilityTree::root([
            leaf(LlmCapability::TextMessage),
            leaf(LlmCapability::EditCreate),
        ]);
        let routes = check(&flat, PatchStrategy::SearchReplace);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].path, vec![PathPart::Key("message".into())]);
        assert_eq!(
            routes[1].kind,
            PreviewRouteKind::Changes {
                allowed_ops: vec![EditFileOp::Create]
            }
        );

        let transparent = CapabilityTree::any_of(
            "action",
            [CapabilityTree::named_set(
                "only",
                [leaf(LlmCapability::EditDelete)],
            )],
        );
        let routes = check(&transparent, PatchStrategy::SearchReplace);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path, vec![PathPart::Key("changes".into())]);
        assert!(routes[0].conditions.is_empty());
    }

    #[test]
    fn nested_branches_keep_their_own_discriminators_and_operations() {
        let tree = CapabilityTree::any_of(
            "action",
            [
                CapabilityTree::named_set(
                    "edit",
                    [CapabilityTree::any_of(
                        "choice",
                        [
                            CapabilityTree::named_set("create", [leaf(LlmCapability::EditCreate)]),
                            CapabilityTree::named_set("delete", [leaf(LlmCapability::EditDelete)]),
                        ],
                    )],
                ),
                CapabilityTree::named_set("answer", []),
            ],
        );
        let routes = check(&tree, PatchStrategy::SearchReplace);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].path, key_path(&["action", "choice", "changes"]));
        assert_eq!(
            routes[0].conditions,
            vec![
                PreviewCondition {
                    path: key_path(&["action", "kind"]),
                    expected: "edit".into()
                },
                PreviewCondition {
                    path: key_path(&["action", "choice", "kind"]),
                    expected: "create".into()
                },
            ]
        );
        assert_eq!(
            routes[0].kind,
            PreviewRouteKind::Changes {
                allowed_ops: vec![EditFileOp::Create]
            }
        );
        assert_eq!(
            routes[1].kind,
            PreviewRouteKind::Changes {
                allowed_ops: vec![EditFileOp::Delete]
            }
        );
    }

    #[test]
    fn same_path_does_not_merge_permissions_across_branches() {
        let tree = CapabilityTree::any_of(
            "action",
            [
                CapabilityTree::named_set("create", [leaf(LlmCapability::EditCreate)]),
                CapabilityTree::named_set("delete", [leaf(LlmCapability::EditDelete)]),
            ],
        );
        let routes = check(&tree, PatchStrategy::SearchReplace);
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].path, routes[1].path);
        assert_ne!(routes[0].conditions, routes[1].conditions);
        assert_eq!(
            routes[0].kind,
            PreviewRouteKind::Changes {
                allowed_ops: vec![EditFileOp::Create]
            }
        );
        assert_eq!(
            routes[1].kind,
            PreviewRouteKind::Changes {
                allowed_ops: vec![EditFileOp::Delete]
            }
        );
    }

    #[test]
    fn strategy_and_text_capability_bound_the_projection() {
        let tree = CapabilityTree::root([
            leaf(LlmCapability::EditOverwrite),
            leaf(LlmCapability::EditReplace),
        ]);
        let anchored = check(&tree, PatchStrategy::LineAnchor);
        assert_eq!(anchored.len(), 1);
        assert_eq!(
            anchored[0].kind,
            PreviewRouteKind::Changes {
                allowed_ops: vec![EditFileOp::Replace]
            }
        );
        assert!(!anchored
            .iter()
            .any(|route| route.kind == PreviewRouteKind::Text));
        assert!(check(&tree, PatchStrategy::ApplyPatch).is_empty());

        let text = check(
            &CapabilityTree::root([leaf(LlmCapability::TextMessage)]),
            PatchStrategy::ApplyPatch,
        );
        assert_eq!(text.len(), 1);
        assert_eq!(text[0].path, key_path(&["message"]));
    }

    use crate::edits::stream::json::events::key_path;
}
