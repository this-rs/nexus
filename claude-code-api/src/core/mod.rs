pub mod auth;
pub mod cache;
pub mod claude_manager;
pub mod config;
pub mod conversation;
pub mod hooks;
pub mod interactive_session;
pub mod memory;
pub mod model_registry;
pub mod objective_tracker;
pub mod process_pool;
pub mod retry;
pub mod session_manager;
pub mod storage;

/// Installing an executable without racing our own `fork`s. See the module docs.
#[cfg(test)]
pub(crate) mod test_exec;
