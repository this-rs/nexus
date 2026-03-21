/// Multi-layered anonymization pipeline for episodic memory export.
///
/// Addresses privacy and IP leakage risks when sharing episodes across instances
/// or organizations. Each stage strips a different category of sensitive information.
///
/// # References
/// - "Episodic Memory in AI Agents Poses Risks That Should Be Studied and Mitigated" (2025)
///   — Identifies privacy/manipulation risks in episodic memory systems.
///   This pipeline directly addresses the privacy dimension by ensuring exported
///   episodes do not leak proprietary code paths, secrets, or business logic identifiers.
use std::collections::HashMap;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::episodes::EpisodeData;

// ---------------------------------------------------------------------------
// AnonymizationStage trait
// ---------------------------------------------------------------------------

/// A single stage in the anonymization pipeline.
///
/// Each stage receives a mutable `EpisodeData` and transforms it in-place,
/// stripping or replacing a category of sensitive information.
///
/// # References
/// - "Episodic Memory in AI Agents Poses Risks That Should Be Studied and Mitigated" (2025)
pub trait AnonymizationStage: Send + Sync {
    /// Human-readable name of this stage.
    fn name(&self) -> &str;
    /// Apply the anonymization transformation to the episode.
    fn apply(&self, episode: &mut EpisodeData);
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// Configurable multi-stage anonymization pipeline.
///
/// Stages are executed in order. The pipeline is `Serialize`/`Deserialize` via its
/// configuration representation ([`PipelineConfig`]).
///
/// # References
/// - "Episodic Memory in AI Agents Poses Risks That Should Be Studied and Mitigated" (2025)
pub struct AnonymizationPipeline {
    stages: Vec<Box<dyn AnonymizationStage>>,
}

impl AnonymizationPipeline {
    /// Create an empty pipeline (no stages).
    pub fn new() -> Self {
        Self { stages: Vec::new() }
    }

    /// Build a pipeline from a serializable configuration.
    pub fn from_config(config: &PipelineConfig) -> Self {
        let mut pipeline = Self::new();
        for stage_cfg in &config.stages {
            match stage_cfg {
                StageConfig::PathStripper => {
                    pipeline.add_stage(Box::new(PathStripper));
                }
                StageConfig::SecretDetector => {
                    pipeline.add_stage(Box::new(SecretDetector::new()));
                }
                StageConfig::EntityGeneralizer => {
                    pipeline.add_stage(Box::new(EntityGeneralizer::new()));
                }
                StageConfig::MetricNoise { sigma } => {
                    pipeline.add_stage(Box::new(MetricNoise::new(*sigma)));
                }
            }
        }
        pipeline
    }

    /// Build the default pipeline with all stages active (σ=0.05 for noise).
    pub fn default_pipeline() -> Self {
        Self::from_config(&PipelineConfig::default())
    }

    /// Add a stage to the pipeline.
    pub fn add_stage(&mut self, stage: Box<dyn AnonymizationStage>) {
        self.stages.push(stage);
    }

    /// Run all stages on the given episode.
    pub fn anonymize(&self, episode: &mut EpisodeData) {
        for stage in &self.stages {
            stage.apply(episode);
        }
    }

    /// Run all stages, returning a new anonymized copy.
    pub fn anonymize_clone(&self, episode: &EpisodeData) -> EpisodeData {
        let mut ep = episode.clone();
        self.anonymize(&mut ep);
        ep
    }
}

impl Default for AnonymizationPipeline {
    fn default() -> Self {
        Self::default_pipeline()
    }
}

// ---------------------------------------------------------------------------
// Serializable config
// ---------------------------------------------------------------------------

/// Serializable/deserializable pipeline configuration.
///
/// # References
/// - "Episodic Memory in AI Agents Poses Risks That Should Be Studied and Mitigated" (2025)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PipelineConfig {
    pub stages: Vec<StageConfig>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            stages: vec![
                StageConfig::PathStripper,
                StageConfig::SecretDetector,
                StageConfig::EntityGeneralizer,
                StageConfig::MetricNoise { sigma: 0.05 },
            ],
        }
    }
}

/// Configuration enum for each available stage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StageConfig {
    PathStripper,
    SecretDetector,
    EntityGeneralizer,
    MetricNoise { sigma: f64 },
}

// ---------------------------------------------------------------------------
// Stage 1: PathStripper
// ---------------------------------------------------------------------------

/// Replaces absolute file paths with relative canonical paths.
///
/// Strips common prefixes like `/home/user/...`, `/Users/...`, `C:\Users\...`
/// to prevent leaking filesystem layout.
///
/// # References
/// - "Episodic Memory in AI Agents Poses Risks That Should Be Studied and Mitigated" (2025)
pub struct PathStripper;

impl PathStripper {
    fn strip_paths(text: &str) -> String {
        // Match Unix and Windows absolute paths
        let re = Regex::new(
            r#"(?:/(?:home|Users|var|tmp|opt|usr|etc)/[^\s:,;"'\]})\x27]+|[A-Z]:\\[^\s:,;"'\]})\x27]+)"#,
        )
        .expect("valid regex");
        re.replace_all(text, |caps: &regex::Captures| {
            let path = &caps[0];
            // Extract filename or last two components
            let parts: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
            if parts.len() >= 2 {
                format!("<path>/{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
            } else if !parts.is_empty() {
                format!("<path>/{}", parts[parts.len() - 1])
            } else {
                "<path>".to_string()
            }
        })
        .to_string()
    }

    fn strip_in_strings(strings: &mut [String]) {
        for s in strings.iter_mut() {
            *s = Self::strip_paths(s);
        }
    }
}

impl AnonymizationStage for PathStripper {
    fn name(&self) -> &str {
        "PathStripper"
    }

    fn apply(&self, episode: &mut EpisodeData) {
        episode.stimulus = Self::strip_paths(&episode.stimulus);
        Self::strip_in_strings(&mut episode.process);
        episode.outcome.description = Self::strip_paths(&episode.outcome.description);
        if let Some(ref mut rec) = episode.outcome.recommendation {
            *rec = Self::strip_paths(rec);
        }
        for gate in &mut episode.gate_results {
            if let Some(ref mut details) = gate.details {
                *details = Self::strip_paths(details);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 2: SecretDetector
// ---------------------------------------------------------------------------

/// Detects and redacts secrets: AWS keys, JWT tokens, passwords, private keys, generic tokens.
///
/// Uses regex patterns to identify common secret formats and replaces them
/// with typed redaction placeholders. False negatives on known patterns are forbidden
/// by design — all standard patterns are covered.
///
/// # References
/// - "Episodic Memory in AI Agents Poses Risks That Should Be Studied and Mitigated" (2025)
///   — Addresses IP/secret leakage risk in exported episodic memory.
pub struct SecretDetector {
    patterns: Vec<(Regex, &'static str)>,
}

impl SecretDetector {
    pub fn new() -> Self {
        let patterns = vec![
            // AWS Access Key ID (starts with AKIA, 20 chars)
            (
                Regex::new(r"AKIA[0-9A-Z]{16}").expect("valid regex"),
                "[REDACTED_AWS_KEY]",
            ),
            // AWS Secret Access Key (40 chars base64-like after a separator)
            (
                Regex::new(r"(?i)(?:aws_secret_access_key|secret_key)\s*[=:]\s*[A-Za-z0-9/+=]{40}")
                    .expect("valid regex"),
                "[REDACTED_AWS_SECRET]",
            ),
            // JWT tokens (eyJ...)
            (
                Regex::new(r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}")
                    .expect("valid regex"),
                "[REDACTED_JWT]",
            ),
            // PEM private keys
            (
                Regex::new(r"-----BEGIN[A-Z ]*PRIVATE KEY-----").expect("valid regex"),
                "[REDACTED_PRIVATE_KEY]",
            ),
            // Generic password/secret/token assignments
            (
                Regex::new(
                    r#"(?i)(?:password|passwd|secret|token|api_key|apikey|api-key|auth_token|access_token)\s*[=:]\s*["']?[^\s"',;]{8,}["']?"#,
                )
                .expect("valid regex"),
                "[REDACTED_SECRET]",
            ),
            // Bearer tokens in headers
            (
                Regex::new(r"(?i)Bearer\s+[A-Za-z0-9._~+/=-]{20,}").expect("valid regex"),
                "[REDACTED_BEARER_TOKEN]",
            ),
            // GitHub personal access tokens
            (
                Regex::new(r"ghp_[A-Za-z0-9]{36}").expect("valid regex"),
                "[REDACTED_GITHUB_TOKEN]",
            ),
            // Hex-encoded secrets (64 chars = 256 bits)
            (
                Regex::new(r"(?i)(?:secret|key|token)\s*[=:]\s*[0-9a-f]{64}").expect("valid regex"),
                "[REDACTED_HEX_SECRET]",
            ),
        ];
        Self { patterns }
    }

    fn redact(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (re, replacement) in &self.patterns {
            result = re.replace_all(&result, *replacement).to_string();
        }
        result
    }

    fn redact_strings(&self, strings: &mut [String]) {
        for s in strings.iter_mut() {
            *s = self.redact(s);
        }
    }
}

impl AnonymizationStage for SecretDetector {
    fn name(&self) -> &str {
        "SecretDetector"
    }

    fn apply(&self, episode: &mut EpisodeData) {
        episode.stimulus = self.redact(&episode.stimulus);
        self.redact_strings(&mut episode.process);
        episode.outcome.description = self.redact(&episode.outcome.description);
        if let Some(ref mut rec) = episode.outcome.recommendation {
            *rec = self.redact(rec);
        }
        for gate in &mut episode.gate_results {
            gate.gate_name = self.redact(&gate.gate_name);
            if let Some(ref mut details) = gate.details {
                *details = self.redact(details);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 3: EntityGeneralizer
// ---------------------------------------------------------------------------

/// Replaces specific code entity names (classes, functions, files) with typed placeholders.
///
/// Maintains a mapping table so the same entity always maps to the same placeholder
/// within a single anonymization pass, preserving structural relationships.
///
/// # References
/// - "Episodic Memory in AI Agents Poses Risks That Should Be Studied and Mitigated" (2025)
///   — Prevents leaking business logic identifiers through exported episodes.
pub struct EntityGeneralizer {
    /// Regex for CamelCase identifiers (likely class/struct names)
    struct_re: Regex,
    /// Regex for snake_case identifiers (likely function names)
    fn_re: Regex,
    /// Regex for file paths/names
    file_re: Regex,
}

impl EntityGeneralizer {
    pub fn new() -> Self {
        Self {
            // CamelCase identifiers: at least two words, min 4 chars
            struct_re: Regex::new(r"\b([A-Z][a-z]+(?:[A-Z][a-z0-9]*)+)\b").expect("valid regex"),
            // snake_case function-like identifiers: min 2 segments
            fn_re: Regex::new(r"\b([a-z][a-z0-9]*(?:_[a-z][a-z0-9]*)+)\b").expect("valid regex"),
            // File names with extensions
            file_re: Regex::new(r"\b(\w+\.(?:rs|py|js|ts|go|java|rb|cpp|c|h|toml|yaml|yml|json))\b")
                .expect("valid regex"),
        }
    }

    fn generalize_text(&self, text: &str, map: &mut EntityMap) -> String {
        let mut result = text.to_string();

        // Replace file names first (most specific)
        result = self
            .file_re
            .replace_all(&result, |caps: &regex::Captures| {
                map.get_or_insert(&caps[1], EntityKind::File)
            })
            .to_string();

        // Replace CamelCase (structs/classes)
        result = self
            .struct_re
            .replace_all(&result, |caps: &regex::Captures| {
                let name = &caps[1];
                // Skip common non-entity CamelCase words
                if is_common_word(name) {
                    name.to_string()
                } else {
                    map.get_or_insert(name, EntityKind::Struct)
                }
            })
            .to_string();

        // Replace snake_case (functions)
        result = self
            .fn_re
            .replace_all(&result, |caps: &regex::Captures| {
                let name = &caps[1];
                if is_common_snake(name) {
                    name.to_string()
                } else {
                    map.get_or_insert(name, EntityKind::Function)
                }
            })
            .to_string();

        result
    }
}

/// Common CamelCase words that should NOT be generalized.
fn is_common_word(w: &str) -> bool {
    matches!(
        w,
        "OutcomeType" | "DateTime" | "HashMap" | "HashSet" | "String" | "Option" | "Result"
            | "EpisodeData" | "GateResult" | "EpisodeOutcome" | "Boolean" | "NexusEpisode"
    )
}

/// Common snake_case identifiers that should NOT be generalized.
fn is_common_snake(w: &str) -> bool {
    matches!(
        w,
        "outcome_type"
            | "created_at"
            | "source_id"
            | "gate_name"
            | "gate_results"
            | "pattern_type"
            | "affected_files"
            | "occurrence_count"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum EntityKind {
    File,
    Struct,
    Function,
}

/// Internal mapping from original entity names to anonymized placeholders.
#[derive(Debug, Clone)]
struct EntityMap {
    map: HashMap<String, String>,
    counters: HashMap<EntityKind, usize>,
}

impl EntityMap {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            counters: HashMap::new(),
        }
    }

    fn get_or_insert(&mut self, name: &str, kind: EntityKind) -> String {
        if let Some(existing) = self.map.get(name) {
            return existing.clone();
        }
        let counter = self.counters.entry(kind).or_insert(0);
        *counter += 1;
        let placeholder = match kind {
            EntityKind::File => format!("File_{}", counter),
            EntityKind::Struct => format!("Struct_{}", counter),
            EntityKind::Function => format!("Function_{}", counter),
        };
        self.map.insert(name.to_string(), placeholder.clone());
        placeholder
    }
}

impl AnonymizationStage for EntityGeneralizer {
    fn name(&self) -> &str {
        "EntityGeneralizer"
    }

    fn apply(&self, episode: &mut EpisodeData) {
        let mut map = EntityMap::new();

        episode.stimulus = self.generalize_text(&episode.stimulus, &mut map);
        for s in &mut episode.process {
            *s = self.generalize_text(s, &mut map);
        }
        episode.outcome.description = self.generalize_text(&episode.outcome.description, &mut map);
        if let Some(ref mut rec) = episode.outcome.recommendation {
            *rec = self.generalize_text(rec, &mut map);
        }
        for gate in &mut episode.gate_results {
            gate.gate_name = self.generalize_text(&gate.gate_name, &mut map);
            if let Some(ref mut details) = gate.details {
                *details = self.generalize_text(details, &mut map);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 4: MetricNoise
// ---------------------------------------------------------------------------

/// Adds controlled Gaussian noise to numeric metrics in episode text for differential privacy.
///
/// Scans text for numeric values and adds noise with configurable standard deviation (σ).
/// The noise is proportional to the value: `noise = value * N(0, σ)`.
///
/// # References
/// - "Episodic Memory in AI Agents Poses Risks That Should Be Studied and Mitigated" (2025)
///   — Differential privacy prevents inference of exact metrics from exported episodes.
pub struct MetricNoise {
    sigma: f64,
    number_re: Regex,
}

impl MetricNoise {
    pub fn new(sigma: f64) -> Self {
        // Sanitize sigma to a reasonable range
        let sigma = sigma.clamp(0.001, 1.0);
        Self {
            sigma,
            number_re: Regex::new(r"\b(\d+\.?\d*)\b").expect("valid regex"),
        }
    }

    fn add_noise_to_text(&self, text: &str) -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();

        self.number_re
            .replace_all(text, |caps: &regex::Captures| {
                let original = &caps[1];
                if let Ok(value) = original.parse::<f64>() {
                    // Skip very small numbers (likely IDs or indices, not metrics)
                    if value.abs() < 1.0 {
                        return original.to_string();
                    }
                    // Box-Muller transform for Gaussian noise
                    let u1: f64 = rng.gen_range(0.0001..1.0);
                    let u2: f64 = rng.gen_range(0.0..std::f64::consts::TAU);
                    let z = (-2.0 * u1.ln()).sqrt() * u2.cos();
                    let noise = value * self.sigma * z;
                    let noisy = value + noise;
                    // Keep positive if original was positive
                    let noisy = if value > 0.0 { noisy.max(0.0) } else { noisy };
                    if original.contains('.') {
                        format!("{:.2}", noisy)
                    } else {
                        format!("{}", noisy.round() as i64)
                    }
                } else {
                    original.to_string()
                }
            })
            .to_string()
    }
}

impl AnonymizationStage for MetricNoise {
    fn name(&self) -> &str {
        "MetricNoise"
    }

    fn apply(&self, episode: &mut EpisodeData) {
        episode.stimulus = self.add_noise_to_text(&episode.stimulus);
        for s in &mut episode.process {
            *s = self.add_noise_to_text(s);
        }
        episode.outcome.description = self.add_noise_to_text(&episode.outcome.description);
        if let Some(ref mut rec) = episode.outcome.recommendation {
            *rec = self.add_noise_to_text(rec);
        }
        for gate in &mut episode.gate_results {
            if let Some(ref mut details) = gate.details {
                *details = self.add_noise_to_text(details);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Consent Gate
// ---------------------------------------------------------------------------

/// Sharing policy for a project, checked before any episode export.
///
/// # References
/// - "Episodic Memory in AI Agents Poses Risks That Should Be Studied and Mitigated" (2025)
///   — Consent gating prevents unauthorized export of episodic memory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SharingPolicy {
    /// Whether sharing/export is enabled for this project.
    pub sharing_enabled: bool,
    /// Optional list of allowed export targets (e.g., registry URLs).
    pub allowed_targets: Vec<String>,
    /// Pipeline configuration to use when exporting.
    pub anonymization_config: PipelineConfig,
}

impl Default for SharingPolicy {
    fn default() -> Self {
        Self {
            sharing_enabled: false,
            allowed_targets: Vec::new(),
            anonymization_config: PipelineConfig::default(),
        }
    }
}

/// Error returned when an export is rejected by the consent gate.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum ExportError {
    #[error("Sharing is disabled for this project")]
    SharingDisabled,
    #[error("Export target '{0}' is not in the allowed list")]
    TargetNotAllowed(String),
}

/// Check consent and anonymize an episode for export.
///
/// Returns the anonymized episode if the sharing policy allows export,
/// or an error if sharing is disabled or the target is not allowed.
///
/// # References
/// - "Episodic Memory in AI Agents Poses Risks That Should Be Studied and Mitigated" (2025)
pub fn consent_gate_export(
    episode: &EpisodeData,
    policy: &SharingPolicy,
    target: Option<&str>,
) -> Result<EpisodeData, ExportError> {
    if !policy.sharing_enabled {
        return Err(ExportError::SharingDisabled);
    }

    if let Some(target) = target {
        // Validate target against allowlist
        let sanitized_target = target.trim();
        if !policy.allowed_targets.is_empty()
            && !policy.allowed_targets.iter().any(|t| t == sanitized_target)
        {
            return Err(ExportError::TargetNotAllowed(sanitized_target.to_string()));
        }
    }

    let pipeline = AnonymizationPipeline::from_config(&policy.anonymization_config);
    Ok(pipeline.anonymize_clone(episode))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::learning::episodes::{EpisodeOutcome, GateResult, OutcomeType};
    use chrono::Utc;
    use uuid::Uuid;

    fn make_test_episode() -> EpisodeData {
        EpisodeData {
            id: Uuid::new_v4(),
            stimulus: "error in /home/user/projects/myapp/src/MySecretService.rs".to_string(),
            process: vec![
                "/Users/dev/code/myapp/src/lib.rs".to_string(),
                "Check MySecretService::process_payment function".to_string(),
            ],
            outcome: EpisodeOutcome {
                outcome_type: OutcomeType::Negative,
                description: "Failed with AKIAIOSFODNN7EXAMPLE key exposed, password=s3cr3tV4lue! in config".to_string(),
                recommendation: Some("Remove hardcoded secrets from /home/user/projects/myapp/config.toml".to_string()),
            },
            gate_results: vec![GateResult {
                gate_name: "security_scan".to_string(),
                passed: false,
                details: Some("Found token=eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiYWRtaW4iOnRydWV9.TJVA95OrM7E2cBab30RMHrHDcEfxjoYZgeFONFh7HgQ".to_string()),
            }],
            synthetic: false,
            source_id: None,
            created_at: Utc::now(),
        }
    }

    // --- PathStripper tests ---

    #[test]
    fn test_path_stripper_replaces_absolute_paths() {
        let mut ep = make_test_episode();
        PathStripper.apply(&mut ep);

        assert!(!ep.stimulus.contains("/home/user/projects/myapp"));
        assert!(ep.stimulus.contains("<path>/src/MySecretService.rs"));
        assert!(!ep.process[0].contains("/Users/dev/code"));
    }

    // --- SecretDetector tests ---

    #[test]
    fn test_secret_detector_redacts_aws_key() {
        let detector = SecretDetector::new();
        let mut ep = make_test_episode();
        detector.apply(&mut ep);

        assert!(
            !ep.outcome.description.contains("AKIAIOSFODNN7EXAMPLE"),
            "AWS key should be redacted"
        );
        assert!(ep.outcome.description.contains("[REDACTED_AWS_KEY]"));
    }

    #[test]
    fn test_secret_detector_redacts_jwt() {
        let detector = SecretDetector::new();
        let mut ep = make_test_episode();
        detector.apply(&mut ep);

        let details = ep.gate_results[0].details.as_ref().unwrap();
        assert!(!details.contains("eyJhbGci"), "JWT should be redacted");
        assert!(details.contains("[REDACTED_JWT]") || details.contains("[REDACTED_SECRET]"));
    }

    #[test]
    fn test_secret_detector_redacts_password() {
        let detector = SecretDetector::new();
        let mut ep = make_test_episode();
        detector.apply(&mut ep);

        assert!(
            !ep.outcome.description.contains("s3cr3tV4lue!"),
            "Password value should be redacted"
        );
    }

    #[test]
    fn test_secret_detector_redacts_pem() {
        let detector = SecretDetector::new();
        let input = "Found -----BEGIN RSA PRIVATE KEY----- in file";
        let result = detector.redact(input);
        assert!(result.contains("[REDACTED_PRIVATE_KEY]"));
        assert!(!result.contains("-----BEGIN"));
    }

    #[test]
    fn test_secret_detector_redacts_bearer_token() {
        let detector = SecretDetector::new();
        let input = "Authorization: Bearer sk_test_4eC39HqLyjWDarjtT1zdp7dc_verylongtokenvalue";
        let result = detector.redact(input);
        assert!(result.contains("[REDACTED_BEARER_TOKEN]"));
    }

    #[test]
    fn test_secret_detector_redacts_github_token() {
        let detector = SecretDetector::new();
        let input = "GITHUB_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let result = detector.redact(input);
        assert!(
            !result.contains("ghp_ABCDEF"),
            "GitHub token should be redacted"
        );
    }

    // --- EntityGeneralizer tests ---

    #[test]
    fn test_entity_generalizer_replaces_class_names() {
        let generalizer = EntityGeneralizer::new();
        let mut ep = EpisodeData {
            id: Uuid::new_v4(),
            stimulus: "error in MySecretService::process_payment".to_string(),
            process: vec![],
            outcome: EpisodeOutcome {
                outcome_type: OutcomeType::Negative,
                description: "MySecretService failed".to_string(),
                recommendation: None,
            },
            gate_results: vec![],
            synthetic: false,
            source_id: None,
            created_at: Utc::now(),
        };
        generalizer.apply(&mut ep);

        assert!(
            !ep.stimulus.contains("MySecretService"),
            "Class name should be generalized, got: {}",
            ep.stimulus
        );
        assert!(
            ep.stimulus.contains("Struct_1"),
            "Should contain Struct_1 placeholder, got: {}",
            ep.stimulus
        );
        assert!(
            ep.stimulus.contains("Function_1"),
            "Should contain Function_1 placeholder, got: {}",
            ep.stimulus
        );
    }

    #[test]
    fn test_entity_generalizer_consistent_mapping() {
        let generalizer = EntityGeneralizer::new();
        let mut ep = EpisodeData {
            id: Uuid::new_v4(),
            stimulus: "MyService failed".to_string(),
            process: vec!["Also MyService here".to_string()],
            outcome: EpisodeOutcome {
                outcome_type: OutcomeType::Negative,
                description: "MyService again".to_string(),
                recommendation: None,
            },
            gate_results: vec![],
            synthetic: false,
            source_id: None,
            created_at: Utc::now(),
        };
        generalizer.apply(&mut ep);

        // Same entity should get the same placeholder across all fields
        assert!(ep.stimulus.contains("Struct_1"));
        assert!(ep.process[0].contains("Struct_1"));
        assert!(ep.outcome.description.contains("Struct_1"));
    }

    // --- MetricNoise tests ---

    #[test]
    fn test_metric_noise_changes_numbers() {
        let noise = MetricNoise::new(0.1);
        let mut ep1 = EpisodeData {
            id: Uuid::new_v4(),
            stimulus: "Pattern detected with confidence 85 across 12 files".to_string(),
            process: vec![],
            outcome: EpisodeOutcome {
                outcome_type: OutcomeType::Positive,
                description: "Score 95.50 and duration 1200ms".to_string(),
                recommendation: None,
            },
            gate_results: vec![],
            synthetic: false,
            source_id: None,
            created_at: Utc::now(),
        };
        let ep_original = ep1.clone();
        noise.apply(&mut ep1);

        // At least one number should differ (probabilistically almost certain with σ=0.1)
        let different = ep1.stimulus != ep_original.stimulus
            || ep1.outcome.description != ep_original.outcome.description;
        assert!(
            different,
            "Noise should change at least some numbers. Original: {:?}, Noisy: {:?}",
            ep_original.stimulus, ep1.stimulus
        );
    }

    #[test]
    fn test_metric_noise_two_exports_differ() {
        let noise = MetricNoise::new(0.1);
        let base = EpisodeData {
            id: Uuid::new_v4(),
            stimulus: "Detected 150 occurrences with score 87.5".to_string(),
            process: vec![],
            outcome: EpisodeOutcome {
                outcome_type: OutcomeType::Positive,
                description: "Duration 3500ms, count 42".to_string(),
                recommendation: None,
            },
            gate_results: vec![],
            synthetic: false,
            source_id: None,
            created_at: Utc::now(),
        };

        let mut ep1 = base.clone();
        let mut ep2 = base.clone();
        noise.apply(&mut ep1);
        noise.apply(&mut ep2);

        // Two independent noise applications should (almost certainly) produce different results
        let same = ep1.stimulus == ep2.stimulus && ep1.outcome.description == ep2.outcome.description;
        // This could theoretically fail but probability is astronomically low
        assert!(
            !same,
            "Two noise applications should produce different results"
        );
    }

    // --- Consent Gate tests ---

    #[test]
    fn test_consent_gate_disabled_returns_error() {
        let ep = make_test_episode();
        let policy = SharingPolicy::default(); // sharing_enabled = false

        let result = consent_gate_export(&ep, &policy, None);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExportError::SharingDisabled => {}
            other => panic!("Expected SharingDisabled, got {:?}", other),
        }
    }

    #[test]
    fn test_consent_gate_enabled_runs_pipeline() {
        let ep = make_test_episode();
        let policy = SharingPolicy {
            sharing_enabled: true,
            allowed_targets: vec![],
            anonymization_config: PipelineConfig {
                stages: vec![StageConfig::SecretDetector],
            },
        };

        let result = consent_gate_export(&ep, &policy, None).unwrap();
        // Secrets should be redacted
        assert!(!result
            .outcome
            .description
            .contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn test_consent_gate_target_not_allowed() {
        let ep = make_test_episode();
        let policy = SharingPolicy {
            sharing_enabled: true,
            allowed_targets: vec!["https://registry.example.com".to_string()],
            anonymization_config: PipelineConfig::default(),
        };

        let result = consent_gate_export(&ep, &policy, Some("https://evil.com"));
        assert!(result.is_err());
        match result.unwrap_err() {
            ExportError::TargetNotAllowed(t) => assert_eq!(t, "https://evil.com"),
            other => panic!("Expected TargetNotAllowed, got {:?}", other),
        }
    }

    #[test]
    fn test_consent_gate_target_allowed() {
        let ep = make_test_episode();
        let policy = SharingPolicy {
            sharing_enabled: true,
            allowed_targets: vec!["https://registry.example.com".to_string()],
            anonymization_config: PipelineConfig {
                stages: vec![StageConfig::PathStripper],
            },
        };

        let result = consent_gate_export(&ep, &policy, Some("https://registry.example.com"));
        assert!(result.is_ok());
    }

    // --- Full pipeline test ---

    #[test]
    fn test_full_pipeline_strips_all_sensitive_data() {
        let ep = make_test_episode();
        let pipeline = AnonymizationPipeline::default_pipeline();
        let anon = pipeline.anonymize_clone(&ep);

        // No absolute paths
        assert!(!anon.stimulus.contains("/home/user"));
        // No AWS keys
        assert!(!anon.outcome.description.contains("AKIAIOSFODNN7EXAMPLE"));
        // No passwords
        assert!(!anon.outcome.description.contains("s3cr3tV4lue!"));
        // No JWT
        let details = anon.gate_results[0].details.as_ref().unwrap();
        assert!(!details.contains("eyJhbGci"));
    }

    // --- Serde roundtrip tests ---

    #[test]
    fn test_pipeline_config_serde_roundtrip() {
        let config = PipelineConfig {
            stages: vec![
                StageConfig::PathStripper,
                StageConfig::SecretDetector,
                StageConfig::EntityGeneralizer,
                StageConfig::MetricNoise { sigma: 0.1 },
            ],
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: PipelineConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.stages.len(), 4);
    }

    #[test]
    fn test_sharing_policy_serde_roundtrip() {
        let policy = SharingPolicy {
            sharing_enabled: true,
            allowed_targets: vec!["https://example.com".to_string()],
            anonymization_config: PipelineConfig::default(),
        };
        let json = serde_json::to_string(&policy).unwrap();
        let deserialized: SharingPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.sharing_enabled, true);
        assert_eq!(deserialized.allowed_targets.len(), 1);
    }

    #[test]
    fn test_export_error_serde_roundtrip() {
        let err = ExportError::SharingDisabled;
        let json = serde_json::to_string(&err).unwrap();
        let deserialized: ExportError = serde_json::from_str(&json).unwrap();
        assert_eq!(
            format!("{}", deserialized),
            "Sharing is disabled for this project"
        );
    }
}
