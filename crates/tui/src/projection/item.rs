use tauqe_protocol::PlanItem;
#[cfg(test)]
use tauqe_protocol::PlanItemStatus;
#[cfg(test)]
pub(crate) trait PlanStatusProjection {
    fn next(&self) -> Self;
}
#[cfg(test)]
impl PlanStatusProjection for PlanItemStatus {
    fn next(&self) -> Self {
        match self {
            Self::Discussion => Self::Todo,
            Self::Todo => Self::InProgress,
            Self::InProgress => Self::Done,
            Self::Done => Self::Blocked,
            Self::Blocked => Self::Cancelled,
            Self::Cancelled => Self::Discussion,
        }
    }
}
pub trait PlanItemProjection {
    fn is_leaf(&self) -> bool;
}
impl PlanItemProjection for PlanItem {
    fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}
