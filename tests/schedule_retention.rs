use chrono::DateTime;
use std::collections::BTreeSet;
use utsurigi_agent_hub::{retention, schedule};

fn timestamp(value: &str) -> i64 {
    DateTime::parse_from_rfc3339(value).unwrap().timestamp()
}

#[test]
fn first_anchor_uses_local_0300_and_includes_an_exact_occurrence() {
    for (now, timezone, expected) in [
        ("2026-10-30T00:00:00Z", "UTC", "2026-10-30T03:00:00Z"),
        ("2026-10-30T03:00:00Z", "UTC", "2026-10-30T03:00:00Z"),
        ("2026-10-30T03:00:01Z", "UTC", "2026-10-31T03:00:00Z"),
        ("2026-10-29T17:59:59Z", "Asia/Tokyo", "2026-10-29T18:00:00Z"),
    ] {
        assert_eq!(
            schedule::first_anchor(timestamp(now), timezone, "03:00").unwrap(),
            timestamp(expected),
            "{now} in {timezone}"
        );
    }
}

#[test]
fn nonexistent_wall_time_skips_the_date() {
    assert_eq!(
        schedule::first_anchor(
            timestamp("2026-03-08T06:00:00Z"),
            "America/New_York",
            "02:30"
        )
        .unwrap(),
        timestamp("2026-03-09T06:30:00Z")
    );
}

#[test]
fn folded_wall_time_chooses_the_first_occurrence_still_in_the_future() {
    for (now, expected) in [
        ("2026-11-01T05:00:00Z", "2026-11-01T05:30:00Z"),
        ("2026-11-01T05:30:01Z", "2026-11-01T06:30:00Z"),
        ("2026-11-01T06:30:01Z", "2026-11-02T06:30:00Z"),
    ] {
        assert_eq!(
            schedule::first_anchor(timestamp(now), "America/New_York", "01:30").unwrap(),
            timestamp(expected)
        );
    }
}

#[test]
fn invalid_schedule_inputs_return_errors() {
    for (timezone, start) in [
        ("Invalid/Timezone", "03:00"),
        ("UTC", "24:00"),
        ("UTC", "03:60"),
        ("UTC", ""),
    ] {
        assert!(schedule::first_anchor(0, timezone, start).is_err());
    }
    assert!(schedule::first_anchor(i64::MAX, "UTC", "03:00").is_err());
}

#[test]
fn next_due_stays_on_the_86400_second_anchor_grid() {
    for (anchor, now, expected) in [
        (100_000, 99_999, 100_000),
        (100_000, 100_000, 186_400),
        (100_000, 186_399, 186_400),
        (100_000, 186_400, 272_800),
        (100_000, 359_201, 445_600),
        (-100_000, -100_001, -100_000),
        (-100_000, -100_000, -13_600),
        (-100_000, 0, 72_800),
    ] {
        assert_eq!(schedule::next_after(anchor, 86_400, now), Ok(expected));
    }
}

#[test]
fn fixed_second_intervals_do_not_drift_at_daylight_saving_transitions() {
    let anchor = timestamp("2026-03-07T08:00:00Z");
    assert_eq!(
        schedule::next_after(anchor, 86_400, timestamp("2026-03-08T08:00:00Z")),
        Ok(timestamp("2026-03-09T08:00:00Z"))
    );
}

#[test]
fn zero_interval_and_unrepresentable_grid_points_fail() {
    assert!(schedule::next_after(100, 0, 0).is_err());
    assert!(schedule::next_after(i64::MAX - 10, 20, i64::MAX - 1).is_err());
    assert!(schedule::next_after(0, u64::MAX, 1).is_err());
    assert!(schedule::next_after(0, 1, i64::MAX).is_err());
}

fn versions(values: &[(&str, Option<i64>)]) -> Vec<retention::Version> {
    values
        .iter()
        .map(|(id, completed_at)| retention::Version {
            id: (*id).to_owned(),
            completed_at: *completed_at,
        })
        .collect()
}

fn expired(values: &[(&str, Option<i64>)], now: i64, seconds: u64) -> BTreeSet<String> {
    let ids = retention::expired(&versions(values), now, seconds);
    let unique: BTreeSet<_> = ids.iter().cloned().collect();
    assert_eq!(ids.len(), unique.len(), "duplicate deletion candidate");
    unique
}

fn ids(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn retention_is_measured_from_the_successor_not_the_old_backup() {
    let history = [("old", Some(-1_000_000)), ("current", Some(1_000))];
    assert_eq!(expired(&history, 1_099, 100), ids(&[]));
    assert_eq!(expired(&history, 1_100, 100), ids(&["old"]));
    assert_eq!(expired(&history, 1_101, 100), ids(&["old"]));
}

#[test]
fn each_version_uses_its_own_successor_and_input_order_is_irrelevant() {
    let a = ("old", Some(0));
    let b = ("middle", Some(100));
    let c = ("current", Some(180));
    for history in [
        [a, b, c],
        [a, c, b],
        [b, a, c],
        [b, c, a],
        [c, a, b],
        [c, b, a],
    ] {
        assert_eq!(expired(&history, 199, 100), ids(&[]));
        assert_eq!(expired(&history, 200, 100), ids(&["old"]));
        assert_eq!(expired(&history, 280, 100), ids(&["old", "middle"]));
    }
}

#[test]
fn incomplete_or_exit_3_backups_never_supersede_a_success() {
    let history = [
        ("current", Some(0)),
        ("exit-3", None),
        ("interrupted", None),
    ];
    assert_eq!(expired(&history, 1_000_000, 1), ids(&[]));
    let history = [("old", Some(0)), ("exit-3", None), ("current", Some(100))];
    assert_eq!(expired(&history, 199, 100), ids(&[]));
    assert_eq!(expired(&history, 200, 100), ids(&["old"]));
}

#[test]
fn latest_empty_and_incomplete_histories_are_never_deleted() {
    assert_eq!(expired(&[], 100, 0), ids(&[]));
    assert_eq!(expired(&[("only", Some(0))], 100, 0), ids(&[]));
    assert_eq!(expired(&[("partial", None)], 100, 0), ids(&[]));
}

#[test]
fn tied_successes_are_preserved_even_when_a_later_success_exists() {
    assert_eq!(
        expired(&[("a", Some(100)), ("b", Some(100))], 1_000, 1),
        ids(&[])
    );
    assert_eq!(
        expired(
            &[("a", Some(100)), ("b", Some(100)), ("latest", Some(200))],
            1_000,
            1
        ),
        ids(&[])
    );
}

#[test]
fn retention_does_not_expire_future_successes_or_overflow_large_durations() {
    let history = [("old", Some(i64::MIN)), ("current", Some(i64::MAX))];
    assert_eq!(expired(&history, 0, 1), ids(&[]));
    assert_eq!(expired(&history, i64::MAX, u64::MAX), ids(&[]));
    assert_eq!(
        expired(&[("old", Some(0)), ("current", Some(100))], 99, 0),
        ids(&[])
    );
}
