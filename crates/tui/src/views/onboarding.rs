use ratatui::style::Style;

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

/// Semantic accent of an onboarding step badge.
///
/// Accents replace raw ANSI colors so the badge keeps its meaning under any
/// terminal palette; the concrete background and label are resolved from the
/// active theme's semantic roles (`#c-1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepAccent {
    /// Destructive or blocking chrome (Git initialization warning).
    Danger,
    /// Informational chrome.
    Info,
    /// Cautionary chrome (credentials).
    Warning,
    /// Accent chrome (workstation layout).
    Accent,
    /// Neutral chrome (command modifier).
    Neutral,
    /// Success chrome (ready).
    Success,
}

impl StepAccent {
    /// Resolves the filled badge style (background + contrasting label) from the
    /// active semantic palette.
    pub fn badge_style(self, engine: &crate::theme::engine::ThemeEngine) -> Style {
        let palette = engine.palette();
        let (bg, fg) = match self {
            StepAccent::Danger => (palette.status_error, palette.text_inverse),
            StepAccent::Info => (palette.status_info, palette.text_inverse),
            StepAccent::Warning => (palette.status_warning, palette.text_inverse),
            StepAccent::Accent => (palette.button_active_bg, palette.text_inverse),
            StepAccent::Neutral => (palette.overlay, palette.text_primary),
            StepAccent::Success => (palette.status_success, palette.text_inverse),
        };
        engine.bg(bg).fg(engine.color(fg))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnboardingStepMeta {
    pub step: OnboardingStep,
    pub name: &'static str,
    pub accent: StepAccent,
}

pub const ONBOARDING_STEPS: &[OnboardingStepMeta] = &[
    OnboardingStepMeta {
        step: OnboardingStep::Git,
        name: "0. Git",
        accent: StepAccent::Danger,
    },
    OnboardingStepMeta {
        step: OnboardingStep::Config,
        name: "1. Configuration",
        accent: StepAccent::Info,
    },
    OnboardingStepMeta {
        step: OnboardingStep::Credentials,
        name: "2. API Key",
        accent: StepAccent::Warning,
    },
    OnboardingStepMeta {
        step: OnboardingStep::Workstation,
        name: "3. Layout",
        accent: StepAccent::Accent,
    },
    OnboardingStepMeta {
        step: OnboardingStep::Modifier,
        name: "4. Modifier",
        accent: StepAccent::Neutral,
    },
    OnboardingStepMeta {
        step: OnboardingStep::Ready,
        name: "5. Ready",
        accent: StepAccent::Success,
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
