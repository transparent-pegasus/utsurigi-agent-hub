use std::collections::BTreeMap;

pub struct Version {
    pub id: String,
    pub completed_at: Option<i64>,
}

pub fn expired(versions: &[Version], now: i64, retention_seconds: u64) -> Vec<String> {
    let mut completed: BTreeMap<i64, Vec<&Version>> = BTreeMap::new();
    let mut occurrences = BTreeMap::new();
    for version in versions {
        *occurrences.entry(version.id.as_str()).or_insert(0) += 1;
        if let Some(time) = version.completed_at {
            completed.entry(time).or_default().push(version);
        }
    }
    let ordered: Vec<_> = completed.into_iter().collect();
    ordered
        .windows(2)
        .filter(|pair| {
            pair[0].1.len() == 1
                && pair[1].1.len() == 1
                && occurrences[pair[0].1[0].id.as_str()] == 1
                && i128::from(now) - i128::from(pair[1].0) >= i128::from(retention_seconds)
        })
        .map(|pair| pair[0].1[0].id.clone())
        .collect()
}
