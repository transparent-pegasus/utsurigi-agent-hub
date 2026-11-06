//! Check interval: decay after idle, failure backoff, jitter.

use crate::live::policy::Policy;

pub fn interval(policy: &Policy, has_tasks: bool, now: i64, changed_at: Option<i64>) -> u64 {
    // A clock that stepped back leaves `changed_at` ahead of `now`; that still counts as recent.
    let recent = changed_at.is_some_and(|changed_at| {
        i128::from(now) - i128::from(changed_at) < i128::from(policy.idle_after_seconds)
    });
    if recent {
        policy.base_seconds
    } else {
        decayed(policy, has_tasks)
    }
}

pub fn backoff(policy: &Policy, has_tasks: bool, failures: u32) -> u64 {
    policy
        .base_seconds
        .saturating_mul(2u64.saturating_pow(failures))
        .min(decayed(policy, has_tasks))
}

/// Spreads `seconds` by ±10%. `unit` is a uniform sample from [0, 1).
pub fn jitter(seconds: u64, unit: f64) -> u64 {
    // max/min rather than clamp: NaN becomes 0 instead of passing through.
    let unit = unit.max(0.0).min(1.0_f64.next_down());
    ((seconds as f64 * (0.9 + 0.2 * unit)).round() as u64).max(1)
}

fn decayed(policy: &Policy, has_tasks: bool) -> u64 {
    if has_tasks {
        policy.task_idle_seconds
    } else {
        policy.rule_idle_seconds
    }
}

#[cfg(test)]
#[path = "cadence_tests.rs"]
mod tests;
