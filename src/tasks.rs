use crate::config::{Config, identity, no_symlinks, parse_yaml};
use crate::error::{Error, Result};
use crate::state::{Lock, private_dir, write_private};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

#[path = "task_import.rs"]
mod migration;

const FORMAT: &str = "utsurigi-agent-hub/tasks/1\n";
const JOURNAL: &str = ".pending.json";
const REQUIRED: &str = "Task management is required; preserve native records, run tasks init and import them before setup or publication";

#[derive(Subcommand)]
pub enum TaskAction {
    /// Initialize the required shared task store without overwriting native files.
    Init,
    Create {
        title: String,
    },
    /// Read in_progress tasks only; other states require an explicit selection.
    List {
        #[arg(long, conflicts_with = "status")]
        all: bool,
        #[arg(long, value_enum)]
        status: Option<Status>,
        /// Read the verified shared cache instead of local authoring drafts.
        #[arg(long)]
        accepted: bool,
    },
    Show {
        id: String,
        #[arg(long)]
        accepted: bool,
    },
    /// Create a task or replace it using the ETag returned by show.
    Put {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        if_match: Option<String>,
    },
    Validate,
    /// Regenerate the in_progress-only INDEX.
    Index,
    /// Import an explicit native JSON export without changing its source.
    Import {
        #[arg(value_enum)]
        format: NativeFormat,
        #[arg(long)]
        file: PathBuf,
    },
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Waiting,
    WaitingUser,
    Completed,
    Cancelled,
}

impl Status {
    fn closed(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Waiting => "waiting",
            Self::WaitingUser => "waiting_user",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub enum NativeFormat {
    Grok,
    Muse,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    bot: String,
    id: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
enum Resource {
    Repo { repo: String },
    Secret { secret: String },
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct Source {
    system: String,
    bot: String,
    id: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Task {
    id: String,
    #[serde(default)]
    parent: Option<String>,
    title: String,
    status: Status,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    schedule: Option<Schedule>,
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
    updated: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<Source>,
}

#[derive(Clone)]
struct Document {
    task: Task,
    body: String,
    raw: String,
}

impl Document {
    fn parse(raw: String) -> Result<Self> {
        let mut lines = raw.split_inclusive('\n');
        let first = lines.next().unwrap_or("");
        if first.trim_end_matches(['\r', '\n']) != "---" {
            return Err(Error::new("Task requires YAML frontmatter"));
        }
        let start = first.len();
        let mut end = start;
        for line in lines {
            if line.trim_end_matches(['\r', '\n']) == "---" {
                let task: Task =
                    parse_yaml(&raw.as_bytes()[start..end], "Invalid task YAML schema")?;
                task.validate()?;
                let body = raw[end + line.len()..].to_owned();
                return Ok(Self { task, body, raw });
            }
            end += line.len();
        }
        Err(Error::new("Unterminated task frontmatter"))
    }

    fn new(task: Task, body: String) -> Result<Self> {
        let yaml =
            serde_saphyr::to_string(&task).map_err(|_| Error::new("Cannot encode task YAML"))?;
        Self::parse(format!("---\n{yaml}---\n{body}"))
    }

    fn etag(&self) -> String {
        format!("{:x}", Sha256::digest(self.raw.as_bytes()))
    }

    fn view(&self) -> Value {
        json!({"task":self.task,"body":self.body,"etag":self.etag()})
    }
}

fn text(value: &str) -> bool {
    !value.trim().is_empty() && !value.chars().any(char::is_control)
}

fn uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

fn new_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn section(body: &str, heading: &str) -> Option<(usize, usize)> {
    let mut start = None;
    let mut found = None;
    let mut offset = 0;
    let mut fence: Option<(u8, usize)> = None;
    for line in body.split_inclusive('\n') {
        let title = line.trim_end_matches(['\r', '\n']);
        let trimmed = title.trim_start_matches(' ');
        let marker = trimmed.as_bytes().first().copied().unwrap_or_default();
        let width = trimmed.bytes().take_while(|b| *b == marker).count();
        let code_marker =
            title.len() - trimmed.len() <= 3 && matches!(marker, b'`' | b'~') && width >= 3;
        if let Some((open, length)) = fence {
            if code_marker
                && marker == open
                && width >= length
                && trimmed[width..].trim().is_empty()
            {
                fence = None;
            }
            offset += line.len();
            continue;
        }
        if code_marker {
            fence = Some((marker, width));
            offset += line.len();
            continue;
        }
        if title.starts_with("## ") {
            if let Some(previous) = start.take() {
                found = Some((previous, offset));
            }
            if title == heading {
                start = Some(offset + line.len());
            }
        }
        offset += line.len();
    }
    start.map(|s| (s, body.len())).or(found)
}

fn append_log(body: &mut String, bot: &str, content: &str) -> Result<()> {
    if !body.ends_with('\n') {
        body.push('\n');
    }
    if section(body, "## Note").is_none() {
        body.push_str("\n## Note\n");
    }
    if section(body, "## Log").is_none() {
        body.push_str("\n## Log\n");
    }
    let invalid = || Error::new("Close fenced Markdown before the task Note and Log sections");
    let (start, end) = section(body, "## Log").ok_or_else(invalid)?;
    if start < section(body, "## Note").ok_or_else(invalid)?.0 {
        return Err(Error::new("Task Log must follow Note"));
    }
    body.insert_str(end, &format!("- {} | {bot} | {content}\n", now()));
    Ok(())
}

fn timestamp(value: &str) -> bool {
    DateTime::parse_from_rfc3339(value).is_ok()
        || DateTime::parse_from_str(value, "%Y-%m-%dT%H:%M%:z").is_ok()
}

impl Task {
    fn validate(&self) -> Result<()> {
        if !uuid(&self.id) || !text(&self.title) {
            return Err(Error::new(
                "Task requires a canonical lowercase UUID and a nonempty single-line title",
            ));
        }
        if !timestamp(&self.updated) || self.due.as_ref().is_some_and(|d| !timestamp(d)) {
            return Err(Error::new("Task dates require an explicit timezone"));
        }
        if self.owner.as_deref().is_some_and(|s| !identity(s))
            || self.workdir.as_deref().is_some_and(|s| !text(s))
            || self
                .schedule
                .as_ref()
                .is_some_and(|s| !identity(&s.bot) || !text(&s.id))
            || self.requires.iter().any(|r| match r {
                Resource::Repo { repo } => !text(repo),
                Resource::Secret { secret } => !text(secret),
            })
        {
            return Err(Error::new(
                "Invalid task owner, workdir, resource or schedule reference",
            ));
        }
        if self.source.as_ref().is_some_and(|s| {
            !matches!(s.system.as_str(), "grok" | "muse") || !identity(&s.bot) || !text(&s.id)
        }) {
            return Err(Error::new("Invalid task import source"));
        }
        Ok(())
    }
}

pub struct Collection {
    documents: BTreeMap<String, Document>,
}

fn read_regular(path: &Path) -> Result<String> {
    no_symlinks(path)?;
    if !fs::metadata(path)?.is_file() {
        return Err(Error::new("Task metadata must be a regular file"));
    }
    Ok(fs::read_to_string(path)?)
}

fn read_directory(
    root: &Path,
    archived: bool,
    replacing: &BTreeSet<String>,
) -> Result<BTreeMap<String, Document>> {
    no_symlinks(root)?;
    let mut documents = BTreeMap::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        no_symlinks(&path)?;
        if path.extension().is_none_or(|s| s != "md")
            || path.file_name().is_some_and(|s| s == "INDEX.md")
        {
            continue;
        }
        let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !uuid(id) {
            return Err(Error::new("Task filename must be a canonical UUID.md"));
        }
        if replacing.contains(id) {
            continue;
        }
        let doc = Document::parse(read_regular(&path)?)?;
        if doc.task.id != id {
            return Err(Error::new("Task ID does not match its filename"));
        }
        if doc.task.status.closed() != archived {
            return Err(Error::new(
                "Closed tasks belong in archive; use tasks put to change status",
            ));
        }
        documents.insert(id.to_owned(), doc);
    }
    Ok(documents)
}

impl Collection {
    fn read(root: &Path, replacing: &BTreeSet<String>) -> Result<Self> {
        let mut documents = read_directory(root, false, replacing)?;
        let archive = root.join("archive");
        no_symlinks(&archive)?;
        if archive.try_exists()? {
            for (id, doc) in read_directory(&archive, true, replacing)? {
                if documents.insert(id, doc).is_some() {
                    return Err(Error::new("Duplicate task ID in open and archived files"));
                }
            }
        }
        Ok(Self { documents })
    }

    fn load(root: &Path) -> Result<Self> {
        let collection = Self::read(root, &BTreeSet::new())?;
        collection.validate()?;
        Ok(collection)
    }

    fn validate(&self) -> Result<()> {
        let mut sources = BTreeSet::new();
        for doc in self.documents.values() {
            let t = &doc.task;
            if let Some(source) = &t.source
                && !sources.insert(source)
            {
                return Err(Error::new("Duplicate native task source"));
            }
            for id in t.parent.iter().chain(&t.start_after).chain(&t.done_when) {
                if !uuid(id) || !self.documents.contains_key(id) {
                    return Err(Error::new(format!("Unknown task reference in {}", t.id)));
                }
            }
        }
        self.acyclic(true)?;
        self.acyclic(false)?;
        for doc in self
            .documents
            .values()
            .filter(|d| d.task.status == Status::Completed)
        {
            if !self.completion_blockers(&doc.task).is_empty() {
                return Err(Error::new(
                    "Cannot complete task before every done_when target is completed",
                ));
            }
        }
        Ok(())
    }

    fn acyclic(&self, parents: bool) -> Result<()> {
        let mut incoming: BTreeMap<&str, usize> =
            self.documents.keys().map(|s| (s.as_str(), 0)).collect();
        let mut dependants: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (id, doc) in &self.documents {
            let dependencies: BTreeSet<&str> = if parents {
                doc.task.parent.iter().map(String::as_str).collect()
            } else {
                doc.task
                    .start_after
                    .iter()
                    .chain(&doc.task.done_when)
                    .map(String::as_str)
                    .collect()
            };
            incoming.insert(id, dependencies.len());
            for dependency in dependencies {
                dependants.entry(dependency).or_default().push(id);
            }
        }
        let mut ready: VecDeque<_> = incoming
            .iter()
            .filter_map(|(&id, &count)| (count == 0).then_some(id))
            .collect();
        let mut visited = 0;
        while let Some(id) = ready.pop_front() {
            visited += 1;
            if let Some(children) = dependants.get(id) {
                for child in children {
                    let remaining = incoming.get_mut(child).expect("validated task reference");
                    *remaining -= 1;
                    if *remaining == 0 {
                        ready.push_back(child);
                    }
                }
            }
        }
        if visited != self.documents.len() {
            return Err(Error::new(if parents {
                "Task parent cycle"
            } else {
                "Task dependency cycle"
            }));
        }
        Ok(())
    }

    fn blockers(&self, task: &Task) -> Vec<String> {
        let mut blockers: Vec<_> = task
            .start_after
            .iter()
            .filter(|id| self.documents[*id].task.status != Status::Completed)
            .cloned()
            .collect();
        let mut parent = task.parent.as_ref();
        while let Some(id) = parent {
            let ancestor = &self.documents[id].task;
            if ancestor.status == Status::Cancelled {
                blockers.push(id.clone());
            }
            parent = ancestor.parent.as_ref();
        }
        let mut queue: VecDeque<_> = task.start_after.iter().chain(&task.done_when).collect();
        let mut seen = BTreeSet::new();
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id) {
                continue;
            }
            let dependency = &self.documents[id].task;
            if dependency.status == Status::Cancelled {
                if !blockers.contains(id) {
                    blockers.push(id.clone());
                }
            } else if dependency.status != Status::Completed {
                queue.extend(dependency.start_after.iter().chain(&dependency.done_when));
            }
        }
        blockers
    }

    fn completion_blockers(&self, task: &Task) -> Vec<String> {
        task.done_when
            .iter()
            .filter(|id| self.documents[*id].task.status != Status::Completed)
            .cloned()
            .collect()
    }

    fn view(&self, doc: &Document) -> Value {
        let mut view = doc.view();
        let blockers = self.blockers(&doc.task);
        view["ready"] = json!(
            matches!(doc.task.status, Status::Pending | Status::InProgress) && blockers.is_empty()
        );
        view["blockers"] = json!(blockers);
        view["completion_blockers"] = json!(self.completion_blockers(&doc.task));
        view
    }

    fn index(&self) -> String {
        let mut index = String::from("# In progress\n\n");
        for (id, doc) in &self.documents {
            if doc.task.status == Status::InProgress {
                let title = doc
                    .task
                    .title
                    .replace('\\', "\\\\")
                    .replace('[', "\\[")
                    .replace(']', "\\]");
                index.push_str(&format!("- [{title}]({id}.md)\n"));
            }
        }
        index
    }

    fn require_cancellation(&self) -> Result<()> {
        for doc in self.documents.values().filter(|d| !d.task.status.closed()) {
            let mut parent = doc.task.parent.as_ref();
            while let Some(id) = parent {
                let ancestor = &self.documents[id].task;
                if ancestor.status == Status::Cancelled {
                    return Err(Error::new(
                        "Cancel unfinished descendants too; use tasks put for atomic propagation",
                    ));
                }
                parent = ancestor.parent.as_ref();
            }
        }
        Ok(())
    }

    fn cascade(&mut self, bot: &str, changes: &mut BTreeMap<String, String>) -> Result<()> {
        let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (id, doc) in &self.documents {
            if let Some(parent) = &doc.task.parent {
                children.entry(parent.clone()).or_default().push(id.clone());
            }
        }
        let mut queue: VecDeque<_> = self
            .documents
            .iter()
            .filter(|(_, d)| d.task.status == Status::Cancelled)
            .map(|(id, _)| (id.clone(), id.clone()))
            .collect();
        let mut seen = BTreeSet::new();
        while let Some((parent, cause)) = queue.pop_front() {
            if !seen.insert(parent.clone()) {
                continue;
            }
            for id in children.get(&parent).into_iter().flatten() {
                let doc = self.documents.get_mut(id).expect("validated child");
                if !doc.task.status.closed() {
                    doc.task.status = Status::Cancelled;
                    doc.task.updated = now();
                    append_log(
                        &mut doc.body,
                        bot,
                        &format!("Cancelled because ancestor {cause} was cancelled."),
                    )?;
                    *doc = Document::new(doc.task.clone(), doc.body.clone())?;
                    changes.insert(id.clone(), doc.raw.clone());
                }
                queue.push_back((id.clone(), cause.clone()));
            }
        }
        Ok(())
    }
}

fn managed(root: &Path) -> Result<bool> {
    let marker = root.join("FORMAT");
    no_symlinks(&marker)?;
    if !marker.try_exists()? {
        return Ok(false);
    }
    if read_regular(&marker)? != FORMAT {
        return Err(Error::new("Unsupported shared task format"));
    }
    Ok(true)
}

pub fn load_shared(shared: &Path) -> Result<Option<Collection>> {
    let root = shared.join("tasks");
    if !managed(&root)? {
        return Ok(None);
    }
    if root.join(JOURNAL).try_exists()? {
        return Err(Error::new(
            "Unreconciled task journal; recover in the authoring tree before publishing",
        ));
    }
    let collection = Collection::load(&root)?;
    collection.require_cancellation()?;
    if read_regular(&root.join("INDEX.md"))? != collection.index() {
        return Err(Error::new(
            "Stale task INDEX; run tasks index before publishing",
        ));
    }
    Ok(Some(collection))
}

pub fn require_shared(shared: &Path) -> Result<Collection> {
    load_shared(shared)?.ok_or_else(|| Error::new(REQUIRED))
}

pub fn ensure_export(export: &mut crate::selection::Export) -> Result<bool> {
    if load_shared(export.tree.path())?.is_some() {
        return Ok(false);
    }
    let root = export.tree.path().join("tasks");
    if root.try_exists()? {
        return Err(Error::new(REQUIRED));
    }
    private_dir(&root.join("archive"))?;
    export.write("tasks/FORMAT", FORMAT.as_bytes())?;
    export.write("tasks/INDEX.md", b"# In progress\n\n")?;
    Ok(true)
}

pub fn check_transition(old: Option<&Collection>, new: Option<&Collection>) -> Result<()> {
    let Some(new) = new else {
        return if old.is_none() {
            Ok(())
        } else {
            Err(Error::new(
                "Retain the managed task store and archived history",
            ))
        };
    };
    new.require_cancellation()?;
    for (id, before) in old.into_iter().flat_map(|c| &c.documents) {
        let after = new.documents.get(id).ok_or_else(|| {
            Error::new("Retain completed and cancelled task files; cancel instead of deleting")
        })?;
        if before.task.source != after.task.source {
            return Err(Error::new("Task import source is immutable"));
        }
        if before.task.status != after.task.status
            && before.task.status.closed()
            && after.task.status != Status::Pending
        {
            return Err(Error::new("Reopen a closed task through pending"));
        }
    }
    for (id, doc) in &new.documents {
        if matches!(doc.task.status, Status::InProgress | Status::Completed)
            && old
                .and_then(|c| c.documents.get(id))
                .is_none_or(|before| before.task.status != doc.task.status)
            && !new.blockers(&doc.task).is_empty()
        {
            return Err(Error::new("Task activation is blocked"));
        }
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    documents: BTreeMap<String, String>,
}

fn candidate(root: &Path, documents: &BTreeMap<String, String>) -> Result<Collection> {
    if documents.keys().any(|id| !uuid(id)) {
        return Err(Error::new("Invalid task journal ID"));
    }
    let replacing = documents.keys().cloned().collect();
    let mut collection = Collection::read(root, &replacing)?;
    for (id, raw) in documents {
        let doc = Document::parse(raw.clone())?;
        if &doc.task.id != id {
            return Err(Error::new("Task journal ID does not match document"));
        }
        collection.documents.insert(id.clone(), doc);
    }
    collection.validate()?;
    Ok(collection)
}

fn apply(root: &Path, journal: Journal) -> Result<()> {
    if journal.version != 1 {
        return Err(Error::new("Unsupported task journal version"));
    }
    let collection = candidate(root, &journal.documents)?;
    collection.require_cancellation()?;
    private_dir(&root.join("archive"))?;
    for (id, raw) in &journal.documents {
        let closed = collection.documents[id].task.status.closed();
        let (destination, previous) = if closed {
            (
                root.join("archive").join(format!("{id}.md")),
                root.join(format!("{id}.md")),
            )
        } else {
            (
                root.join(format!("{id}.md")),
                root.join("archive").join(format!("{id}.md")),
            )
        };
        write_private(&destination, raw.as_bytes())?;
        no_symlinks(&previous)?;
        if previous.try_exists()? {
            fs::remove_file(previous)?;
        }
    }
    File::open(root.join("archive"))?.sync_all()?;
    write_private(&root.join("INDEX.md"), collection.index().as_bytes())?;
    fs::remove_file(root.join(JOURNAL))?;
    File::open(root)?.sync_all()?;
    Ok(())
}

fn recover(root: &Path) -> Result<()> {
    let path = root.join(JOURNAL);
    no_symlinks(&path)?;
    if path.try_exists()? {
        let journal = serde_json::from_str(&read_regular(&path)?).map_err(|_| {
            Error::new("Invalid task recovery journal; preserve it for reconciliation")
        })?;
        apply(root, journal)?;
    }
    Ok(())
}

fn commit(
    root: &Path,
    old: &Collection,
    mut documents: BTreeMap<String, String>,
    bot: &str,
) -> Result<()> {
    let mut new = candidate(root, &documents)?;
    new.cascade(bot, &mut documents)?;
    new.validate()?;
    check_transition(Some(old), Some(&new))?;
    let journal = Journal {
        version: 1,
        documents,
    };
    let bytes =
        serde_json::to_vec(&journal).map_err(|_| Error::new("Cannot encode task journal"))?;
    write_private(&root.join(JOURNAL), &bytes)?;
    apply(root, journal)
}

pub fn authoring_lock(shared: &Path) -> Result<Option<Lock>> {
    let root = shared.join("tasks");
    if !managed(&root)? {
        return Ok(None);
    }
    let lock = Lock::cache_exclusive(&root)?;
    recover(&root)?;
    Ok(Some(lock))
}

fn read_tasks(root: &Path, action: &TaskAction) -> Result<Value> {
    if let TaskAction::List {
        all: false, status, ..
    } = action
        && !status.is_some_and(Status::closed)
    {
        let status = status.unwrap_or(Status::InProgress);
        if status == Status::InProgress {
            let index = read_regular(&root.join("INDEX.md"))?;
            let rows = index
                .strip_prefix("# In progress\n\n")
                .ok_or_else(|| Error::new("Invalid task INDEX; run tasks index"))?;
            let mut seen = BTreeSet::new();
            let mut views = Vec::new();
            for row in rows.lines() {
                let id = row
                    .strip_prefix("- [")
                    .and_then(|r| r.rsplit_once("]("))
                    .and_then(|(_, link)| link.strip_suffix(".md)"))
                    .filter(|id| uuid(id))
                    .ok_or_else(|| Error::new("Invalid task INDEX row; run tasks index"))?;
                let doc = Document::parse(read_regular(&root.join(format!("{id}.md")))?)?;
                if doc.task.id != id || doc.task.status != Status::InProgress || !seen.insert(id) {
                    return Err(Error::new("Stale task INDEX; run tasks index"));
                }
                views.push(doc.view());
            }
            return Ok(json!(views));
        }
        let documents = read_directory(root, false, &BTreeSet::new())?;
        return Ok(json!(
            documents
                .values()
                .filter(|doc| doc.task.status == status)
                .map(Document::view)
                .collect::<Vec<_>>()
        ));
    }
    let collection = Collection::load(root)?;
    match action {
        TaskAction::Show { id, .. } => {
            let doc = collection
                .documents
                .get(id)
                .ok_or_else(|| Error::new("Unknown task UUID"))?;
            Ok(collection.view(doc))
        }
        TaskAction::List { all, status, .. } => Ok(json!(
            collection
                .documents
                .values()
                .filter(|d| *all || Some(d.task.status) == *status)
                .map(|d| collection.view(d))
                .collect::<Vec<_>>()
        )),
        _ => unreachable!(),
    }
}

pub fn execute(config: &Config, action: TaskAction) -> Result<()> {
    if matches!(
        action,
        TaskAction::List { accepted: true, .. } | TaskAction::Show { accepted: true, .. }
    ) {
        let cache = crate::shared::root(config).ok_or_else(|| {
            Error::new("No verified shared cache; configure imports and sync first")
        })?;
        let _lock = Lock::cache_shared(&cache)?;
        let active = crate::shared::active(config)?
            .ok_or_else(|| Error::new("No verified shared cache; sync first"))?;
        let root = active.join("tasks");
        if !managed(&root)? {
            return Err(Error::new("No managed tasks in the verified shared cache"));
        }
        println!("{}", read_tasks(&root, &action)?);
        return Ok(());
    }
    let shared = config
        .paths
        .shared
        .as_ref()
        .ok_or_else(|| Error::new("Set paths.shared for framework tasks"))?;
    let root = shared.join("tasks");
    if matches!(action, TaskAction::Init) {
        no_symlinks(&root)?;
        if root.try_exists()? && !managed(&root)? {
            return Err(Error::new(
                "Preserve the existing tasks directory; explicitly migrate it before adoption",
            ));
        }
        private_dir(&root)?;
        let _lock = Lock::cache_exclusive(&root)?;
        if !managed(&root)? {
            write_private(&root.join("FORMAT"), FORMAT.as_bytes())?;
        }
        private_dir(&root.join("archive"))?;
        recover(&root)?;
        let collection = Collection::load(&root)?;
        write_private(&root.join("INDEX.md"), collection.index().as_bytes())?;
        println!("{}", json!({"initialized":true}));
        return Ok(());
    }
    let _lock = authoring_lock(shared)?
        .ok_or_else(|| Error::new("Run tasks init to adopt the shared task format"))?;
    // Progress reads never load archived task bodies or historical activity logs.
    if matches!(action, TaskAction::List { .. } | TaskAction::Show { .. }) {
        println!("{}", read_tasks(&root, &action)?);
        return Ok(());
    }
    let collection = Collection::load(&root)?;
    let output = match action {
        TaskAction::Create { title } => {
            let bot = config
                .bot
                .id
                .as_deref()
                .ok_or_else(|| Error::new("Task writes require bot.id for Log attribution"))?;
            let task = Task {
                id: new_id()?,
                parent: None,
                title,
                status: Status::Pending,
                owner: config.bot.id.clone(),
                schedule: None,
                due: None,
                workdir: None,
                requires: vec![],
                start_after: vec![],
                done_when: vec![],
                updated: now(),
                source: None,
            };
            let mut body = "## Next\n\n## Note\n\n## Log\n".to_owned();
            append_log(&mut body, bot, "Task created.")?;
            let doc = Document::new(task, body)?;
            let id = doc.task.id.clone();
            if collection.documents.contains_key(&id) {
                return Err(Error::new("Task UUID collision; retry creation"));
            }
            commit(
                &root,
                &collection,
                BTreeMap::from([(id.clone(), doc.raw)]),
                bot,
            )?;
            let saved = Collection::load(&root)?;
            saved.view(&saved.documents[&id])
        }
        TaskAction::Put { file, if_match } => {
            let bot = config
                .bot
                .id
                .as_deref()
                .ok_or_else(|| Error::new("Task writes require bot.id for Log attribution"))?;
            let mut doc = Document::parse(read_regular(&file)?)?;
            match (collection.documents.get(&doc.task.id), if_match) {
                (Some(_), None) => return Err(Error::new("Replacing a task requires --if-match")),
                (Some(old), Some(etag)) if old.etag() == etag => (),
                (None, None) => (),
                _ => {
                    return Err(Error::new(
                        "Task ETag conflict; reread and reconcile; local input was preserved",
                    ));
                }
            }
            let content = match collection.documents.get(&doc.task.id) {
                Some(old) if old.task.status != doc.task.status => format!(
                    "Status changed: {} -> {}.",
                    old.task.status.name(),
                    doc.task.status.name()
                ),
                Some(_) => "Task updated.".to_owned(),
                None => "Task created.".to_owned(),
            };
            append_log(&mut doc.body, bot, &content)?;
            doc.task.updated = now();
            let doc = Document::new(doc.task, doc.body)?;
            let id = doc.task.id.clone();
            commit(
                &root,
                &collection,
                BTreeMap::from([(id.clone(), doc.raw)]),
                bot,
            )?;
            let saved = Collection::load(&root)?;
            saved.view(&saved.documents[&id])
        }
        TaskAction::Validate => json!({"valid":true,"tasks":collection.documents.len()}),
        TaskAction::Index => {
            write_private(&root.join("INDEX.md"), collection.index().as_bytes())?;
            json!({"indexed":true})
        }
        TaskAction::Import { format, file } => {
            migration::import(config, &root, &collection, format, &file)?
        }
        TaskAction::Init | TaskAction::Show { .. } | TaskAction::List { .. } => unreachable!(),
    };
    println!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_logs_ignore_fenced_examples_and_stay_inside_the_log_section() {
        let mut body = "## Next\n```markdown\n## Log\nexample\n```\n\n## Note\nkeep\n\n## Log\nold\n\n## References\nkeep reference\n".to_owned();
        append_log(&mut body, "alpha", "Worked on this.")
            .unwrap_or_else(|e| panic!("{}", e.message));
        assert!(body.contains("```markdown\n## Log\nexample\n```"));
        assert!(
            body.find("| alpha | Worked on this.").unwrap() < body.find("## References").unwrap()
        );
        assert!(body.ends_with("## References\nkeep reference\n"));
    }

    #[test]
    fn unfinished_code_blocks_report_a_task_error_without_panicking() {
        let mut body = "## Next\n```\ncode example".to_owned();
        assert!(append_log(&mut body, "alpha", "Worked.").is_err());
    }

    #[test]
    fn staged_migration_adds_required_tasks_and_preserves_native_bytes() {
        let tree = tempfile::tempdir().unwrap();
        fs::write(tree.path().join("knowledge.md"), "keep").unwrap();
        let mut export = crate::selection::Export { tree, bytes: 4 };
        assert!(ensure_export(&mut export).unwrap_or_else(|e| panic!("{}", e.message)));
        assert!(
            load_shared(export.tree.path())
                .unwrap_or_else(|e| panic!("{}", e.message))
                .is_some()
        );
        assert!(!ensure_export(&mut export).unwrap_or_else(|e| panic!("{}", e.message)));
        assert_eq!(
            fs::read_to_string(export.tree.path().join("knowledge.md")).unwrap(),
            "keep"
        );
        assert_eq!(
            export.bytes,
            4 + FORMAT.len() as u64 + "# In progress\n\n".len() as u64
        );
        fs::remove_file(export.tree.path().join("tasks/FORMAT")).unwrap();
        let index = fs::read(export.tree.path().join("tasks/INDEX.md")).unwrap();
        assert!(ensure_export(&mut export).is_err());
        assert_eq!(
            fs::read(export.tree.path().join("tasks/INDEX.md")).unwrap(),
            index
        );
    }
}
