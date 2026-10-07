use ratatui::style::Style;
use ratatui::text::Span;

use super::{MarkdownTheme, StyleModifier};

/// Converts LaTeX math commands, symbols, and formatting into clean Unicode text.
pub fn convert_latex_to_unicode(latex: &str) -> String {
    let mut s = latex.trim().to_string();

    // Strip outer math delimiters if present
    if (s.starts_with("$$") && s.ends_with("$$") && s.len() >= 4)
        || (s.starts_with("\\[") && s.ends_with("\\]") && s.len() >= 4)
        || (s.starts_with("\\(") && s.ends_with("\\)") && s.len() >= 4)
    {
        s = s[2..s.len() - 2].trim().to_string();
    } else if s.starts_with('$') && s.ends_with('$') && s.len() >= 2 {
        s = s[1..s.len() - 1].trim().to_string();
    }

    // Calligraphic letters: \mathcal{F} -> ℱ, \mathcal{R} -> ℛ
    s = replace_calligraphic(&s);

    // Unwrap text/math mode wrappers: \text{...}, \mathrm{...}, \mathbf{...}, etc.
    s = unwrap_braced_commands(
        &s,
        &[
            "\\text",
            "\\mathrm",
            "\\mathbf",
            "\\mathit",
            "\\operatorname",
            "\\mathsf",
            "\\mathtt",
            "\\textbf",
            "\\textit",
            "\\texttt",
            "\\textnormal",
            "\\boldsymbol",
            "\\bold",
            "\\bm",
        ],
    );

    // Handle fractions: \frac{a}{b}, \dfrac, \tfrac -> (a)/(b) or a/b
    s = replace_fractions(&s);

    // Handle square roots: \sqrt{x} -> √(x)
    s = replace_sqrt(&s);

    // Blackboard bold numbers/sets: \mathbb{R} -> ℝ, etc.
    s = replace_blackboard(&s);

    // Standard LaTeX commands to Unicode symbols
    s = replace_latex_symbols(&s);

    // Subscripts and superscripts: x^2 -> x², a_1 -> a₁
    s = replace_scripts(&s);

    // Clean up remaining single braces used for grouping in LaTeX
    s = cleanup_grouping_braces(&s);

    // Normalize multiple spaces
    let mut result = String::with_capacity(s.len());
    let mut last_was_space = false;
    for ch in s.chars() {
        if ch == ' ' {
            if !last_was_space {
                result.push(' ');
                last_was_space = true;
            }
        } else {
            result.push(ch);
            last_was_space = false;
        }
    }

    result.trim().to_string()
}

fn to_calligraphic(ch: char) -> Option<char> {
    match ch {
        'A' => Some('𝒜'),
        'B' => Some('ℬ'),
        'C' => Some('𝒞'),
        'D' => Some('𝒟'),
        'E' => Some('ℰ'),
        'F' => Some('ℱ'),
        'G' => Some('𝒢'),
        'H' => Some('ℋ'),
        'I' => Some('ℐ'),
        'J' => Some('𝒥'),
        'K' => Some('𝒦'),
        'L' => Some('ℒ'),
        'M' => Some('ℳ'),
        'N' => Some('𝒩'),
        'O' => Some('𝒪'),
        'P' => Some('𝒫'),
        'Q' => Some('𝒬'),
        'R' => Some('ℛ'),
        'S' => Some('𝒮'),
        'T' => Some('𝒯'),
        'U' => Some('𝒰'),
        'V' => Some('𝒱'),
        'W' => Some('𝒲'),
        'X' => Some('𝒳'),
        'Y' => Some('𝒴'),
        'Z' => Some('𝒵'),
        _ => None,
    }
}

fn replace_calligraphic(mut text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let commands = ["\\mathcal", "\\mathscr"];

    while !text.is_empty() {
        let mut found: Option<(usize, &str)> = None;
        for cmd in &commands {
            if let Some(pos) = text.find(cmd) {
                if found.as_ref().is_none_or(|(p, _)| pos < *p) {
                    found = Some((pos, *cmd));
                }
            }
        }

        if let Some((pos, cmd)) = found {
            out.push_str(&text[..pos]);
            let after_cmd = &text[pos + cmd.len()..];
            let after_cmd_trimmed = after_cmd.trim_start();
            if after_cmd_trimmed.starts_with('{') {
                let offset = pos + cmd.len() + (after_cmd.len() - after_cmd_trimmed.len());
                if let Some(close_idx) = find_matching_brace(&text[offset..]) {
                    let content = &text[offset + 1..offset + close_idx];
                    for c in content.chars() {
                        if let Some(calli) = to_calligraphic(c) {
                            out.push(calli);
                        } else {
                            out.push(c);
                        }
                    }
                    text = &text[offset + close_idx + 1..];
                    continue;
                }
            } else if let Some(first_char) = after_cmd_trimmed.chars().next() {
                if first_char.is_alphabetic() {
                    let offset = pos + cmd.len() + (after_cmd.len() - after_cmd_trimmed.len());
                    if let Some(calli) = to_calligraphic(first_char) {
                        out.push(calli);
                    } else {
                        out.push(first_char);
                    }
                    text = &text[offset + first_char.len_utf8()..];
                    continue;
                }
            }
            out.push_str(cmd);
            text = &text[pos + cmd.len()..];
        } else {
            out.push_str(text);
            break;
        }
    }
    out
}

fn unwrap_braced_commands(mut text: &str, commands: &[&str]) -> String {
    let mut out = String::with_capacity(text.len());
    while !text.is_empty() {
        let mut found: Option<(usize, &str)> = None;
        for cmd in commands {
            if let Some(pos) = text.find(cmd) {
                if found.as_ref().is_none_or(|(p, _)| pos < *p) {
                    found = Some((pos, *cmd));
                }
            }
        }

        if let Some((pos, cmd)) = found {
            out.push_str(&text[..pos]);
            let after_cmd = &text[pos + cmd.len()..];
            let after_cmd_trimmed = after_cmd.trim_start();
            if after_cmd_trimmed.starts_with('{') {
                let open_idx = pos + cmd.len() + (after_cmd.len() - after_cmd_trimmed.len());
                if let Some(close_idx) = find_matching_brace(&text[open_idx..]) {
                    let content = &text[open_idx + 1..open_idx + close_idx];
                    out.push_str(content);
                    text = &text[open_idx + close_idx + 1..];
                    continue;
                }
            }
            out.push_str(cmd);
            text = &text[pos + cmd.len()..];
        } else {
            out.push_str(text);
            break;
        }
    }
    out
}

fn replace_fractions(mut text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let commands = ["\\dfrac", "\\tfrac", "\\frac"];

    while !text.is_empty() {
        let mut found: Option<(usize, &str)> = None;
        for cmd in &commands {
            if let Some(pos) = text.find(cmd) {
                if found.as_ref().is_none_or(|(p, _)| pos < *p) {
                    found = Some((pos, *cmd));
                }
            }
        }

        if let Some((pos, cmd)) = found {
            out.push_str(&text[..pos]);
            let mut cursor = pos + cmd.len();
            let rest = &text[cursor..];
            let trimmed_num = rest.trim_start();
            cursor += rest.len() - trimmed_num.len();

            let (num_str, new_cursor) = if text[cursor..].starts_with('{') {
                if let Some(close_num) = find_matching_brace(&text[cursor..]) {
                    let n = &text[cursor + 1..cursor + close_num];
                    (Some(n), cursor + close_num + 1)
                } else {
                    (None, cursor)
                }
            } else if let Some(c) = text[cursor..].chars().next() {
                if c.is_alphanumeric() {
                    let n = &text[cursor..cursor + c.len_utf8()];
                    (Some(n), cursor + c.len_utf8())
                } else {
                    (None, cursor)
                }
            } else {
                (None, cursor)
            };

            if let Some(num) = num_str {
                cursor = new_cursor;
                let rest_den = &text[cursor..];
                let trimmed_den = rest_den.trim_start();
                cursor += rest_den.len() - trimmed_den.len();

                let (den_str, final_cursor) = if text[cursor..].starts_with('{') {
                    if let Some(close_den) = find_matching_brace(&text[cursor..]) {
                        let d = &text[cursor + 1..cursor + close_den];
                        (Some(d), cursor + close_den + 1)
                    } else {
                        (None, cursor)
                    }
                } else if let Some(c) = text[cursor..].chars().next() {
                    if c.is_alphanumeric() {
                        let d = &text[cursor..cursor + c.len_utf8()];
                        (Some(d), cursor + c.len_utf8())
                    } else {
                        (None, cursor)
                    }
                } else {
                    (None, cursor)
                };

                if let Some(den) = den_str {
                    let num_clean = convert_latex_to_unicode(num);
                    let den_clean = convert_latex_to_unicode(den);

                    let num_fmt = if num_clean.contains(' ')
                        || num_clean.contains('+')
                        || num_clean.contains('-')
                    {
                        format!("({})", num_clean)
                    } else {
                        num_clean
                    };
                    let den_fmt = if den_clean.contains(' ')
                        || den_clean.contains('+')
                        || den_clean.contains('-')
                    {
                        format!("({})", den_clean)
                    } else {
                        den_clean
                    };

                    out.push_str(&format!("{}/{}", num_fmt, den_fmt));
                    text = &text[final_cursor..];
                    continue;
                }
            }
            out.push_str(cmd);
            text = &text[pos + cmd.len()..];
        } else {
            out.push_str(text);
            break;
        }
    }
    out
}

fn replace_sqrt(mut text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let cmd = "\\sqrt";
    while let Some(pos) = text.find(cmd) {
        out.push_str(&text[..pos]);
        let cursor = pos + cmd.len();
        if text[cursor..].starts_with('{') {
            if let Some(close_idx) = find_matching_brace(&text[cursor..]) {
                let inner = &text[cursor + 1..cursor + close_idx];
                out.push_str(&format!("√({})", convert_latex_to_unicode(inner)));
                text = &text[cursor + close_idx + 1..];
                continue;
            }
        }
        out.push('√');
        text = &text[cursor..];
    }
    out.push_str(text);
    out
}

fn replace_blackboard(text: &str) -> String {
    let mut s = text.to_string();
    let mappings = [
        ("\\mathbb{R}", "ℝ"),
        ("\\mathbb{N}", "ℕ"),
        ("\\mathbb{Z}", "ℤ"),
        ("\\mathbb{Q}", "ℚ"),
        ("\\mathbb{C}", "ℂ"),
        ("\\mathbb{P}", "ℙ"),
        ("\\mathbb{H}", "ℍ"),
    ];
    for (k, v) in mappings {
        s = s.replace(k, v);
    }
    s
}

fn replace_latex_symbols(text: &str) -> String {
    let mut s = text.to_string();

    // Sorted by specificity (longer sequences first to prevent partial match collisions)
    let mappings: &[(&str, &str)] = &[
        // Arrows (long)
        ("\\longleftrightarrow", "⟷"),
        ("\\Longleftrightarrow", "⟺"),
        ("\\longrightarrow", "⟶"),
        ("\\Longrightarrow", "⟹"),
        ("\\longleftarrow", "⟵"),
        ("\\Longleftarrow", "⟺"),
        // Arrows (standard)
        ("\\leftrightarrow", "↔"),
        ("\\Leftrightarrow", "⇔"),
        ("\\hookrightarrow", "↪"),
        ("\\rightarrow", "→"),
        ("\\Rightarrow", "⇒"),
        ("\\leftarrow", "←"),
        ("\\Leftarrow", "⇐"),
        ("\\uparrow", "↑"),
        ("\\Uparrow", "⇑"),
        ("\\downarrow", "↓"),
        ("\\Downarrow", "⇓"),
        ("\\updownarrow", "↕"),
        ("\\nearrow", "↗"),
        ("\\searrow", "↘"),
        ("\\swarrow", "↙"),
        ("\\nwarrow", "↖"),
        ("\\mapsto", "↦"),
        ("\\implies", "⇒"),
        ("\\iff", "⇔"),
        ("\\gets", "←"),
        ("\\to", "→"),
        // Comparison and relations
        ("\\approx", "≈"),
        ("\\equiv", "≡"),
        ("\\simeq", "≃"),
        ("\\cong", "≅"),
        ("\\propto", "∝"),
        ("\\notin", "∉"),
        ("\\subseteq", "⊆"),
        ("\\supseteq", "⊇"),
        ("\\subset", "⊂"),
        ("\\supset", "⊃"),
        ("\\neq", "≠"),
        ("\\leq", "≤"),
        ("\\geq", "≥"),
        ("\\sim", "∼"),
        ("\\ne", "≠"),
        ("\\le", "≤"),
        ("\\ge", "≥"),
        ("\\pm", "±"),
        ("\\mp", "∓"),
        ("\\times", "×"),
        ("\\cdot", "·"),
        ("\\div", "÷"),
        ("\\circ", "∘"),
        ("\\bullet", "•"),
        ("\\otimes", "⊗"),
        ("\\oplus", "⊕"),
        ("\\ast", "∗"),
        ("\\star", "⋆"),
        // Sets and Logic
        ("\\emptyset", "∅"),
        ("\\varnothing", "∅"),
        ("\\forall", "∀"),
        ("\\exists", "∃"),
        ("\\nexists", "∄"),
        ("\\setminus", "∖"),
        ("\\infty", "∞"),
        ("\\partial", "∂"),
        ("\\nabla", "∇"),
        ("\\lnot", "¬"),
        ("\\neg", "¬"),
        ("\\land", "∧"),
        ("\\lor", "∨"),
        ("\\cap", "∩"),
        ("\\cup", "∪"),
        ("\\in", "∈"),
        ("\\ni", "∋"),
        ("\\sum", "∑"),
        ("\\prod", "∏"),
        ("\\int", "∫"),
        ("\\oint", "∮"),
        // Dots
        ("\\ldots", "…"),
        ("\\cdots", "⋯"),
        ("\\vdots", "⋮"),
        ("\\ddots", "⋱"),
        ("\\dots", "…"),
        // Greek letters (variants & uppercase)
        ("\\varepsilon", "ε"),
        ("\\vartheta", "θ"),
        ("\\varpi", "ϖ"),
        ("\\varrho", "ϱ"),
        ("\\varsigma", "ς"),
        ("\\varphi", "φ"),
        ("\\Upsilon", "Υ"),
        ("\\Lambda", "Λ"),
        ("\\Theta", "Θ"),
        ("\\Sigma", "Σ"),
        ("\\Omega", "Ω"),
        ("\\Gamma", "Γ"),
        ("\\Delta", "Δ"),
        ("\\Phi", "Φ"),
        ("\\Psi", "Ψ"),
        ("\\Xi", "Ξ"),
        ("\\Pi", "Π"),
        // Greek letters (standard lowercase)
        ("\\alpha", "α"),
        ("\\beta", "β"),
        ("\\gamma", "γ"),
        ("\\delta", "δ"),
        ("\\epsilon", "ε"),
        ("\\zeta", "ζ"),
        ("\\eta", "η"),
        ("\\theta", "θ"),
        ("\\iota", "ι"),
        ("\\kappa", "κ"),
        ("\\lambda", "λ"),
        ("\\mu", "μ"),
        ("\\nu", "ν"),
        ("\\xi", "ξ"),
        ("\\pi", "π"),
        ("\\rho", "ρ"),
        ("\\sigma", "σ"),
        ("\\tau", "τ"),
        ("\\upsilon", "υ"),
        ("\\phi", "φ"),
        ("\\chi", "χ"),
        ("\\psi", "ψ"),
        ("\\omega", "ω"),
        // Spacing & escapes
        ("\\qquad", "    "),
        ("\\quad", "  "),
        ("\\,", " "),
        ("\\;", " "),
        ("\\:", " "),
        ("\\{", "{"),
        ("\\}", "}"),
        ("\\$", "$"),
        ("\\_", "_"),
        ("\\%", "%"),
        ("\\&", "&"),
    ];

    for &(from, to) in mappings {
        s = s.replace(from, to);
    }

    s
}

fn to_superscript(ch: char) -> Option<char> {
    match ch {
        '0' => Some('⁰'),
        '1' => Some('¹'),
        '2' => Some('²'),
        '3' => Some('³'),
        '4' => Some('⁴'),
        '5' => Some('⁵'),
        '6' => Some('⁶'),
        '7' => Some('⁷'),
        '8' => Some('⁸'),
        '9' => Some('⁹'),
        '+' => Some('⁺'),
        '-' => Some('⁻'),
        '=' => Some('⁼'),
        '(' => Some('⁽'),
        ')' => Some('⁾'),
        'n' => Some('ⁿ'),
        'i' => Some('ⁱ'),
        _ => None,
    }
}

fn to_subscript(ch: char) -> Option<char> {
    match ch {
        '0' => Some('₀'),
        '1' => Some('₁'),
        '2' => Some('₂'),
        '3' => Some('₃'),
        '4' => Some('₄'),
        '5' => Some('₅'),
        '6' => Some('₆'),
        '7' => Some('₇'),
        '8' => Some('₈'),
        '9' => Some('₉'),
        '+' => Some('₊'),
        '-' => Some('₋'),
        '=' => Some('₌'),
        '(' => Some('₍'),
        ')' => Some('₎'),
        'a' => Some('ₐ'),
        'e' => Some('ₑ'),
        'h' => Some('ₕ'),
        'i' => Some('ᵢ'),
        'j' => Some('ⱼ'),
        'k' => Some('ₖ'),
        'l' => Some('ₗ'),
        'm' => Some('ₘ'),
        'n' => Some('ₙ'),
        'o' => Some('ₒ'),
        'p' => Some('ₚ'),
        'r' => Some('ᵣ'),
        's' => Some('ₛ'),
        't' => Some('ₜ'),
        'u' => Some('ᵤ'),
        'v' => Some('ᵥ'),
        'x' => Some('ₓ'),
        _ => None,
    }
}

fn replace_scripts(mut text: &str) -> String {
    let mut out = String::with_capacity(text.len());

    while !text.is_empty() {
        if let Some(pos) = text.find(['^', '_']) {
            out.push_str(&text[..pos]);
            let is_super = text.as_bytes()[pos] == b'^';
            let after = &text[pos + 1..];

            if after.starts_with('{') {
                if let Some(close_idx) = find_matching_brace(after) {
                    let group = &after[1..close_idx];
                    let mut converted = String::new();
                    let mut all_converted = true;
                    for c in group.chars() {
                        if let Some(sc) = if is_super {
                            to_superscript(c)
                        } else {
                            to_subscript(c)
                        } {
                            converted.push(sc);
                        } else {
                            all_converted = false;
                            break;
                        }
                    }

                    if all_converted && !converted.is_empty() {
                        out.push_str(&converted);
                        text = &after[close_idx + 1..];
                        continue;
                    }
                }
            } else if let Some(first_char) = after.chars().next() {
                if let Some(sc) = if is_super {
                            to_superscript(first_char)
                        } else {
                            to_subscript(first_char)
                        } {
                    out.push(sc);
                    text = &after[first_char.len_utf8()..];
                    continue;
                }
            }

            out.push(if is_super { '^' } else { '_' });
            text = after;
        } else {
            out.push_str(text);
            break;
        }
    }

    out
}

fn cleanup_grouping_braces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(&next) = chars.peek() {
                if next == '{' || next == '}' {
                    out.push(next);
                    chars.next();
                    continue;
                }
            }
            out.push(ch);
        } else if ch != '{' && ch != '}' {
            out.push(ch);
        }
    }
    out
}

fn find_matching_brace(text: &str) -> Option<usize> {
    if !text.starts_with('{') {
        return None;
    }
    let mut depth = 0;
    for (i, c) in text.char_indices() {
        if c == '{' {
            depth += 1;
        } else if c == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Cleans standalone LaTeX commands and formatting in plain text outside math delimiters.
fn clean_latex_in_plain_text(text: &str) -> String {
    let mut s = text.to_string();
    s = replace_calligraphic(&s);
    s = unwrap_braced_commands(
        &s,
        &[
            "\\text",
            "\\mathrm",
            "\\mathbf",
            "\\mathit",
            "\\operatorname",
            "\\mathsf",
            "\\mathtt",
            "\\textbf",
            "\\textit",
            "\\texttt",
            "\\textnormal",
            "\\boldsymbol",
            "\\bold",
            "\\bm",
        ],
    );
    s = replace_fractions(&s);
    s = replace_sqrt(&s);
    s = replace_blackboard(&s);
    s = replace_latex_symbols(&s);
    s = cleanup_grouping_braces(&s);
    s
}

/// Renders a text slice into Ratatui `Span`s, extracting and styling LaTeX math segments.
pub(crate) fn render_text_spans(
    text: &str,
    base_style: Style,
    theme: &MarkdownTheme,
    style_mod: StyleModifier,
) -> Vec<Span<'static>> {
    if !text.contains('$') && !text.contains('\\') {
        return vec![Span::styled(
            text.to_string(),
            style_mod.to_style(base_style, theme),
        )];
    }

    let mut spans = Vec::new();
    let mut remainder = text;

    while !remainder.is_empty() {
        // Check for display math: $$...$$ or \[...\]
        if let Some(pos) = remainder.find("$$") {
            let after = &remainder[pos + 2..];
            if let Some(end_pos) = after.find("$$") {
                let prefix = &remainder[..pos];
                if !prefix.is_empty() {
                    spans.extend(render_inline_math_or_plain(
                        prefix, base_style, theme, style_mod,
                    ));
                }
                let math_content = &after[..end_pos];
                let converted = convert_latex_to_unicode(math_content);
                spans.push(Span::styled(
                    converted,
                    style_mod.to_style(theme.math_style, theme),
                ));
                remainder = &after[end_pos + 2..];
                continue;
            }
        }

        if let Some(pos) = remainder.find("\\[") {
            let after = &remainder[pos + 2..];
            if let Some(end_pos) = after.find("\\]") {
                let prefix = &remainder[..pos];
                if !prefix.is_empty() {
                    spans.extend(render_inline_math_or_plain(
                        prefix, base_style, theme, style_mod,
                    ));
                }
                let math_content = &after[..end_pos];
                let converted = convert_latex_to_unicode(math_content);
                spans.push(Span::styled(
                    converted,
                    style_mod.to_style(theme.math_style, theme),
                ));
                remainder = &after[end_pos + 2..];
                continue;
            }
        }

        // Process inline math and plain text for the rest
        spans.extend(render_inline_math_or_plain(
            remainder, base_style, theme, style_mod,
        ));
        break;
    }

    spans
}

fn render_inline_math_or_plain(
    text: &str,
    base_style: Style,
    theme: &MarkdownTheme,
    style_mod: StyleModifier,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut remainder = text;

    while let Some(open_idx) = remainder.find('$') {
        let after_open = &remainder[open_idx + 1..];
        // Ensure opening $ is not followed immediately by space or digit (like $50) unless it's a closed formula
        if let Some(close_idx) = after_open.find('$') {
            let candidate = &after_open[..close_idx];
            // Valid math formula: not empty, doesn't span multiple lines, and looks like math
            if is_likely_math(candidate) {
                let prefix = &remainder[..open_idx];
                if !prefix.is_empty() {
                    let cleaned_prefix = clean_latex_in_plain_text(prefix);
                    spans.push(Span::styled(
                        cleaned_prefix,
                        style_mod.to_style(base_style, theme),
                    ));
                }

                let converted = convert_latex_to_unicode(candidate);
                spans.push(Span::styled(
                    converted,
                    style_mod.to_style(theme.math_style, theme),
                ));

                remainder = &after_open[close_idx + 1..];
                continue;
            }
        }

        // Not valid math block, push up to $ and continue
        let chunk = &remainder[..open_idx + 1];
        let cleaned = clean_latex_in_plain_text(chunk);
        spans.push(Span::styled(
            cleaned,
            style_mod.to_style(base_style, theme),
        ));
        remainder = after_open;
    }

    if !remainder.is_empty() {
        // Even plain text might contain standalone arrows/macros like \longrightarrow or \to
        let cleaned = clean_latex_in_plain_text(remainder);
        spans.push(Span::styled(
            cleaned,
            style_mod.to_style(base_style, theme),
        ));
    }

    spans
}

fn is_likely_math(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return false;
    }

    // Has explicit LaTeX macros or operators
    if trimmed.contains('\\')
        || trimmed.contains('^')
        || trimmed.contains('_')
        || trimmed.contains('=')
    {
        return true;
    }

    // Common single-letter math variables or expressions (e.g. $x$, $y$, $O(n \log n)$)
    if trimmed.len() <= 3 && trimmed.chars().all(|c| c.is_alphabetic()) {
        return true;
    }

    // Reject pure currency strings: e.g. "50", "100.00"
    if trimmed
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
    {
        return false;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_arrows_and_relations() {
        assert_eq!(convert_latex_to_unicode(r"\to"), "→");
        assert_eq!(convert_latex_to_unicode(r"\longrightarrow"), "⟶");
        assert_eq!(convert_latex_to_unicode(r"A \implies B"), "A ⇒ B");
        assert_eq!(
            convert_latex_to_unicode(r"x \le y \ge z \neq 0"),
            "x ≤ y ≥ z ≠ 0"
        );
    }

    #[test]
    fn test_text_wrapper_and_display_math() {
        let input =
            r"$$\text{Prompt} \longrightarrow \text{Discovery Phase} \longrightarrow \dots$$";
        assert_eq!(
            convert_latex_to_unicode(input),
            "Prompt ⟶ Discovery Phase ⟶ …"
        );
    }

    #[test]
    fn test_calligraphic_and_fractions() {
        assert_eq!(convert_latex_to_unicode(r"\mathcal{F}"), "ℱ");
        assert_eq!(convert_latex_to_unicode(r"\mathcal{O}(n)"), "𝒪(n)");
        assert_eq!(convert_latex_to_unicode(r"\frac{1}{2}"), "1/2");
        assert_eq!(convert_latex_to_unicode(r"\dfrac{a + b}{c}"), "(a + b)/c");
    }

    #[test]
    fn test_greek_and_subscripts() {
        assert_eq!(
            convert_latex_to_unicode(r"\alpha_1 + \beta^2 = \gamma"),
            "α₁ + β² = γ"
        );
        assert_eq!(convert_latex_to_unicode(r"2^{10}"), "2¹⁰");
        assert_eq!(convert_latex_to_unicode(r"x_{ij}"), "xᵢⱼ");
    }

    #[test]
    fn test_fractions_and_sqrt() {
        assert_eq!(convert_latex_to_unicode(r"\frac{1}{2}"), "1/2");
        assert_eq!(convert_latex_to_unicode(r"\frac{a + b}{c}"), "(a + b)/c");
        assert_eq!(convert_latex_to_unicode(r"\sqrt{x}"), "√(x)");
    }

    #[test]
    fn test_blackboard_sets() {
        assert_eq!(convert_latex_to_unicode(r"x \in \mathbb{R}"), "x ∈ ℝ");
    }

    #[test]
    fn test_standalone_arrows_and_macros_in_plain_text() {
        let spans = render_text_spans(
            r"User \frac{1}{2} \text{Prompt} \longrightarrow \mathcal{F}",
            Style::default(),
            &MarkdownTheme::answer(),
            StyleModifier::default(),
        );
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "User 1/2 Prompt ⟶ ℱ");
    }

    #[test]
    fn test_currency_is_not_mangled() {
        let spans = render_text_spans(
            "Price is $50 and $100 today",
            Style::default(),
            &MarkdownTheme::answer(),
            StyleModifier::default(),
        );
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "Price is $50 and $100 today");
    }
}
