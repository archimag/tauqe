//! Technical edit inputs for virtual staging, independent of serialization.
//!
//! Paths and payloads are supplied by the engine. Staging validates filesystem
//! confinement and admission; these types perform no wire decoding.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditReplace {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditCreate {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditDelete {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditMove {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditMoveDir {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOverwrite {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditLineAnchor {
    pub path: String,
    pub start: String,
    pub end: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOperation {
    Replace(EditReplace),
    Create(EditCreate),
    Delete(EditDelete),
    Move(EditMove),
    MoveDir(EditMoveDir),
    Overwrite(EditOverwrite),
    LineAnchor(EditLineAnchor),
}
