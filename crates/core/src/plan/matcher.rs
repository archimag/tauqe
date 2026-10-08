use tauqe_protocol::Plan;

/// Computes the Levenshtein edit distance between two strings.
pub fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let m = a_chars.len();
    let n = b_chars.len();

    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }

    let mut dp = vec![vec![0usize; n + 1]; m + 1];

    for (i, row) in dp.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in dp[0].iter_mut().enumerate() {
        *cell = j;
    }

    for i in 1..=m {
        for j in 1..=n {
            let cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
        }
    }

    dp[m][n]
}

/// Normalizes an identifier or slug for comparison.
fn normalize_identifier(id: &str) -> String {
    id.trim().to_lowercase().replace('_', "-")
}

/// Resolves a candidate plan id, slug, or title against a list of known plans.
///
/// Matching strategy:
/// 1. If query is empty or explicitly "active"/"current", falls back to `active_id`.
/// 2. Exact case-insensitive match on plan ID.
/// 3. Normalized ID match (hyphens vs underscores, trimmed).
/// 4. Exact case-insensitive match on plan Title.
/// 5. Substring / prefix match on plan ID.
/// 6. Tolerant Levenshtein fuzzy match (allows small typos / misspellings).
/// 7. Final fallback to active plan if specified.
pub fn resolve_plan_id<'a>(
    query: &str,
    plans: &'a [Plan],
    active_id: Option<&str>,
) -> Option<&'a Plan> {
    let q = query.trim();
    if q.is_empty() || q.eq_ignore_ascii_case("current") || q.eq_ignore_ascii_case("active") {
        if let Some(act) = active_id {
            if let Some(p) = plans.iter().find(|p| p.id.eq_ignore_ascii_case(act)) {
                return Some(p);
            }
        }
    }

    if plans.is_empty() {
        return None;
    }

    let norm_q = normalize_identifier(q);

    // 1. Exact ID match
    if let Some(p) = plans.iter().find(|p| p.id.eq_ignore_ascii_case(q)) {
        return Some(p);
    }

    // 2. Normalized ID match
    if let Some(p) = plans
        .iter()
        .find(|p| normalize_identifier(&p.id) == norm_q)
    {
        return Some(p);
    }

    // 3. Exact Title match
    if let Some(p) = plans.iter().find(|p| p.title.eq_ignore_ascii_case(q)) {
        return Some(p);
    }

    // 4. Prefix or substring match
    if let Some(p) = plans.iter().find(|p| {
        let norm_p = normalize_identifier(&p.id);
        norm_p.starts_with(&norm_q) || norm_q.starts_with(&norm_p)
    }) {
        return Some(p);
    }

    // 5. Fuzzy Levenshtein match
    let mut best_match = None;
    let mut best_distance = usize::MAX;

    for plan in plans {
        let norm_p = normalize_identifier(&plan.id);
        let dist = levenshtein_distance(&norm_q, &norm_p);
        let max_len = norm_q.len().max(norm_p.len());

        let threshold = if max_len <= 3 {
            1
        } else if max_len <= 7 {
            2
        } else {
            3
        };

        if dist <= threshold && dist < best_distance {
            best_distance = dist;
            best_match = Some(plan);
        }
    }

    if let Some(p) = best_match {
        return Some(p);
    }

    // 6. Fallback to active plan
    if let Some(act) = active_id {
        return plans.iter().find(|p| p.id.eq_ignore_ascii_case(act));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_plans() -> Vec<Plan> {
        vec![
            Plan {
                id: "jwt-auth".to_string(),
                title: "JWT Authentication".to_string(),
                ..Default::default()
            },
            Plan {
                id: "tui-keymaps".to_string(),
                title: "TUI Keymap Refactor".to_string(),
                ..Default::default()
            },
            Plan {
                id: "context-layers".to_string(),
                title: "Three-Tier Context Layers".to_string(),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn test_levenshtein() {
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
        assert_eq!(levenshtein_distance("plan", "plan"), 0);
        assert_eq!(levenshtein_distance("", "test"), 4);
    }

    #[test]
    fn test_resolve_exact_and_normalized() {
        let plans = make_test_plans();

        // Exact
        let res = resolve_plan_id("jwt-auth", &plans, None).unwrap();
        assert_eq!(res.id, "jwt-auth");

        // Case-insensitive
        let res = resolve_plan_id("JWT-AUTH", &plans, None).unwrap();
        assert_eq!(res.id, "jwt-auth");

        // Underscore normalization
        let res = resolve_plan_id("tui_keymaps", &plans, None).unwrap();
        assert_eq!(res.id, "tui-keymaps");

        // Title match
        let res = resolve_plan_id("Three-Tier Context Layers", &plans, None).unwrap();
        assert_eq!(res.id, "context-layers");
    }

    #[test]
    fn test_resolve_typos_fuzzy() {
        let plans = make_test_plans();

        // Typo: "jwt-aut" (missing 'h') -> dist 1
        let res = resolve_plan_id("jwt-aut", &plans, None).unwrap();
        assert_eq!(res.id, "jwt-auth");

        // Typo: "tui-kymaps" -> dist 1
        let res = resolve_plan_id("tui-kymaps", &plans, None).unwrap();
        assert_eq!(res.id, "tui-keymaps");

        // Typo: "context-layrs" -> dist 1
        let res = resolve_plan_id("context-layrs", &plans, None).unwrap();
        assert_eq!(res.id, "context-layers");
    }

    #[test]
    fn test_resolve_active_fallback() {
        let plans = make_test_plans();

        // Query empty with active
        let res = resolve_plan_id("", &plans, Some("tui-keymaps")).unwrap();
        assert_eq!(res.id, "tui-keymaps");

        // Query "current"
        let res = resolve_plan_id("current", &plans, Some("jwt-auth")).unwrap();
        assert_eq!(res.id, "jwt-auth");

        // Completely unmatched query with active fallback
        let res = resolve_plan_id("completely-unknown-foo", &plans, Some("jwt-auth")).unwrap();
        assert_eq!(res.id, "jwt-auth");
    }
}
