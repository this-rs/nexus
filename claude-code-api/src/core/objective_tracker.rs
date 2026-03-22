//! Objective tracking for agent sessions.
//!
//! When an agent is executing a plan, this module can inject periodic
//! "system hint" reminders into the conversation to keep the agent
//! focused on its current objectives.
//!
//! The core logic is a pure function [`check_objective_reminder`] that
//! decides whether a reminder should be injected based on:
//! - Whether the last turn contained tool use (no reminder if tools were used)
//! - Whether objective tracking is enabled
//! - A cooldown mechanism to avoid spamming reminders every turn
//! - Whether there are actually pending objectives to remind about

/// Check whether an objective reminder should be injected.
///
/// Returns `Some(reminder_text)` if a reminder should be sent, `None` otherwise.
///
/// # Arguments
/// * `had_tool_use` - Whether the last assistant turn contained tool_use blocks
/// * `tracking_enabled` - Whether objective tracking is enabled for this session
/// * `turns_since_last_reminder` - How many turns since the last reminder was sent
/// * `cooldown_turns` - Minimum turns between reminders (typically 3)
/// * `pending_objectives` - Description of pending objectives (empty string = no objectives)
#[allow(dead_code)] // Will be called from session_manager once objective tracking is wired in
pub fn check_objective_reminder(
    had_tool_use: bool,
    tracking_enabled: bool,
    turns_since_last_reminder: u32,
    cooldown_turns: u32,
    pending_objectives: &str,
) -> Option<String> {
    // Gate 1: tracking must be enabled
    if !tracking_enabled {
        return None;
    }

    // Gate 2: no reminder when agent is actively using tools
    if had_tool_use {
        return None;
    }

    // Gate 3: respect cooldown period
    if turns_since_last_reminder < cooldown_turns {
        return None;
    }

    // Gate 4: no reminder if there are no pending objectives
    let objectives = pending_objectives.trim();
    if objectives.is_empty() {
        return None;
    }

    // All gates passed — build the reminder
    Some(format!(
        "[SystemHint] Reminder — you have pending objectives:\n{}",
        objectives
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test 1: When had_tool_use == false and there are pending objectives,
    /// a reminder is injected.
    #[test]
    fn test_reminder_injected_when_no_tools_and_pending_objectives() {
        let result = check_objective_reminder(
            false, // had_tool_use
            true,  // tracking_enabled
            5,     // turns_since_last_reminder (past cooldown)
            3,     // cooldown_turns
            "- Task T4: Write integration tests\n- Task T5: Update frontend",
        );

        assert!(result.is_some(), "Should inject a reminder");
        let msg = result.unwrap();
        assert!(
            msg.contains("SystemHint"),
            "Should contain SystemHint marker"
        );
        assert!(
            msg.contains("Task T4"),
            "Should contain the pending objectives"
        );
    }

    /// Test 2: When had_tool_use == true, no reminder is injected
    /// (agent is actively working, don't interrupt).
    #[test]
    fn test_no_reminder_with_tools() {
        let result = check_objective_reminder(
            true, // had_tool_use — agent used tools
            true, // tracking_enabled
            10,   // turns_since_last_reminder
            3,    // cooldown_turns
            "- Task T4: Write integration tests",
        );

        assert!(
            result.is_none(),
            "Should NOT inject reminder when tools were used"
        );
    }

    /// Test 3: Cooldown is respected — no reminder if fewer than cooldown_turns
    /// have passed since the last one.
    #[test]
    fn test_cooldown_respected() {
        // Turn 0: just sent a reminder
        let result = check_objective_reminder(false, true, 0, 3, "- Task T4: Write tests");
        assert!(
            result.is_none(),
            "Should NOT remind at turn 0 (just reminded)"
        );

        // Turn 1: still in cooldown
        let result = check_objective_reminder(false, true, 1, 3, "- Task T4: Write tests");
        assert!(result.is_none(), "Should NOT remind at turn 1");

        // Turn 2: still in cooldown
        let result = check_objective_reminder(false, true, 2, 3, "- Task T4: Write tests");
        assert!(result.is_none(), "Should NOT remind at turn 2");

        // Turn 3: cooldown expired, should remind
        let result = check_objective_reminder(false, true, 3, 3, "- Task T4: Write tests");
        assert!(
            result.is_some(),
            "Should remind at turn 3 (cooldown expired)"
        );
    }

    /// Test 4: When objective_tracking is disabled, no reminder is ever sent.
    #[test]
    fn test_disabled_tracking() {
        let result = check_objective_reminder(
            false, // had_tool_use
            false, // tracking_enabled — DISABLED
            10,    // turns_since_last_reminder
            3,     // cooldown_turns
            "- Task T4: Write integration tests",
        );

        assert!(
            result.is_none(),
            "Should NOT inject reminder when tracking is disabled"
        );
    }

    /// Test 5: When there are no pending objectives (empty string),
    /// no reminder is sent even if all other conditions are met.
    #[test]
    fn test_no_plans_pending() {
        // Empty string
        let result = check_objective_reminder(false, true, 10, 3, "");
        assert!(result.is_none(), "Should NOT remind with empty objectives");

        // Whitespace only
        let result = check_objective_reminder(false, true, 10, 3, "   \n  ");
        assert!(
            result.is_none(),
            "Should NOT remind with whitespace-only objectives"
        );
    }
}
