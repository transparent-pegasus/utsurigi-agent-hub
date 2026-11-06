//! Live check timing, defined per Bot in its live list.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

const MIN_SECONDS: u64 = 60;
const MAX_SECONDS: u64 = 86_400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub base_seconds: u64,
    pub idle_after_seconds: u64,
    pub task_idle_seconds: u64,
    pub rule_idle_seconds: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            base_seconds: 60,
            idle_after_seconds: 600,
            task_idle_seconds: 300,
            rule_idle_seconds: 600,
        }
    }
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        let base = self.base_seconds;
        let others = [
            self.idle_after_seconds,
            self.task_idle_seconds,
            self.rule_idle_seconds,
        ];
        if !(MIN_SECONDS..=MAX_SECONDS).contains(&base)
            || others
                .iter()
                .any(|value| !(base..=MAX_SECONDS).contains(value))
        {
            return Err(Error::new(
                "Live policy needs base_seconds 60-86400 and other values from base_seconds to 86400",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_the_owner_decision_and_is_valid() {
        let policy = Policy::default();
        assert_eq!(
            (
                policy.base_seconds,
                policy.idle_after_seconds,
                policy.task_idle_seconds,
                policy.rule_idle_seconds
            ),
            (60, 600, 300, 600)
        );
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn validate_enforces_bounds() {
        let with = |base, idle_after, task, rule| Policy {
            base_seconds: base,
            idle_after_seconds: idle_after,
            task_idle_seconds: task,
            rule_idle_seconds: rule,
        };
        assert!(with(60, 60, 60, 60).validate().is_ok());
        assert!(with(86_400, 86_400, 86_400, 86_400).validate().is_ok());
        for invalid in [
            with(59, 600, 300, 600),
            with(86_401, 86_401, 86_401, 86_401),
            with(120, 60, 300, 600),
            with(120, 600, 60, 600),
            with(120, 600, 300, 60),
            with(60, 86_401, 300, 600),
        ] {
            assert!(invalid.validate().is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let error = serde_json::from_str::<Policy>(
            r#"{"base_seconds":60,"idle_after_seconds":600,"task_idle_seconds":300,"rule_idle_seconds":600,"extra":1}"#,
        );
        assert!(error.is_err());
    }
}
