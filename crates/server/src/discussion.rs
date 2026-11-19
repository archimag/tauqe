//! RPC conversion for the application discussion scenario.
use crate::contracts::Project;

use tauqe_flow::api as core;
use tauqe_protocol as wire;

pub(crate) fn target_from_dto(target: wire::DiscussionTarget) -> core::DiscussionTarget {
    target.project()
}

pub(crate) fn focus_from_dto(focus: wire::DiscussionFocus) -> core::DiscussionFocus {
    match focus {
        wire::DiscussionFocus::None => core::DiscussionFocus::None,
        wire::DiscussionFocus::Focused {
            target,
            focused_items,
        } => core::DiscussionFocus::Focused {
            target: target_from_dto(target),
            focused_items,
        },
    }
}

pub(crate) fn focus_dto(focus: core::DiscussionFocus) -> wire::DiscussionFocus {
    match focus {
        core::DiscussionFocus::None => wire::DiscussionFocus::None,
        core::DiscussionFocus::Focused {
            target,
            focused_items,
        } => {
            let target = target.project();
            wire::DiscussionFocus::Focused {
                target,
                focused_items,
            }
        }
    }
}

impl Project for wire::DiscussionFocus {
    type Target = core::DiscussionFocus;

    fn project(self) -> Self::Target {
        focus_from_dto(self)
    }
}

impl Project for core::DiscussionFocus {
    type Target = wire::DiscussionFocus;

    fn project(self) -> Self::Target {
        focus_dto(self)
    }
}
