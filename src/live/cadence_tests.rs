//! Tests for live cadence.

use super::{backoff, interval, jitter};
use crate::live::policy::Policy;

const NOW: i64 = 1_794_000_000;
// The defaults share 600 between idle_after and rule_idle; distinct values expose a swapped field.
const DISTINCT: Policy = Policy {
    base_seconds: 90,
    idle_after_seconds: 1_000,
    task_idle_seconds: 400,
    rule_idle_seconds: 700,
};

#[test]
fn interval_uses_base_until_idle_after() {
    let defaults = Policy::default();
    for (policy, has_tasks, changed_at, expected) in [
        (defaults, true, Some(NOW - 599), 60),
        (defaults, false, Some(NOW - 599), 60),
        (defaults, true, Some(NOW - 600), 300),
        (defaults, false, Some(NOW - 600), 600),
        (defaults, true, None, 300),
        (defaults, false, None, 600),
        (defaults, true, Some(NOW + 30), 60),
        (defaults, false, Some(NOW + 30), 60),
        (DISTINCT, true, Some(NOW - 999), 90),
        (DISTINCT, true, Some(NOW - 1_000), 400),
        (DISTINCT, false, Some(NOW - 1_000), 700),
        (DISTINCT, false, None, 700),
    ] {
        assert_eq!(
            interval(&policy, has_tasks, NOW, changed_at),
            expected,
            "{policy:?} has_tasks={has_tasks} changed_at={changed_at:?}"
        );
    }
}

#[test]
fn backoff_doubles_and_caps() {
    let defaults = Policy::default();
    for (policy, has_tasks, failures, expected) in [
        (defaults, true, 0, 60),
        (defaults, true, 1, 120),
        (defaults, true, 2, 240),
        (defaults, true, 3, 300),
        (defaults, false, 3, 480),
        (defaults, false, 4, 600),
        (DISTINCT, true, 1, 180),
        (DISTINCT, false, 3, 700),
        // 63 overflows only the multiplication; larger counts also overflow the power.
        (defaults, true, 63, 300),
        (defaults, true, u32::MAX, 300),
        (defaults, false, u32::MAX, 600),
    ] {
        assert_eq!(
            backoff(&policy, has_tasks, failures),
            expected,
            "{policy:?} has_tasks={has_tasks} failures={failures}"
        );
    }
}

#[test]
fn jitter_stays_within_ten_percent() {
    for (seconds, unit, expected) in [
        (60, 0.0, 54),
        (60, 0.5, 60),
        (60, 0.999, 66),
        (60, -1.0, 54),
        (60, 2.0, 66),
        (600, 0.25, 570),
        (0, 0.5, 1),
    ] {
        assert_eq!(jitter(seconds, unit), expected, "jitter({seconds}, {unit})");
    }
}
