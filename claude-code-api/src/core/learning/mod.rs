/// Self-evolving knowledge system with mutation critic.
///
/// # References
/// - EvoFSM (2026) — "Controllable Self-Evolution for Deep Research with FSMs"
///   Separates Flow optimization and Skill optimization with a pre-evaluation critic.
pub mod critic;
pub mod evolve;
pub mod types;

pub use critic::{GraphBasedCritic, MutationCritic};
pub use evolve::EvolutionEngine;
pub use types::*;
