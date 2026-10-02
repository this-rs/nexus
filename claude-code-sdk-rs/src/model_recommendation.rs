//! Model selection recommendations for token optimization
//!
//! This module provides utilities to help choose the most cost-effective Claude model
//! based on task complexity and requirements.

use std::collections::HashMap;

/// Model recommendation helper
///
/// Provides recommendations for which Claude model to use based on task type.
/// You can use the default recommendations or provide custom mappings.
#[derive(Debug, Clone)]
pub struct ModelRecommendation {
    recommendations: HashMap<String, String>,
}

impl ModelRecommendation {
    /// Create with default recommendations
    ///
    /// Default mappings:
    /// - "simple" / "fast" / "cheap" → claude-3-5-haiku-20241022 (fastest, cheapest)
    /// - "balanced" / "general" / "latest" → claude-sonnet-4-5-20250929 (latest Sonnet, balanced performance/cost)
    /// - "complex" / "best" / "quality" → claude-opus-4-7 (most capable)
    ///
    /// # Example
    ///
    /// ```rust
    /// use nexus_claude::model_recommendation::ModelRecommendation;
    ///
    /// let recommender = ModelRecommendation::default();
    /// let model = recommender.suggest("simple").unwrap();
    /// assert_eq!(model, "claude-3-5-haiku-20241022");
    /// ```
    pub fn with_defaults() -> Self {
        let mut map = HashMap::new();

        // Simple/fast tasks - use Haiku (cheapest, fastest)
        map.insert(
            "simple".to_string(),
            "claude-3-5-haiku-20241022".to_string(),
        );
        map.insert("fast".to_string(), "claude-3-5-haiku-20241022".to_string());
        map.insert("cheap".to_string(), "claude-3-5-haiku-20241022".to_string());
        map.insert("quick".to_string(), "claude-3-5-haiku-20241022".to_string());

        // Balanced tasks - use Sonnet 4.5 (good balance, latest)
        map.insert(
            "balanced".to_string(),
            "claude-sonnet-4-5-20250929".to_string(),
        );
        map.insert(
            "general".to_string(),
            "claude-sonnet-4-5-20250929".to_string(),
        );
        map.insert(
            "normal".to_string(),
            "claude-sonnet-4-5-20250929".to_string(),
        );
        map.insert(
            "standard".to_string(),
            "claude-sonnet-4-5-20250929".to_string(),
        );
        map.insert(
            "latest".to_string(),
            "claude-sonnet-4-5-20250929".to_string(),
        );

        // Complex/critical tasks - use Opus 4.7 (most capable, latest)
        map.insert("complex".to_string(), "claude-opus-4-7".to_string());
        map.insert("best".to_string(), "claude-opus-4-7".to_string());
        map.insert("quality".to_string(), "claude-opus-4-7".to_string());
        map.insert("critical".to_string(), "claude-opus-4-7".to_string());
        map.insert("advanced".to_string(), "claude-opus-4-7".to_string());

        Self {
            recommendations: map,
        }
    }

    /// Create with custom recommendations
    ///
    /// # Example
    ///
    /// ```rust
    /// use nexus_claude::model_recommendation::ModelRecommendation;
    /// use std::collections::HashMap;
    ///
    /// let mut custom_map = HashMap::new();
    /// custom_map.insert("code_review".to_string(), "sonnet".to_string());
    /// custom_map.insert("documentation".to_string(), "claude-3-5-haiku-20241022".to_string());
    ///
    /// let recommender = ModelRecommendation::custom(custom_map);
    /// ```
    pub fn custom(recommendations: HashMap<String, String>) -> Self {
        Self { recommendations }
    }

    /// Get a model suggestion for a given task type
    ///
    /// Returns the recommended model name, or None if no recommendation exists.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nexus_claude::model_recommendation::ModelRecommendation;
    ///
    /// let recommender = ModelRecommendation::default();
    ///
    /// // For simple tasks, use Haiku
    /// assert_eq!(recommender.suggest("simple"), Some("claude-3-5-haiku-20241022"));
    ///
    /// // For complex tasks, use Opus 4.7
    /// assert_eq!(recommender.suggest("complex"), Some("claude-opus-4-7"));
    /// ```
    pub fn suggest(&self, task_type: &str) -> Option<&str> {
        self.recommendations.get(task_type).map(|s| s.as_str())
    }

    /// Add or update a recommendation
    ///
    /// # Example
    ///
    /// ```rust
    /// use nexus_claude::model_recommendation::ModelRecommendation;
    ///
    /// let mut recommender = ModelRecommendation::default();
    /// recommender.add("my_task", "sonnet");
    /// assert_eq!(recommender.suggest("my_task"), Some("sonnet"));
    /// ```
    pub fn add(&mut self, task_type: impl Into<String>, model: impl Into<String>) {
        self.recommendations.insert(task_type.into(), model.into());
    }

    /// Remove a recommendation
    pub fn remove(&mut self, task_type: &str) -> Option<String> {
        self.recommendations.remove(task_type)
    }

    /// Get all task types with recommendations
    pub fn task_types(&self) -> Vec<&str> {
        self.recommendations.keys().map(|s| s.as_str()).collect()
    }

    /// Get all available recommendations
    pub fn all_recommendations(&self) -> &HashMap<String, String> {
        &self.recommendations
    }
}

impl Default for ModelRecommendation {
    fn default() -> Self {
        // Use our predefined defaults
        ModelRecommendation::with_defaults()
    }
}

/// Quick helper functions for common use cases
/// Get the cheapest/fastest model (Haiku)
pub fn cheapest_model() -> &'static str {
    "claude-3-5-haiku-20241022"
}

/// Get the balanced model (Sonnet 4.5 - latest)
pub fn balanced_model() -> &'static str {
    "claude-sonnet-4-5-20250929"
}

/// Get the latest Sonnet model alias
pub fn latest_sonnet() -> &'static str {
    "claude-sonnet-4-5-20250929"
}

/// Get the most capable model (Opus 4.7)
pub fn best_model() -> &'static str {
    "claude-opus-4-7"
}

/// Estimate relative cost multiplier for different models
///
/// Returns approximate cost multiplier relative to Haiku (1.0x).
/// These are rough estimates and actual costs depend on usage patterns.
///
/// # Example
///
/// ```rust
/// use nexus_claude::model_recommendation::estimate_cost_multiplier;
///
/// // Haiku is baseline (1.0x)
/// assert_eq!(estimate_cost_multiplier("claude-3-5-haiku-20241022"), 1.0);
///
/// // Sonnet is ~5x more expensive
/// assert_eq!(estimate_cost_multiplier("sonnet"), 5.0);
///
/// // Opus is ~15x more expensive
/// assert_eq!(estimate_cost_multiplier("opus"), 15.0);
/// ```
pub fn estimate_cost_multiplier(model: &str) -> f64 {
    match model {
        // Haiku - baseline (cheapest)
        "haiku" | "claude-3-5-haiku-20241022" => 1.0,

        // Sonnet - ~5x more expensive than Haiku
        "sonnet"
        | "claude-sonnet-4-5-20250929"  // Sonnet 4.5 (latest)
        | "claude-sonnet-4-20250514"    // Sonnet 4
        | "claude-3-5-sonnet-20241022"  // Sonnet 3.5
        => 5.0,

        // Opus - ~15x more expensive than Haiku
        "opus" | "claude-opus-4-7" | "claude-opus-4-6" | "claude-opus-4-1-20250805" => 15.0,

        // Unknown - assume Sonnet level
        _ => 5.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_recommendations() {
        let recommender = ModelRecommendation::default();

        assert_eq!(
            recommender.suggest("simple"),
            Some("claude-3-5-haiku-20241022")
        );
        assert_eq!(
            recommender.suggest("fast"),
            Some("claude-3-5-haiku-20241022")
        );
        assert_eq!(
            recommender.suggest("balanced"),
            Some("claude-sonnet-4-5-20250929")
        );
        assert_eq!(
            recommender.suggest("latest"),
            Some("claude-sonnet-4-5-20250929")
        );
        assert_eq!(recommender.suggest("complex"), Some("claude-opus-4-7"));
        assert_eq!(recommender.suggest("unknown"), None);
    }

    #[test]
    fn test_custom_recommendations() {
        let mut map = HashMap::new();
        map.insert("code_review".to_string(), "sonnet".to_string());

        let recommender = ModelRecommendation::custom(map);
        assert_eq!(recommender.suggest("code_review"), Some("sonnet"));
    }

    #[test]
    fn test_add_remove() {
        let mut recommender = ModelRecommendation::default();

        recommender.add("my_task", "sonnet");
        assert_eq!(recommender.suggest("my_task"), Some("sonnet"));

        recommender.remove("my_task");
        assert_eq!(recommender.suggest("my_task"), None);
    }

    #[test]
    fn test_cost_multipliers() {
        assert_eq!(estimate_cost_multiplier("haiku"), 1.0);
        assert_eq!(estimate_cost_multiplier("sonnet"), 5.0);
        assert_eq!(estimate_cost_multiplier("opus"), 15.0);
    }

    #[test]
    fn test_quick_helpers() {
        assert_eq!(cheapest_model(), "claude-3-5-haiku-20241022");
        assert_eq!(balanced_model(), "claude-sonnet-4-5-20250929");
        assert_eq!(latest_sonnet(), "claude-sonnet-4-5-20250929");
        assert_eq!(best_model(), "claude-opus-4-7");
    }

    #[test]
    fn test_cost_multiplier_haiku_full_name() {
        assert_eq!(estimate_cost_multiplier("claude-3-5-haiku-20241022"), 1.0);
    }

    #[test]
    fn test_cost_multiplier_sonnet_4_5_full_name() {
        assert_eq!(estimate_cost_multiplier("claude-sonnet-4-5-20250929"), 5.0);
    }

    #[test]
    fn test_cost_multiplier_sonnet_4_full_name() {
        assert_eq!(estimate_cost_multiplier("claude-sonnet-4-20250514"), 5.0);
    }

    #[test]
    fn test_cost_multiplier_sonnet_3_5_full_name() {
        assert_eq!(estimate_cost_multiplier("claude-3-5-sonnet-20241022"), 5.0);
    }

    #[test]
    fn test_cost_multiplier_opus_full_name() {
        assert_eq!(estimate_cost_multiplier("claude-opus-4-1-20250805"), 15.0);
    }

    #[test]
    fn test_cost_multiplier_unknown_model() {
        assert_eq!(estimate_cost_multiplier("gpt-4o"), 5.0);
        assert_eq!(estimate_cost_multiplier("unknown-model-xyz"), 5.0);
    }

    #[test]
    fn test_task_types_returns_all_keys() {
        let recommender = ModelRecommendation::with_defaults();
        let mut types = recommender.task_types();
        types.sort();
        let expected = vec![
            "advanced", "balanced", "best", "cheap", "complex", "critical", "fast", "general",
            "latest", "normal", "quality", "quick", "simple", "standard",
        ];
        assert_eq!(types, expected);
    }

    #[test]
    fn test_all_recommendations_returns_hashmap() {
        let recommender = ModelRecommendation::with_defaults();
        let all = recommender.all_recommendations();
        assert_eq!(all.len(), 14);
        assert_eq!(
            all.get("simple").map(|s| s.as_str()),
            Some("claude-3-5-haiku-20241022")
        );
        assert_eq!(
            all.get("complex").map(|s| s.as_str()),
            Some("claude-opus-4-7")
        );
        assert_eq!(
            all.get("balanced").map(|s| s.as_str()),
            Some("claude-sonnet-4-5-20250929")
        );
    }

    #[test]
    fn test_with_defaults_all_mappings() {
        let recommender = ModelRecommendation::with_defaults();

        // cheap/quick → haiku
        assert_eq!(
            recommender.suggest("cheap"),
            Some("claude-3-5-haiku-20241022")
        );
        assert_eq!(
            recommender.suggest("quick"),
            Some("claude-3-5-haiku-20241022")
        );

        // normal/standard → sonnet 4.5
        assert_eq!(
            recommender.suggest("normal"),
            Some("claude-sonnet-4-5-20250929")
        );
        assert_eq!(
            recommender.suggest("standard"),
            Some("claude-sonnet-4-5-20250929")
        );

        // best/quality/critical/advanced → opus 4.7
        assert_eq!(recommender.suggest("best"), Some("claude-opus-4-7"));
        assert_eq!(recommender.suggest("quality"), Some("claude-opus-4-7"));
        assert_eq!(recommender.suggest("critical"), Some("claude-opus-4-7"));
        assert_eq!(recommender.suggest("advanced"), Some("claude-opus-4-7"));
    }

    #[test]
    fn test_suggest_general_returns_sonnet_4_5() {
        let recommender = ModelRecommendation::with_defaults();
        assert_eq!(
            recommender.suggest("general"),
            Some("claude-sonnet-4-5-20250929")
        );
    }

    /// `remove()` hands back the mapping it deleted, then `None`.
    #[test]
    fn test_remove_returns_the_previous_mapping_then_none() {
        let mut recommender = ModelRecommendation::with_defaults();

        assert_eq!(
            recommender.remove("simple"),
            Some("claude-3-5-haiku-20241022".to_string())
        );
        assert_eq!(recommender.remove("simple"), None);
        assert_eq!(recommender.suggest("simple"), None);
    }

    /// `add()` overwrites an existing mapping silently.
    #[test]
    fn test_add_overwrites_an_existing_task_type() {
        let mut recommender = ModelRecommendation::with_defaults();
        recommender.add("simple", "claude-opus-4-7");
        assert_eq!(recommender.suggest("simple"), Some("claude-opus-4-7"));
        assert_eq!(recommender.all_recommendations().len(), 14);
    }

    /// Lookups are case-sensitive and not trimmed: a near-miss returns `None`
    /// rather than the obvious match.
    #[test]
    fn test_suggest_is_case_sensitive_and_untrimmed() {
        let recommender = ModelRecommendation::with_defaults();
        assert_eq!(recommender.suggest("Simple"), None);
        assert_eq!(recommender.suggest(" simple"), None);
        assert_eq!(recommender.suggest(""), None);
    }

    /// `custom()` performs no validation at all: an empty table, an empty task
    /// type and an empty model name are all accepted.
    #[test]
    fn test_custom_accepts_an_empty_and_nonsensical_table() {
        let empty = ModelRecommendation::custom(HashMap::new());
        assert_eq!(empty.suggest("anything"), None);
        assert!(empty.task_types().is_empty());

        let mut map = HashMap::new();
        map.insert(String::new(), String::new());
        let nonsense = ModelRecommendation::custom(map);
        assert_eq!(nonsense.suggest(""), Some(""));
    }

    /// `estimate_cost_multiplier` matches exact strings only, so an unexpected
    /// casing silently falls back to the Sonnet price.
    #[test]
    fn test_cost_multiplier_matching_is_exact_and_falls_back_silently() {
        assert_eq!(estimate_cost_multiplier("HAIKU"), 5.0);
        assert_eq!(estimate_cost_multiplier("claude-3-5-haiku"), 5.0);
        assert_eq!(estimate_cost_multiplier(""), 5.0);
    }

    /// The price table predates the current model lineup: the models the API
    /// crate advertises today (`claude-haiku-4-5`, `claude-opus-4-5`, …) are
    /// unknown here and therefore priced as Sonnet. For Haiku 4.5 that is a 5x
    /// overestimate — see `claude-code-api/src/models/claude.rs` for the list
    /// this table should track.
    #[test]
    fn test_current_lineup_is_missing_from_the_cost_table() {
        assert_eq!(
            estimate_cost_multiplier("claude-haiku-4-5"),
            5.0,
            "a Haiku-class model is priced as Sonnet because it is unknown"
        );
        assert_eq!(estimate_cost_multiplier("claude-opus-4-5"), 5.0);
        // Opus 4.6 and 4.7 *are* known, so the gap is specific to the newer
        // Haiku/Opus ids rather than systematic.
        assert_eq!(estimate_cost_multiplier("claude-opus-4-6"), 15.0);
        assert_eq!(estimate_cost_multiplier("claude-opus-4-7"), 15.0);
    }

    /// Consistency between the two halves of this module: every model the
    /// default table recommends must have an explicit price, never the unknown
    /// fallback that happens to be 5.0.
    #[test]
    fn test_every_default_recommendation_has_an_explicit_price() {
        let recommender = ModelRecommendation::with_defaults();
        let expected: HashMap<&str, f64> = HashMap::from([
            ("claude-3-5-haiku-20241022", 1.0),
            ("claude-sonnet-4-5-20250929", 5.0),
            ("claude-opus-4-7", 15.0),
        ]);

        for (task_type, model) in recommender.all_recommendations() {
            let price = expected.get(model.as_str()).copied().unwrap_or_else(|| {
                panic!("task type {task_type:?} recommends unpriced model {model:?}")
            });
            assert_eq!(
                estimate_cost_multiplier(model),
                price,
                "price drift for {model}"
            );
        }
    }

    /// The three quick helpers must stay ordered by price, cheapest first.
    #[test]
    fn test_quick_helpers_are_ordered_by_cost() {
        assert!(
            estimate_cost_multiplier(cheapest_model()) < estimate_cost_multiplier(balanced_model())
        );
        assert!(
            estimate_cost_multiplier(balanced_model()) < estimate_cost_multiplier(best_model())
        );
        assert_eq!(balanced_model(), latest_sonnet());
    }

    /// The helpers and the default table must not disagree.
    #[test]
    fn test_quick_helpers_match_the_default_table() {
        let recommender = ModelRecommendation::with_defaults();
        assert_eq!(recommender.suggest("cheap"), Some(cheapest_model()));
        assert_eq!(recommender.suggest("balanced"), Some(balanced_model()));
        assert_eq!(recommender.suggest("best"), Some(best_model()));
    }
}
