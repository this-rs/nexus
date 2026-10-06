//! The tools a server can run, and which of them a session may see.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::profile::Profile;
use crate::tool::Tool;

/// The canonical names of every tool this crate can serve, whatever the platform (`Bash`,
/// `Monitor` and `TaskStop` exist on Unix only). `--tools` accepts exactly these (plus the test
/// tools of a `test-tools` build); a name that is not served here is simply absent.
pub const CANONICAL_TOOLS: [&str; 11] = [
    "Read",
    "Write",
    "Edit",
    "NotebookEdit",
    "Glob",
    "Grep",
    "Bash",
    "Monitor",
    "TaskStop",
    "WebFetch",
    "WebSearch",
];

/// The set of tools of a server.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tool (replacing one of the same name).
    pub fn with(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.insert(tool.name().to_owned(), Arc::new(tool));
        self
    }

    /// Adds an already shared tool.
    pub fn with_arc(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.insert(tool.name().to_owned(), tool);
        self
    }

    /// Keeps only the tools named in `names` (`--tools`): the others are dropped from the
    /// server, so no profile, token or client request can reach them. A name that this registry
    /// does not hold is ignored here; checking names is the caller's job ([`CANONICAL_TOOLS`]).
    pub fn retain_only(mut self, names: &std::collections::BTreeSet<String>) -> Self {
        self.tools.retain(|name, _| names.contains(name));
        self
    }

    /// Every tool name, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// The tools `profile` may see, sorted by name. **Fail closed**: a tool the profile does
    /// not name is not listed.
    pub fn visible(&self, profile: &Profile) -> Vec<Arc<dyn Tool>> {
        self.tools
            .values()
            .filter(|tool| profile.allows(tool.name()))
            .cloned()
            .collect()
    }

    /// The tool called `name` **if the profile allows it**. A tool that does not exist and a
    /// tool that is forbidden are indistinguishable to the caller.
    pub fn resolve(&self, profile: &Profile, name: &str) -> Option<Arc<dyn Tool>> {
        if !profile.allows(name) {
            return None;
        }
        self.tools.get(name).cloned()
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.names())
            .finish()
    }
}
