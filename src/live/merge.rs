//! Three-way merge plan and generated file names.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base {
    pub object: String,
    pub plain: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Push(String),
    Delete(String),
    Pull {
        path: String,
        object: String,
    },
    Remove(String),
    Conflict {
        path: String,
        remote: Option<String>,
    },
    Forget(String),
}

/// `remote` maps path to object SHA-256, `local` maps path to plaintext SHA-256, and
/// `base` is what this machine last applied. Returns one action per changed path, by path.
pub fn plan(
    base: &BTreeMap<String, Base>,
    remote: &BTreeMap<String, String>,
    local: &BTreeMap<String, String>,
) -> Vec<Action> {
    let paths: BTreeSet<&String> = base
        .keys()
        .chain(remote.keys())
        .chain(local.keys())
        .collect();
    paths
        .into_iter()
        .filter_map(|path| {
            let applied = base.get(path);
            let remote = remote.get(path);
            let local = local.get(path);
            let remote_changed = remote != applied.map(|base| &base.object);
            let local_changed = local != applied.map(|base| &base.plain);
            let path = path.clone();
            Some(match (remote_changed, local_changed, remote, local) {
                (false, false, ..) => return None,
                (false, true, _, Some(_)) => Action::Push(path),
                (false, true, _, None) => Action::Delete(path),
                (true, false, Some(object), _) => Action::Pull {
                    path,
                    object: object.clone(),
                },
                (true, false, None, _) => Action::Remove(path),
                // Both changed: the remote version keeps the path, and a local edit moves to a
                // conflict copy.
                (true, true, Some(object), None) => Action::Pull {
                    path,
                    object: object.clone(),
                },
                (true, true, None, None) => Action::Forget(path),
                (true, true, remote, Some(_)) => Action::Conflict {
                    path,
                    remote: remote.cloned(),
                },
            })
        })
        .collect()
}

pub fn conflict_name(path: &str, sha: &str) -> String {
    let (stem, extension) = split_extension(path);
    format!("{stem}.conflict-{}{extension}", &sha[..8])
}

// shortcut: unused until `live request` lands (plan Task 6); remove the allowance then.
#[allow(dead_code)]
pub fn request_name(root: &str, requester: &str, sha: &str, source: &str) -> String {
    let (_, extension) = split_extension(source);
    format!("{root}{requester}-{}{extension}", &sha[..8])
}

/// Splits before the last extension of the final component. A leading dot starts no extension.
fn split_extension(path: &str) -> (&str, &str) {
    let name = path.rfind('/').map_or(0, |slash| slash + 1);
    let dot = path[name..]
        .rfind('.')
        .filter(|&dot| dot > 0)
        .map_or(path.len(), |dot| name + dot);
    path.split_at(dot)
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
