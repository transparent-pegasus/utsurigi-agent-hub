# Current practices

Read for feature/policy decisions or reviews. Review periodically and on capability
changes; use `review.reviewer_bot` when configured. Preserve data externalization.
Review everything else: shared knowledge within each hub, principles, practices,
settings, implementation choices, and enabled or disabled optional features.
Propose additions, changes, or removals for user approval.

- Prefer native Bot features. Optionally adopt alternatives that provide missing
  capabilities or improve native ones while preserving compatibility.
  During setup, set an optional feature's default to `false` when the Bot already
  covers it and it adds no missing capability. Preserve explicit user
  choices, including `true`, and never turn off native systems with this rule.
- Prefer native Bot scheduling for periodic tasks, including reviews and sync.
  Set review timing there. Reuse its server cron wake mechanism when the local host sleeps.
- Use Rust unless specified otherwise.
- Use capable host connectors first; otherwise use authorized computer automation. Keep secret delivery secure.
- Keep prose brief. Install Caveman only when enabled.
- Let restic retry transient transport failures within configured limits. Host
  reads may use bounded exponential backoff with jitter. Never blindly replay an
  uncertain write; reconcile first. Authentication, configuration, and integrity
  failures need correction, not retries.
