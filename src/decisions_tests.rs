use crate::decisions::bundled_default;
use serde_json::{Value, json};

const APPROVED_DIGEST: &str = "85db6f9972ed7cb33b710b049673f3bf7d86c8b4d122668b66dd9aa34c9f9b81";

#[test]
fn bundled_default_preserves_the_confirmed_scoped_decision_and_provenance() {
    let record = match bundled_default("default", APPROVED_DIGEST) {
        Ok(Some(record)) => record,
        Ok(None) => panic!("the exact approved scope must have its bundled decision"),
        Err(_) => panic!("the approved bundled record must be valid"),
    };
    let mut actual = serde_json::to_value(record).expect("typed decisions must serialize");
    // The optional original timestamp may serialize as null or be omitted.
    let original = actual["caveman"]
        .as_object_mut()
        .unwrap()
        .remove("original_decision_at");
    assert!(original.is_none_or(|value| value == Value::Null));
    assert_eq!(
        actual,
        json!({
            "version": 1,
            "hub": "default",
            "repository_sha256": APPROVED_DIGEST,
            "caveman": {
                "enabled": true,
                "basis": "user_decision",
                "source": "Kunimaru: Q4=A; default hub Caveman=true, relayed in this ownerless-hub session by Shotoku Taishi / Hashida Itaru",
                "recorded_by": "uah-own-orch",
                "recorded_at": "2026-11-04T08:15:37Z"
            }
        })
    );
}

#[test]
fn bundled_default_never_supplies_consent_for_another_hub_or_digest() {
    let wrong_digest = "0".repeat(64);
    let uppercase_digest = APPROVED_DIGEST.to_uppercase();
    for (hub, digest) in [
        ("default", wrong_digest.as_str()),
        ("default", uppercase_digest.as_str()),
        ("Default", APPROVED_DIGEST),
        ("default-other", APPROVED_DIGEST),
        ("muse", APPROVED_DIGEST),
    ] {
        match bundled_default(hub, digest) {
            Ok(None) => {}
            Ok(Some(_)) => panic!("bundled consent escaped its exact scope: {hub}/{digest}"),
            Err(_) => panic!("scope mismatch must return None: {hub}/{digest}"),
        }
    }
}
