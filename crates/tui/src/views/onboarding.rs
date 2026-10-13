use ratatui::style::Color;

use crate::app::OnboardingStep;

pub mod input;
pub mod render;

pub use self::input::handle_onboarding_key;
pub use self::render::{render_onboarding_step_badge, render_onboarding_view};

pub fn format_masked_key(key: &str) -> String {
    let len = key.chars().count();
    if len == 0 {
        return String::new();
    }
    let bullets = "•".repeat(len.min(16));
    format!("{} ({} chars)", bullets, len)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnboardingStepMeta {
    pub step: OnboardingStep,
    pub name: &'static str,
    pub active_bg: Color,
    pub active_fg: Color,
}

pub const ONBOARDING_STEPS: &[OnboardingStepMeta] = &[
    OnboardingStepMeta {
        step: OnboardingStep::Git,
        name: "0. Git",
        active_bg: Color::Red,
        active_fg: Color::White,
    },
    OnboardingStepMeta {
        step: OnboardingStep::Config,
        name: "1. Configuration",
        active_bg: Color::Cyan,
        active_fg: Color::Black,
    },
    OnboardingStepMeta {
        step: OnboardingStep::Credentials,
        name: "2. API Key",
        active_bg: Color::Yellow,
        active_fg: Color::Black,
    },
    OnboardingStepMeta {
        step: OnboardingStep::Workstation,
        name: "3. Layout",
        active_bg: Color::Magenta,
        active_fg: Color::White,
    },
    OnboardingStepMeta {
        step: OnboardingStep::Modifier,
        name: "4. Modifier",
        active_bg: Color::Blue,
        active_fg: Color::White,
    },
    OnboardingStepMeta {
        step: OnboardingStep::Ready,
        name: "5. Ready",
        active_bg: Color::Green,
        active_fg: Color::Black,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_masked_key() {
        assert_eq!(format_masked_key(""), "");
        assert_eq!(format_masked_key("short-key"), "••••••••• (9 chars)");
        assert_eq!(
            format_masked_key("sk-or-v1-0123456789abcdef0123456789abcdef"),
            "•••••••••••••••• (41 chars)"
        );
    }
}
