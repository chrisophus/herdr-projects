//! The activity ledger: an append-only JSON Lines file per project that outside
//! tools (a notifier, a chat bridge, a dashboard) can follow to learn what the
//! project's threads are doing, without parsing the inbox or the sidebar.
//!
//! `<project>/ledger.jsonl`, one object per line, never rewritten. Every record
//! has `v` (format version, 1), `ts` (Unix seconds), `at` (the same time in
//! RFC 3339), `project`, `event` and, for thread events, `thread` and `title`.
//! Readers must ignore members and events they do not know.
//!
//! | `event`     | when                                   | extra members              |
//! | ----------- | -------------------------------------- | -------------------------- |
//! | `dispatch`  | a thread was started                   | `profile`, `kind`          |
//! | `needs-you` | a thread is waiting on the user        | `reason`                   |
//! | `report`    | a thread wrote a new report            |                            |
//! | `pr`        | a pull request changed                 | `pr`, `what`               |
//! | `resolved`  | a thread was resolved and cleaned up   | `reason`                   |
//! | `away`      | away mode was switched                 | `on`                       |
//!
//! Writing is best effort: a ledger that cannot be written never fails the
//! command or the tick that produced the event. Nothing from GitHub or from an
//! agent's report is copied in except the fixed words this binary chose and the
//! thread's own title.

use std::io::Write;

use serde_json::{Map, Value, json};

use crate::project::Project;

pub const FILE: &str = "ledger.jsonl";
/// Past this size the file moves to `ledger.jsonl.1` (replacing the older one).
const ROTATE_BYTES: u64 = 8 * 1024 * 1024;

pub fn path(project: &Project) -> std::path::PathBuf {
    project.dir().join(FILE)
}

/// Appends one record. `fields` are added after the common members.
pub fn append(project: &Project, event: &str, fields: &[(&str, Value)]) {
    let _ = try_append(project, event, fields, jiff::Timestamp::now());
}

fn try_append(project: &Project, event: &str, fields: &[(&str, Value)], now: jiff::Timestamp) -> std::io::Result<()> {
    let line = record(&project.slug, event, fields, now);
    let file = path(project);
    if std::fs::metadata(&file).is_ok_and(|m| m.len() > ROTATE_BYTES) {
        let _ = std::fs::rename(&file, file.with_extension("jsonl.1"));
    }
    // One `write` of one line with O_APPEND: concurrent writers (the ticker and
    // a command) never interleave inside a line. The project folder must
    // exist; a deleted project is not recreated.
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(&file)?;
    out.write_all(line.as_bytes())
}

fn record(slug: &str, event: &str, fields: &[(&str, Value)], now: jiff::Timestamp) -> String {
    let mut map = Map::new();
    map.insert("v".into(), json!(1));
    map.insert("ts".into(), json!(now.as_second()));
    map.insert("at".into(), json!(now.round(jiff::Unit::Second).map(|t| t.to_string()).unwrap_or_default()));
    map.insert("project".into(), json!(slug));
    map.insert("event".into(), json!(event));
    for (key, value) in fields {
        map.insert((*key).to_string(), value.clone());
    }
    // Control characters (a title with a newline) must not split a record.
    format!("{}\n", Value::Object(map))
}

/// A thread event: adds `thread` and `title`.
pub fn thread_event(project: &Project, event: &str, id: &str, title: &str, fields: &[(&str, Value)]) {
    let mut all: Vec<(&str, Value)> = vec![("thread", json!(id)), ("title", json!(title))];
    all.extend(fields.iter().map(|(k, v)| (*k, v.clone())));
    append(project, event, &all);
}

/// Parsed records, oldest first; lines that are not JSON objects are skipped.
pub fn read(project: &Project) -> Vec<Map<String, Value>> {
    let text = std::fs::read_to_string(path(project)).unwrap_or_default();
    text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|v| match v {
        Value::Object(m) => Some(m),
        _ => None,
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project;

    #[test]
    fn records_are_one_json_object_per_line() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        thread_event(&project, "dispatch", "t-0001", "Fix\nthe \"login\"", &[("profile", json!("claude"))]);
        append(&project, "away", &[("on", json!(true))]);
        let text = std::fs::read_to_string(path(&project)).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        let all = read(&project);
        assert_eq!(all[0]["v"], 1);
        assert_eq!(all[0]["project"], "demo");
        assert_eq!(all[0]["event"], "dispatch");
        assert_eq!(all[0]["title"], "Fix\nthe \"login\"");
        assert_eq!(all[0]["profile"], "claude");
        assert_eq!(all[1]["on"], true);
        assert!(all[1]["ts"].as_i64().unwrap() > 1_700_000_000);
    }

    #[test]
    fn a_deleted_project_is_not_recreated_and_nothing_panics() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        std::fs::remove_dir_all(project.dir()).unwrap();
        append(&project, "away", &[]);
        assert!(!project.dir().exists());
    }

    #[test]
    fn a_large_ledger_rotates() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        std::fs::write(path(&project), vec![b'x'; (ROTATE_BYTES + 1) as usize]).unwrap();
        append(&project, "away", &[]);
        assert_eq!(read(&project).len(), 1);
        assert!(project.dir().join("ledger.jsonl.1").is_file());
    }
}
