//! The project's event log: `events.jsonl`, one JSON object per line, appended
//! by the binary at every thread step and for every inbox item, never
//! rewritten or pruned. It is the history `context` does not print; `log`
//! reads it.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths::Ctx;
use crate::project::{self, Project};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Event {
    pub ts: String,
    pub kind: String,
    /// The thread id, or "" for a project-level event.
    pub thread: String,
    pub summary: String,
}

pub fn path(project: &Project) -> PathBuf {
    project.dir().join("events.jsonl")
}

/// Appends one event. Never takes the project lock: callers often hold it, and
/// a single `write_all` of one line with `O_APPEND` is enough. A failure to
/// log never fails the step that was logged.
pub fn append(project: &Project, kind: &str, thread: &str, summary: &str) {
    let event = Event {
        ts: project::now(),
        kind: kind.to_string(),
        thread: thread.to_string(),
        // One line: the log is line-delimited.
        summary: summary.chars().map(|c| if c.is_control() { ' ' } else { c }).collect(),
    };
    let result = (|| -> Result<()> {
        let mut line = serde_json::to_string(&event)?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path(project))?;
        file.write_all(line.as_bytes())?;
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("warning: events.jsonl: {error:#}");
    }
}

/// Every event in file order. A line that does not parse is skipped.
pub fn read(project: &Project) -> Vec<Event> {
    std::fs::read_to_string(path(project))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// `<N>m`, `<N>h` or `<N>d` as seconds.
pub fn parse_since(text: &str) -> Result<i64> {
    let text = text.trim();
    let (digits, unit) = text.split_at(text.len().saturating_sub(1));
    let n: i64 = digits.parse().ok().filter(|n| *n > 0).with_context(|| format!("bad duration `{text}`: use `<N>m`, `<N>h` or `<N>d`"))?;
    Ok(match unit {
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => bail!("bad duration `{text}`: use `<N>m`, `<N>h` or `<N>d`"),
    })
}

pub struct Filter<'a> {
    pub thread: Option<&'a str>,
    pub since_secs: Option<i64>,
    /// The newest N; 0 means all.
    pub limit: usize,
}

pub fn select(events: Vec<Event>, filter: &Filter, now: jiff::Timestamp) -> Vec<Event> {
    let cutoff = filter.since_secs.map(|s| now.as_second() - s);
    let mut selected: Vec<Event> = events
        .into_iter()
        .filter(|e| filter.thread.is_none_or(|t| e.thread == t))
        .filter(|e| cutoff.is_none_or(|c| e.ts.parse::<jiff::Timestamp>().is_ok_and(|ts| ts.as_second() >= c)))
        .collect();
    if filter.limit > 0 && selected.len() > filter.limit {
        selected.drain(..selected.len() - filter.limit);
    }
    selected
}

/// `log`: the selected events, oldest first, in local time.
pub fn print_log(ctx: &Ctx, slug: &str, filter: &Filter, json: bool) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    if let Some(id) = filter.thread {
        crate::thread::validate_id(id)?;
    }
    let events = select(read(&project), filter, jiff::Timestamp::now());
    if json {
        for e in &events {
            println!("{}", serde_json::to_string(e)?);
        }
        return Ok(());
    }
    if events.is_empty() {
        println!("no events{}", if path(&project).is_file() { " match" } else { " yet" });
        return Ok(());
    }
    let tz = jiff::tz::TimeZone::system();
    for e in &events {
        let when = e.ts.parse::<jiff::Timestamp>().map(|t| t.to_zoned(tz.clone()).strftime("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|_| e.ts.clone());
        let thread = if e.thread.is_empty() { String::new() } else { format!("{} ", e.thread) };
        println!("{when}  {thread}{}: {}", e.kind, e.summary);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_then_read_in_order_and_one_line_each() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "Demo", "", vec![]).unwrap();
        assert!(read(&project).is_empty());
        append(&project, "thread-started", "t-0001", "first\nline");
        append(&project, "inbox:pr", "t-0001", "second");
        append(&project, "session", "", "third");
        let events = read(&project);
        assert_eq!(events.iter().map(|e| e.summary.as_str()).collect::<Vec<_>>(), ["first line", "second", "third"]);
        assert_eq!(std::fs::read_to_string(path(&project)).unwrap().lines().count(), 3);
        assert!(events[0].ts.parse::<jiff::Timestamp>().is_ok());
    }

    #[test]
    fn select_filters_by_thread_time_and_limit() {
        let now = jiff::Timestamp::now();
        let at = |secs_ago: i64| now.checked_sub(jiff::SignedDuration::from_secs(secs_ago)).unwrap().to_string();
        let events = vec![
            Event { ts: at(7200), kind: "a".into(), thread: "t-0001".into(), summary: "old".into() },
            Event { ts: at(60), kind: "b".into(), thread: "t-0002".into(), summary: "other".into() },
            Event { ts: at(30), kind: "c".into(), thread: "t-0001".into(), summary: "new".into() },
            Event { ts: "garbage".into(), kind: "d".into(), thread: "t-0001".into(), summary: "bad ts".into() },
        ];
        let names = |f: Filter| select(events.clone(), &f, now).into_iter().map(|e| e.summary).collect::<Vec<_>>();
        assert_eq!(names(Filter { thread: None, since_secs: None, limit: 0 }), ["old", "other", "new", "bad ts"]);
        assert_eq!(names(Filter { thread: Some("t-0001"), since_secs: None, limit: 0 }), ["old", "new", "bad ts"]);
        assert_eq!(names(Filter { thread: None, since_secs: Some(3600), limit: 0 }), ["other", "new"]);
        assert_eq!(names(Filter { thread: None, since_secs: None, limit: 2 }), ["new", "bad ts"]);
        assert_eq!(parse_since("2h").unwrap(), 7200);
        assert_eq!(parse_since("30m").unwrap(), 1800);
        assert_eq!(parse_since("3d").unwrap(), 259_200);
        assert!(parse_since("0h").is_err() && parse_since("2w").is_err() && parse_since("").is_err());
    }
}
