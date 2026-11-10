use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Grok {
    id: String,
    content: String,
    status: Status,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Checklist,
    Tracking,
    Goal,
    Schedule,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Muse {
    id: String,
    kind: Kind,
    title: String,
    status: Status,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    due: Option<String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    requires: Vec<Resource>,
    #[serde(default)]
    start_after: Vec<String>,
    #[serde(default)]
    done_when: Vec<String>,
    #[serde(default)]
    next: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    goal: Option<String>,
    #[serde(default)]
    activity: Option<String>,
}

pub(super) fn import(
    config: &Config,
    root: &Path,
    old: &Collection,
    format: NativeFormat,
    file: &Path,
) -> Result<Value> {
    let bot = config
        .bot
        .id
        .as_ref()
        .ok_or_else(|| Error::new("Import requires bot.id to scope native task IDs"))?;
    let bytes = read_regular(file)?;
    let system = match format {
        NativeFormat::Grok => "grok",
        NativeFormat::Muse => "muse",
    };
    let records: Vec<Muse> = match format {
        NativeFormat::Grok => {
            let grok: Vec<Grok> = serde_json::from_str(&bytes).map_err(|_| {
                Error::new("Invalid Grok task export; expected id/content/status records")
            })?;
            let mut records = Vec::new();
            for g in grok {
                if matches!(g.status, Status::Waiting | Status::WaitingUser) {
                    return Err(Error::new(
                        "Grok status must be pending, in_progress, completed or cancelled",
                    ));
                }
                // Grok content is free text, so keep it intact in the Markdown body.
                let title = g
                    .content
                    .lines()
                    .find(|s| text(s))
                    .ok_or_else(|| Error::new("Grok task content needs nonempty text"))?
                    .to_owned();
                records.push(Muse {
                    id: g.id,
                    kind: Kind::Checklist,
                    title,
                    status: g.status,
                    parent: None,
                    schedule: None,
                    due: None,
                    workdir: None,
                    requires: vec![],
                    start_after: vec![],
                    done_when: vec![],
                    next: g.content,
                    note: String::new(),
                    goal: None,
                    activity: None,
                });
            }
            records
        }
        NativeFormat::Muse => serde_json::from_str(&bytes).map_err(|_| {
            Error::new(
                "Invalid Muse task export; normalize native records as documented in TASKS.md",
            )
        })?,
    };
    let mut ids = BTreeMap::new();
    let mut sources = BTreeSet::new();
    let existing: BTreeMap<_, _> = old
        .documents
        .values()
        .filter_map(|d| {
            d.task
                .source
                .as_ref()
                .map(|s| (s.clone(), d.task.id.clone()))
        })
        .collect();
    for record in &records {
        if !text(&record.id) || !sources.insert(record.id.clone()) {
            return Err(Error::new("Empty or duplicate native task ID"));
        }
        let source = Source {
            system: system.to_owned(),
            bot: bot.clone(),
            id: record.id.clone(),
        };
        ids.insert(
            record.id.clone(),
            match existing.get(&source) {
                Some(id) => id.clone(),
                None => new_id()?,
            },
        );
    }
    let resolve = |native: &str| -> Result<String> {
        ids.get(native)
            .cloned()
            .or_else(|| {
                existing
                    .get(&Source {
                        system: system.to_owned(),
                        bot: bot.clone(),
                        id: native.to_owned(),
                    })
                    .cloned()
            })
            .ok_or_else(|| {
                Error::new("Unknown native task dependency; export its referenced task too")
            })
    };
    let mut documents = BTreeMap::new();
    for record in records {
        let source = Source {
            system: system.to_owned(),
            bot: bot.clone(),
            id: record.id.clone(),
        };
        if existing.contains_key(&source) {
            continue;
        }
        if matches!(record.kind, Kind::Schedule) && record.schedule.is_none() {
            return Err(Error::new(
                "Muse schedule tasks require a native schedule reference",
            ));
        }
        let mut body = String::new();
        if let Some(goal) = record.goal {
            body.push_str(&format!("## Goal\n{goal}\n\n"));
        }
        body.push_str(&format!(
            "## Next\n{}\n\n## Note\n{}\n\n## Log\n",
            record.next, record.note
        ));
        let mut content = format!("Imported {system} task.");
        if let Some(activity) = record.activity {
            content.push_str("\n  Native activity:\n");
            for line in activity.lines() {
                content.push_str(&format!("  {line}\n"));
            }
        }
        append_log(&mut body, bot, &content)?;
        let task = Task {
            id: ids[&record.id].clone(),
            parent: record.parent.as_deref().map(resolve).transpose()?,
            title: record.title,
            status: record.status,
            owner: Some(bot.clone()),
            schedule: record.schedule.map(|id| Schedule {
                bot: bot.clone(),
                id,
            }),
            due: record.due,
            workdir: record.workdir,
            requires: record.requires,
            start_after: record
                .start_after
                .iter()
                .map(|id| resolve(id))
                .collect::<Result<_>>()?,
            done_when: record
                .done_when
                .iter()
                .map(|id| resolve(id))
                .collect::<Result<_>>()?,
            updated: now(),
            source: Some(source),
        };
        let doc = Document::new(task, body)?;
        if old.documents.contains_key(&doc.task.id)
            || documents.insert(doc.task.id.clone(), doc.raw).is_some()
        {
            return Err(Error::new("Imported task UUID collision; retry the import"));
        }
    }
    let count = documents.len();
    if count > 0 {
        commit(root, old, documents, bot)?;
    }
    Ok(json!({"imported":count,"skipped":ids.len()-count,"ids":ids}))
}
