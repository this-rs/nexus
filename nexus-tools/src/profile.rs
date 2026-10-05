//! Which tools a session may use.

use std::collections::BTreeSet;

/// The tools a profile names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSet {
    /// Every tool the server has. Only the explicit development flag produces it: a signed
    /// token always **names** its tools.
    All,
    /// Exactly these tools.
    Only(BTreeSet<String>),
}

/// A session's identity and the tools it may see (decision A35: a tool outside the
/// profile is never listed and never run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// Session identifier, from the token. Keys the session's state.
    pub session_id: String,
    /// The tools the session may use.
    pub tools: ToolSet,
}

impl Profile {
    /// A profile limited to `tools`.
    pub fn only<I, S>(session_id: impl Into<String>, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            session_id: session_id.into(),
            tools: ToolSet::Only(tools.into_iter().map(Into::into).collect()),
        }
    }

    /// Every tool. Development only.
    pub fn unrestricted(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            tools: ToolSet::All,
        }
    }

    /// Whether `name` is one of the tools of this profile.
    pub fn allows(&self, name: &str) -> bool {
        match &self.tools {
            ToolSet::All => true,
            ToolSet::Only(names) => names.contains(name),
        }
    }
}
