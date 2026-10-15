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
pub fn normalize_identifier(id: &str) -> String {
    id.trim().to_lowercase().replace('_', "-")
}

/// Result of resolving a plan query against a list of known plans.
#[derive(Debug, PartialEq, Eq)]
pub enum PlanResolution<'a> {
    /// Exactly one unambiguous plan was resolved.
    Found(&'a Plan),
    /// Multiple plans matched the query with equal confidence.
    Ambiguous(Vec<&'a Plan>),
    /// No matching plan was found.
    NotFound,
}

impl<'a> PlanResolution<'a> {
    /// Returns the resolved plan if unambiguous.
    pub fn plan(self) -> Option<&'a Plan> {
        match self {
            PlanResolution::Found(p) => Some(p),
            _ => None,
        }
    }

    /// Returns `true` if multiple candidates matched.
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, PlanResolution::Ambiguous(_))
    }
}

/// Resolves a candidate plan by exact ID or normalized ID match only (case-insensitive and `_`/`-` normalized).
///
/// Never performs fuzzy Levenshtein, prefix, or title matching.
pub fn resolve_plan_id_exact<'a>(query: &str, plans: &'a [Plan]) -> Option<&'a Plan> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }

    // 1. Exact ID match (case-insensitive)
    if let Some(p) = plans.iter().find(|p| p.id.eq_ignore_ascii_case(q)) {
        return Some(p);
    }

    // 2. Normalized ID match (hyphens vs underscores, lowercase)
    let norm_q = normalize_identifier(q);
    plans.iter().find(|p| normalize_identifier(&p.id) == norm_q)
}

/// Resolves a candidate plan id, slug, or title against a list of known plans.
///
/// Matching strategy:
/// 1. If query is empty or explicitly "active"/"current", falls back to `active_id`.
/// 2. Exact case-insensitive match on plan ID.
/// 3. Normalized ID match (hyphens vs underscores, trimmed).
/// 4. Exact case-insensitive match on plan Title.
/// 5. Prefix match on plan ID.
/// 6. Tolerant Levenshtein fuzzy match (allows small typos / misspellings).
///
/// At stages 2-6, if multiple candidates match with equal confidence, returns
/// `PlanResolution::Ambiguous(candidates)` instead of guessing.
pub fn resolve_plan<'a>(
    query: &str,
    plans: &'a [Plan],
    active_id: Option<&str>,
) -> PlanResolution<'a> {
    let q = query.trim();
    if q.is_empty() || q.eq_ignore_ascii_case("current") || q.eq_ignore_ascii_case("active") {
        if let Some(act) = active_id {
            if let Some(p) = plans.iter().find(|p| p.id.eq_ignore_ascii_case(act)) {
                return PlanResolution::Found(p);
            }
        }
        return PlanResolution::NotFound;
    }

    if plans.is_empty() {
        return PlanResolution::NotFound;
    }

    // 1. Exact ID match (case-insensitive)
    let exact_id_matches: Vec<&'a Plan> = plans
        .iter()
        .filter(|p| p.id.eq_ignore_ascii_case(q))
        .collect();
    if exact_id_matches.len() == 1 {
        return PlanResolution::Found(exact_id_matches[0]);
    } else if exact_id_matches.len() > 1 {
        return PlanResolution::Ambiguous(exact_id_matches);
    }

    // 2. Normalized ID match (hyphens vs underscores, trimmed)
    let norm_q = normalize_identifier(q);
    let norm_id_matches: Vec<&'a Plan> = plans
        .iter()
        .filter(|p| normalize_identifier(&p.id) == norm_q)
        .collect();
    if norm_id_matches.len() == 1 {
        return PlanResolution::Found(norm_id_matches[0]);
    } else if norm_id_matches.len() > 1 {
        return PlanResolution::Ambiguous(norm_id_matches);
    }

    // 3. Exact Title match
    let title_matches: Vec<&'a Plan> = plans
        .iter()
        .filter(|p| p.title.eq_ignore_ascii_case(q))
        .collect();
    if title_matches.len() == 1 {
        return PlanResolution::Found(title_matches[0]);
    } else if title_matches.len() > 1 {
        return PlanResolution::Ambiguous(title_matches);
    }

    // 4. Prefix match on plan ID
    let prefix_matches: Vec<&'a Plan> = plans
        .iter()
        .filter(|p| {
            let norm_p = normalize_identifier(&p.id);
            norm_p.starts_with(&norm_q) || norm_q.starts_with(&norm_p)
        })
        .collect();
    if prefix_matches.len() == 1 {
        return PlanResolution::Found(prefix_matches[0]);
    } else if prefix_matches.len() > 1 {
        return PlanResolution::Ambiguous(prefix_matches);
    }

    // 5. Fuzzy Levenshtein match
    let mut best_matches = Vec::new();
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

        if dist <= threshold {
            if dist < best_distance {
                best_distance = dist;
                best_matches.clear();
                best_matches.push(plan);
            } else if dist == best_distance {
                best_matches.push(plan);
            }
        }
    }

    if best_matches.len() == 1 {
        return PlanResolution::Found(best_matches[0]);
    } else if best_matches.len() > 1 {
        return PlanResolution::Ambiguous(best_matches);
    }

    PlanResolution::NotFound
}

/// Resolves a candidate plan id, slug, or title against a list of known plans.
///
/// Returns `Some(&Plan)` only if an unambiguous match is found.
pub fn resolve_plan_id<'a>(
    query: &str,
    plans: &'a [Plan],
    active_id: Option<&str>,
) -> Option<&'a Plan> {
    resolve_plan(query, plans, active_id).plan()
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

        // Unmatched query must not guess or fall back to active plan
        let res = resolve_plan_id("completely-unknown-foo", &plans, Some("jwt-auth"));
        assert!(res.is_none());
    }

    #[test]
    fn test_resolve_exact_only() {
        let plans = make_test_plans();

        assert_eq!(
            resolve_plan_id_exact("jwt-auth", &plans).map(|p| p.id.as_str()),
            Some("jwt-auth")
        );
        assert_eq!(
            resolve_plan_id_exact("JWT_AUTH", &plans).map(|p| p.id.as_str()),
            Some("jwt-auth")
        );
        // Fuzzy and prefix are rejected
        assert!(resolve_plan_id_exact("jwt-aut", &plans).is_none());
        assert!(resolve_plan_id_exact("jwt", &plans).is_none());
        assert!(resolve_plan_id_exact("JWT Authentication", &plans).is_none());
    }

    #[test]
    fn test_resolve_ambiguous() {
        let plans = vec![
            Plan {
                id: "plan-1".to_string(),
                title: "Plan One".to_string(),
                ..Default::default()
            },
            Plan {
                id: "plan-2".to_string(),
                title: "Plan Two".to_string(),
                ..Default::default()
            },
        ];

        // "plan" is a prefix for both plan-1 and plan-2
        let res = resolve_plan("plan", &plans, None);
        assert!(res.is_ambiguous());
        match res {
            PlanResolution::Ambiguous(candidates) => {
                assert_eq!(candidates.len(), 2);
            }
            _ => panic!("Expected Ambiguous resolution"),
        }

        // resolve_plan_id returns None for ambiguous queries
        assert!(resolve_plan_id("plan", &plans, None).is_none());
    }
}
