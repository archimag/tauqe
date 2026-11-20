//! Exact-path routing of speculative json_skimmer events.

use json_skimmer::{Event, Mode, Parser, StringKind};

use crate::capabilities::{PreviewCondition, PreviewRoute, PreviewRouteKind};
use crate::edits::EditFileOp;

use super::string::StringSegments;

/// Addressed preview data from one structured response attempt.
pub enum PreviewUpdate {
    Text(String),
    Kind(String),
    Header {
        route: usize,
        start: usize,
        op: String,
        path: String,
    },
    Change {
        route: usize,
        start: usize,
        end: usize,
        op: Option<String>,
        path: Option<String>,
    },
    ArrayEnd {
        route: usize,
    },
    Invalid,
}

/// Parser state is preview telemetry, not acceptance of the model response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewState {
    Incomplete,
    CandidateEnd,
    Invalid,
}

struct ChangeHeader {
    start: usize,
    path_parts: Vec<PathPart>,
    op: Option<String>,
    path: Option<String>,
    announced: bool,
}

struct Discriminator {
    path: Vec<PathPart>,
    // None means undecided; Some(None) means absent or non-string.
    value: Option<Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathPart {
    Key(String),
    Index(usize),
}

pub fn key_path(keys: &[&str]) -> Vec<PathPart> {
    keys.iter()
        .map(|key| PathPart::Key((*key).into()))
        .collect()
}

enum Container {
    Object {
        path: Vec<PathPart>,
        key: Option<String>,
    },
    Array {
        path: Vec<PathPart>,
        next: usize,
    },
}

struct Router {
    stack: Vec<Container>,
    string: Option<StringSegments>,
    string_path: Option<Vec<PathPart>>,
    routes: Vec<PreviewRoute>,
    selected: bool,
    invalid: bool,
    key_text: String,
    value_text: String,
    discriminators: Vec<Discriminator>,
    pending: Vec<(Vec<usize>, PreviewUpdate)>,
    candidate_end: bool,
    change: Option<ChangeHeader>,
}

impl Router {
    fn permission(&self, route: usize) -> Option<bool> {
        let mut undecided = false;
        for condition in &self.routes[route].conditions {
            match self
                .discriminators
                .iter()
                .find(|item| item.path == condition.path)
                .and_then(|item| item.value.as_ref())
            {
                Some(Some(value)) if value == &condition.expected => {}
                Some(_) => return Some(false),
                None => undecided = true,
            }
        }
        if undecided {
            None
        } else {
            Some(true)
        }
    }

    fn route_ids(&self, path: &[PathPart], op: Option<&str>, text: bool) -> Vec<usize> {
        self.routes
            .iter()
            .enumerate()
            .filter_map(|(id, route)| {
                if route.path != path {
                    return None;
                }
                match (&route.kind, text) {
                    (PreviewRouteKind::Text, true) => Some(id),
                    (PreviewRouteKind::Changes { allowed_ops }, false)
                        if op.is_none_or(|name| {
                            name.parse::<EditFileOp>()
                                .is_ok_and(|op| allowed_ops.contains(&op))
                        }) =>
                    {
                        Some(id)
                    }
                    _ => None,
                }
            })
            .collect()
    }

    fn emit(&mut self, ids: Vec<usize>, update: PreviewUpdate, updates: &mut Vec<PreviewUpdate>) {
        if !ids.is_empty() {
            self.pending.push((ids, update));
            self.resolve_pending(updates);
        }
    }

    fn resolve_pending(&mut self, updates: &mut Vec<PreviewUpdate>) {
        // An undecided earlier event must not be overtaken by a later one.
        while let Some((ids, _)) = self.pending.first() {
            let chosen = ids
                .iter()
                .copied()
                .find(|id| self.permission(*id) == Some(true));
            if chosen.is_none() && ids.iter().any(|id| self.permission(*id).is_none()) {
                break;
            }
            let (_, mut update) = self.pending.remove(0);
            if let Some(id) = chosen {
                match &mut update {
                    PreviewUpdate::Header { route, .. }
                    | PreviewUpdate::Change { route, .. }
                    | PreviewUpdate::ArrayEnd { route } => *route = id,
                    _ => {}
                }
                updates.push(update);
            }
        }
    }

    fn invalidate(&mut self, updates: &mut Vec<PreviewUpdate>) {
        self.invalid = true;
        self.pending.clear();
        updates.push(PreviewUpdate::Invalid);
    }

    fn header_property(&self, path: &[PathPart]) -> bool {
        self.change.as_ref().is_some_and(|change| {
            path.len() == change.path_parts.len() + 1
                && path.starts_with(&change.path_parts)
                && matches!(path.last(), Some(PathPart::Key(key)) if key == "op" || key == "path")
        })
    }

    fn change_index(&self, path: &[PathPart]) -> bool {
        self.routes.iter().any(|route| {
            matches!(route.kind, PreviewRouteKind::Changes { .. })
                && path.len() == route.path.len() + 1
                && path.starts_with(&route.path)
                && matches!(path.last(), Some(PathPart::Index(_)))
        })
    }

    fn update_header(&mut self, updates: &mut Vec<PreviewUpdate>) {
        if let Some(change) = self.change.as_mut() {
            if !change.announced {
                if let (Some(op), Some(path)) = (&change.op, &change.path) {
                    change.announced = true;
                    let header = (
                        change.path_parts[..change.path_parts.len() - 1].to_vec(),
                        change.start,
                        op.clone(),
                        path.clone(),
                    );
                    let (array_path, start, op, path) = header;
                    let ids = self.route_ids(&array_path, Some(&op), false);
                    self.emit(
                        ids,
                        PreviewUpdate::Header {
                            route: 0,
                            start,
                            op,
                            path,
                        },
                        updates,
                    );
                }
            }
        }
    }

    fn next_path(&mut self) -> Vec<PathPart> {
        match self.stack.last_mut() {
            Some(Container::Object { path, key }) => {
                let mut result = path.clone();
                if let Some(key) = key.take() {
                    result.push(PathPart::Key(key));
                }
                result
            }
            Some(Container::Array { path, next }) => {
                let mut result = path.clone();
                result.push(PathPart::Index(*next));
                *next += 1;
                result
            }
            None => Vec::new(),
        }
    }

    fn set_discriminator(
        &mut self,
        path: &[PathPart],
        value: Option<String>,
        updates: &mut Vec<PreviewUpdate>,
    ) {
        let mut matched = false;
        for item in &mut self.discriminators {
            if item.path == path {
                item.value = Some(value.clone());
                matched = true;
            }
        }
        if matched {
            if let Some(value) = value {
                updates.push(PreviewUpdate::Kind(value));
            }
            self.resolve_pending(updates);
        }
    }

    fn close_discriminators(&mut self, owner: &[PathPart], updates: &mut Vec<PreviewUpdate>) {
        let mut changed = false;
        for item in &mut self.discriminators {
            if item.path.len() == owner.len() + 1
                && item.path.starts_with(owner)
                && item.value.is_none()
            {
                item.value = Some(None);
                changed = true;
            }
        }
        if changed {
            self.resolve_pending(updates);
        }
    }

    fn event(&mut self, event: Event<'_>, updates: &mut Vec<PreviewUpdate>) {
        if self.invalid {
            return;
        }
        match event {
            Event::Invalid(_) => self.invalidate(updates),
            Event::ObjectStart { offset } => {
                let path = self.next_path();
                self.set_discriminator(&path, None, updates);
                if self.change_index(&path) {
                    self.change = Some(ChangeHeader {
                        start: offset as usize,
                        path_parts: path.clone(),
                        op: None,
                        path: None,
                        announced: false,
                    });
                }
                self.stack.push(Container::Object { path, key: None });
            }
            Event::ArrayStart { .. } => {
                let path = self.next_path();
                self.set_discriminator(&path, None, updates);
                self.stack.push(Container::Array { path, next: 0 });
            }
            Event::ObjectEnd { offset } => {
                if let Some(Container::Object { path, .. }) = self.stack.last() {
                    self.close_discriminators(&path.clone(), updates);
                }
                if self.stack.last().is_some_and(|container| match container {
                    Container::Object { path, .. } => self.change_index(path),
                    Container::Array { .. } => false,
                }) {
                    if let Some(change) = self.change.take() {
                        let ids = self.route_ids(
                            &change.path_parts[..change.path_parts.len() - 1],
                            change.op.as_deref(),
                            false,
                        );
                        self.emit(
                            ids,
                            PreviewUpdate::Change {
                                route: 0,
                                start: change.start,
                                end: offset as usize + 1,
                                op: change.op,
                                path: change.path,
                            },
                            updates,
                        );
                    }
                }
                self.stack.pop();
            }
            Event::ArrayEnd { .. } => {
                if let Some(Container::Array { path, .. }) = self.stack.last() {
                    let ids = self.route_ids(path, None, false);
                    self.emit(ids, PreviewUpdate::ArrayEnd { route: 0 }, updates);
                }
                self.stack.pop();
            }
            Event::StringStart { kind, .. } => {
                self.string_path = (kind == StringKind::Value).then(|| self.next_path());
                if kind == StringKind::Key {
                    self.key_text.clear();
                }
                let relevant = kind == StringKind::Key
                    || self.string_path.as_ref().is_some_and(|path| {
                        !self.route_ids(path, None, true).is_empty()
                            || self.discriminators.iter().any(|item| path == &item.path)
                            || self.header_property(path)
                    });
                self.string = relevant.then(StringSegments::default);
                self.value_text.clear();
            }
            Event::StringFragment { kind, bytes, .. } => {
                if let Some(string) = self.string.as_mut() {
                    match string.push(bytes) {
                        Ok(text) if !text.is_empty() => {
                            if kind == StringKind::Key {
                                // Keys are accumulated until StringEnd; incomplete keys
                                // must never influence path selection.
                                self.key_text.push_str(&text);
                            } else {
                                if let Some(path) = self.string_path.clone() {
                                    if !self.selected {
                                        let ids = self.route_ids(&path, None, true);
                                        self.emit(ids, PreviewUpdate::Text(text.clone()), updates);
                                    }
                                    if self.discriminators.iter().any(|item| item.path == path)
                                        || self.header_property(&path)
                                    {
                                        self.value_text.push_str(&text);
                                    }
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(_) => self.invalidate(updates),
                    }
                }
            }
            Event::StringEnd { kind, .. } => {
                if self
                    .string
                    .as_ref()
                    .is_some_and(|string| string.finish().is_err())
                {
                    self.invalidate(updates);
                    return;
                }
                if kind == StringKind::Value {
                    if let Some(path) = self.string_path.clone() {
                        if self.discriminators.iter().any(|item| item.path == path) {
                            self.set_discriminator(&path, Some(self.value_text.clone()), updates);
                        }
                        if let Some(change) = self.change.as_mut() {
                            if path.len() == change.path_parts.len() + 1
                                && path.starts_with(&change.path_parts)
                            {
                                if matches!(path.last(), Some(PathPart::Key(key)) if key == "op") {
                                    change.op = Some(self.value_text.clone());
                                } else if matches!(path.last(), Some(PathPart::Key(key)) if key == "path")
                                {
                                    change.path = Some(self.value_text.clone());
                                }
                            }
                        }
                        self.update_header(updates);
                    }
                }
                if kind == StringKind::Value
                    && self.string_path.as_ref().is_some_and(|path| {
                        self.route_ids(path, None, true)
                            .iter()
                            .any(|id| self.permission(*id) == Some(true))
                    })
                {
                    self.selected = true;
                }
                self.string = None;
                self.string_path = None;
                self.value_text.clear();
                if kind == StringKind::Key {
                    if let Some(Container::Object { key, .. }) = self.stack.last_mut() {
                        *key = Some(std::mem::take(&mut self.key_text));
                    }
                }
            }
            Event::NumberStart { .. } | Event::Boolean { .. } | Event::Null { .. } => {
                let path = self.next_path();
                self.set_discriminator(&path, None, updates);
                if let Some(change) = self.change.as_mut() {
                    if path.len() == change.path_parts.len() + 1
                        && path.starts_with(&change.path_parts)
                    {
                        match path.last() {
                            Some(PathPart::Key(key)) if key == "op" => change.op = None,
                            Some(PathPart::Key(key)) if key == "path" => change.path = None,
                            _ => {}
                        }
                    }
                }
                self.resolve_pending(updates);
            }
            Event::CandidateEnd { .. } => self.candidate_end = true,
            _ => {}
        }
    }
}

/// A parser and its routing state have the lifetime of one provider attempt.
pub struct PreviewEvents {
    parser: Parser,
    router: Router,
}

impl PreviewEvents {
    pub fn new(target: Vec<PathPart>) -> Self {
        Self::with_paths(vec![target])
    }

    pub fn with_paths(targets: Vec<Vec<PathPart>>) -> Self {
        Self::with_routes(
            targets
                .into_iter()
                .map(|path| PreviewRoute {
                    path,
                    conditions: Vec::new(),
                    kind: PreviewRouteKind::Text,
                })
                .collect(),
        )
    }

    pub(crate) fn with_routes(routes: Vec<PreviewRoute>) -> Self {
        let mut discriminators = Vec::new();
        for condition in routes.iter().flat_map(|route| &route.conditions) {
            if !discriminators
                .iter()
                .any(|item: &Discriminator| item.path == condition.path)
            {
                discriminators.push(Discriminator {
                    path: condition.path.clone(),
                    value: None,
                });
            }
        }
        Self {
            parser: Parser::new(Mode::Single),
            router: Router {
                stack: Vec::new(),
                string: None,
                string_path: None,
                routes,
                selected: false,
                invalid: false,
                key_text: String::new(),
                value_text: String::new(),
                discriminators,
                pending: Vec::new(),
                candidate_end: false,
                change: None,
            },
        }
    }

    /// Only these arrays contain previewable change entries.
    pub fn with_edits(mut self, paths: Vec<Vec<PathPart>>) -> Self {
        self.router
            .routes
            .extend(paths.into_iter().map(|path| PreviewRoute {
                path,
                conditions: Vec::new(),
                kind: PreviewRouteKind::Changes {
                    allowed_ops: vec![
                        EditFileOp::Replace,
                        EditFileOp::Create,
                        EditFileOp::Delete,
                        EditFileOp::Move,
                        EditFileOp::MoveDir,
                        EditFileOp::Overwrite,
                    ],
                },
            }));
        self
    }

    /// Require a completed discriminator before previewing branch-owned data.
    pub fn with_branch(mut self, path: Vec<PathPart>, value: impl Into<String>) -> Self {
        let condition = PreviewCondition {
            path: path.clone(),
            expected: value.into(),
        };
        for route in &mut self.router.routes {
            route.conditions.push(condition.clone());
        }
        if !self
            .router
            .discriminators
            .iter()
            .any(|item| item.path == path)
        {
            self.router
                .discriminators
                .push(Discriminator { path, value: None });
        }
        self
    }

    pub fn state(&self) -> PreviewState {
        if self.router.invalid {
            PreviewState::Invalid
        } else if self.router.candidate_end {
            PreviewState::CandidateEnd
        } else {
            PreviewState::Incomplete
        }
    }

    pub fn push_updates(&mut self, bytes: &[u8]) -> Vec<PreviewUpdate> {
        let mut updates = Vec::new();
        self.parser
            .push(bytes, |event| self.router.event(event, &mut updates));
        updates
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.push_updates(bytes)
            .into_iter()
            .filter_map(|update| match update {
                PreviewUpdate::Text(text) => Some(text),
                _ => None,
            })
            .collect()
    }

    pub fn finish(&mut self) -> PreviewState {
        self.parser
            .finish(|event| self.router.event(event, &mut Vec::new()));
        self.state()
    }

    pub fn reset(&mut self) {
        *self = Self::with_routes(self.router.routes.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(updates: Vec<PreviewUpdate>) -> String {
        updates
            .into_iter()
            .filter_map(|update| match update {
                PreviewUpdate::Text(text) => Some(text),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn exact_paths_escaped_keys_and_scalar_array_indices() {
        let mut preview = PreviewEvents::new(key_path(&["message"]));
        let input = br#"{"metadata":{"message":"hidden"},"list":[0,true,null,"skip"],"mess\u0061ge":"live"}"#;
        let mut streamed = String::new();
        for byte in input {
            streamed.push_str(&text(preview.push_updates(&[*byte])));
        }
        assert_eq!(streamed, "live");
        assert_eq!(preview.state(), PreviewState::CandidateEnd);

        let mut indexed =
            PreviewEvents::new(vec![PathPart::Key("list".into()), PathPart::Index(3)]);
        assert_eq!(
            text(indexed.push_updates(br#"{"list":[0,true,null,"fourth"]}"#)),
            "fourth"
        );
    }

    #[test]
    fn headers_belong_only_to_direct_change_properties() {
        let mut preview = PreviewEvents::with_paths(vec![])
            .with_edits(vec![key_path(&["action", "changes"])])
            .with_branch(key_path(&["action", "kind"]), "edit");
        let updates = preview.push_updates(br#"{"action":{"kind":"edit","changes":[{"nested":{"op":"delete","path":"wrong"},"op":"create","path":"right"}]}}"#);
        let headers: Vec<_> = updates
            .iter()
            .filter_map(|update| match update {
                PreviewUpdate::Header { op, path, .. } => Some((op.as_str(), path.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(headers, [("create", "right")]);
        assert_eq!(
            updates
                .iter()
                .filter(|update| matches!(update, PreviewUpdate::Change { .. }))
                .count(),
            1
        );

        preview.reset();
        let updates = preview.push_updates(
            br#"{"action":{"kind":"research","changes":[{"op":"create","path":"wrong"}]}}"#,
        );
        assert!(!updates.iter().any(|update| matches!(
            update,
            PreviewUpdate::Header { .. }
                | PreviewUpdate::Change { .. }
                | PreviewUpdate::ArrayEnd { .. }
        )));
    }

    #[test]
    fn alternative_routes_keep_element_identity_and_ignore_foreign_fields() {
        let changes = key_path(&["action", "changes"]);
        let kind = key_path(&["action", "kind"]);
        let routes: Vec<_> = ["create", "delete"]
            .into_iter()
            .map(|name| PreviewRoute {
                path: changes.clone(),
                conditions: vec![PreviewCondition {
                    path: kind.clone(),
                    expected: name.into(),
                }],
                kind: PreviewRouteKind::Changes {
                    allowed_ops: vec![name.parse().unwrap()],
                },
            })
            .collect();
        let input = br#"{"metadata":{"changes":[{"op":"delete","path":"hidden"}]},"action":{"changes":[{"op":"delete","path":"a.rs"},{"op":"create","path":"b.rs"}],"kind":"delete"}}"#;
        let run = |chunked: bool| {
            let mut preview = PreviewEvents::with_routes(routes.clone());
            let mut updates = Vec::new();
            if chunked {
                for byte in input {
                    updates.extend(preview.push_updates(&[*byte]));
                }
            } else {
                updates.extend(preview.push_updates(input));
            }
            assert_eq!(preview.finish(), PreviewState::CandidateEnd);
            updates
                .into_iter()
                .filter_map(|update| match update {
                    PreviewUpdate::Header {
                        route,
                        start,
                        op,
                        path,
                    } => Some(format!("h:{route}:{start}:{op}:{path}")),
                    PreviewUpdate::Change {
                        route,
                        start,
                        op,
                        path,
                        ..
                    } => Some(format!("c:{route}:{start}:{op:?}:{path:?}")),
                    PreviewUpdate::ArrayEnd { route } => Some(format!("end:{route}")),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let whole = run(false);
        assert_eq!(whole, run(true));
        assert_eq!(whole.len(), 3);
        assert!(whole[0].starts_with("h:1:"));
        assert!(whole[1].starts_with("c:1:"));
        assert_eq!(whole[2], "end:1");
        assert_eq!(whole[0].split(':').nth(2), whole[1].split(':').nth(2));
    }

    #[test]
    fn nested_conditions_and_reset_discard_pending_text() {
        let route = PreviewRoute {
            path: key_path(&["action", "choice", "message"]),
            conditions: vec![
                PreviewCondition {
                    path: key_path(&["action", "kind"]),
                    expected: "edit".into(),
                },
                PreviewCondition {
                    path: key_path(&["action", "choice", "kind"]),
                    expected: "answer".into(),
                },
            ],
            kind: PreviewRouteKind::Text,
        };
        let mut preview = PreviewEvents::with_routes(vec![route]);
        assert_eq!(
            text(preview.push_updates(br#"{"action":{"choice":{"message":"pending""#)),
            ""
        );
        preview.reset();
        for (input, expected) in [
            (
                r#"{"action":{"choice":{"message":"yes","kind":"answer"},"kind":"edit"}}"#,
                "yes",
            ),
            (
                r#"{"action":{"choice":{"message":"no","kind":"wrong"},"kind":"edit"}}"#,
                "",
            ),
            (
                r#"{"action":{"choice":{"message":"no","kind":null},"kind":"edit"}}"#,
                "",
            ),
            (
                r#"{"action":{"choice":{"message":"no"},"kind":"edit"}}"#,
                "",
            ),
        ] {
            let whole = text(preview.push_updates(input.as_bytes()));
            assert_eq!(whole, expected);
            preview.reset();
            let fragmented: String = input
                .bytes()
                .map(|byte| text(preview.push_updates(&[byte])))
                .collect();
            assert_eq!(fragmented, expected);
            preview.reset();
        }
    }

    #[test]
    fn invalid_finish_and_reset_isolate_attempts() {
        let mut preview = PreviewEvents::new(key_path(&["message"]));
        assert_eq!(
            text(preview.push_updates(br#"{"message":"partial"#)),
            "partial"
        );
        assert_eq!(preview.state(), PreviewState::Incomplete);
        assert_eq!(preview.finish(), PreviewState::Invalid);
        assert!(preview.push_updates(br#"{"message":"ignored"}"#).is_empty());
        preview.reset();
        assert_eq!(
            text(preview.push_updates(br#"{"message":"fresh"}"#)),
            "fresh"
        );
        assert_eq!(preview.finish(), PreviewState::CandidateEnd);
        preview.reset();
        let _ = preview.push_updates(br#"{"message":"bad","x":!}"#);
        assert_eq!(preview.state(), PreviewState::Invalid);
        assert!(preview
            .push_updates(br#"{"message":"not recovered"}"#)
            .is_empty());
    }
}
