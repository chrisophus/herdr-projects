//! Away mode: the user says they are not at the desk, and the ticker stops
//! trusting that a notification was seen.
//!
//! While away the ticker (1) re-notifies, with the request sound, a thread that
//! has been waiting on the user for `STILL_WAITING_SECS`, once per interval,
//! and (2) raises a wedge alarm when inbox items have sat unhandled for
//! `WEDGE_SECS`, which means the coordinator is not picking them up (closed,
//! stuck on a dialog, out of tokens). Both go to the Herdr notification and the
//! activity ledger, so a bridge that follows the ledger forwards them too.
//! The coordinator's digest also says the user is away, so it keeps working
//! through what it may and queues decisions instead of waiting for an answer.
//! Away mode never grants an agent anything: safety settings are unchanged.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::project::{self, Project};
use crate::thread::{self, Group, Status};

pub const STILL_WAITING_SECS: i64 = 15 * 60;
pub const WEDGE_SECS: i64 = 10 * 60;
/// How often the same wedge alarm repeats.
pub const WEDGE_REPEAT_SECS: i64 = 30 * 60;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Away {
    pub on: bool,
    pub since: String,
    /// Thread id -> when it was last escalated.
    pub escalated: BTreeMap<String, String>,
    pub last_wedge: String,
}

fn path(project: &Project) -> std::path::PathBuf {
    project.state_dir().join("away.json")
}

pub fn load(project: &Project) -> Away {
    project::read_json(&path(project)).unwrap_or_default()
}

pub fn set(project: &Project, on: bool) -> Result<Away> {
    let _lock = project.lock()?;
    let mut away = load(project);
    if away.on == on {
        return Ok(away);
    }
    away = Away { on, since: if on { project::now() } else { String::new() }, ..Away::default() };
    project::write_json(&path(project), &away)?;
    crate::ledger::append(project, "away", &[("on", json!(on))]);
    Ok(away)
}

/// The line `context` shows the coordinator, empty when the user is present.
pub fn digest_line(project: &Project) -> String {
    let away = load(project);
    if !away.on {
        return String::new();
    }
    format!(
        "Away: the user is away (since {}). Keep working through what you are allowed to do, answer only plainly in-task prompts, and queue every decision for the user as one short line each instead of waiting for an answer. Their approvals are unchanged: do not start threads, merge or resolve on your own.\n",
        away.since
    )
}

/// One ticker pass. Returns the notifications to send as (subject, body), so
/// the caller owns the Herdr call and tests need no session.
pub fn pass(project: &Project, now: jiff::Timestamp) -> Vec<(String, String)> {
    let mut away = load(project);
    if !away.on {
        return Vec::new();
    }
    let before = away.clone();
    let mut out = Vec::new();

    let open: Vec<thread::Thread> = thread::list(project).into_iter().filter(|t| t.status == Status::Open).collect();
    for t in &open {
        if t.last_group != Group::WaitingOnYou.token() {
            away.escalated.remove(&t.id);
            continue;
        }
        let waited = thread::seconds_since(&t.last_state_change, now);
        let since_last = away.escalated.get(&t.id).map(|at| thread::seconds_since(at, now));
        if waited >= STILL_WAITING_SECS && since_last.is_none_or(|s| s >= STILL_WAITING_SECS) {
            away.escalated.insert(t.id.clone(), now.to_string());
            let body = format!("still waiting on you after {} min · {}", waited / 60, t.title);
            crate::ledger::thread_event(project, "needs-you", &t.id, &t.title, &[("reason", json!("still waiting"))]);
            out.push((t.id.clone(), body));
        }
    }
    away.escalated.retain(|id, _| open.iter().any(|t| &t.id == id));

    let stuck: Vec<_> = crate::inbox::unhandled(project).into_iter().filter(|i| thread::seconds_since(&i.created, now) >= WEDGE_SECS).collect();
    if !stuck.is_empty() && (away.last_wedge.is_empty() || thread::seconds_since(&away.last_wedge, now) >= WEDGE_REPEAT_SECS) {
        away.last_wedge = now.to_string();
        let oldest = stuck.iter().map(|i| thread::seconds_since(&i.created, now)).max().unwrap_or(0);
        crate::ledger::append(project, "wedge", &[("items", json!(stuck.len())), ("minutes", json!(oldest / 60))]);
        out.push((String::new(), format!("the coordinator has not picked up {} inbox item(s) for {} min; is it running?", stuck.len(), oldest / 60)));
    } else if stuck.is_empty() {
        away.last_wedge.clear();
    }

    if away != before {
        let _ = project::write_json(&path(project), &away);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::Thread;

    fn waiting(project: &Project, minutes_ago: i64, now: jiff::Timestamp) -> String {
        let id = thread::allocate(project, |t| {
            *t = Thread { title: "ask".into(), ..t.clone() };
        })
        .unwrap()
        .id;
        let when = jiff::Timestamp::from_second(now.as_second() - minutes_ago * 60).unwrap().to_string();
        thread::update(project, &id, |t| {
            t.status = Status::Open;
            t.last_group = Group::WaitingOnYou.token().into();
            t.last_state_change = when;
        })
        .unwrap();
        id
    }

    #[test]
    fn nothing_happens_when_present() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let now = jiff::Timestamp::now();
        waiting(&project, 60, now);
        assert!(pass(&project, now).is_empty());
        assert_eq!(digest_line(&project), "");
    }

    #[test]
    fn a_long_wait_escalates_once_per_interval() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let now = jiff::Timestamp::now();
        let id = waiting(&project, 20, now);
        set(&project, true).unwrap();
        assert!(digest_line(&project).starts_with("Away:"));
        let first = pass(&project, now);
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(first[0].0, id);
        assert!(pass(&project, now).is_empty(), "not again within the interval");
        let later = jiff::Timestamp::from_second(now.as_second() + STILL_WAITING_SECS + 1).unwrap();
        assert_eq!(pass(&project, later).len(), 1);
        // The ledger recorded the switch and the escalations.
        let events: Vec<String> = crate::ledger::read(&project).iter().map(|r| r["event"].as_str().unwrap().to_string()).collect();
        assert_eq!(events, ["away", "needs-you", "needs-you"]);
    }

    #[test]
    fn a_recent_wait_is_left_alone_and_turning_off_clears_state() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let now = jiff::Timestamp::now();
        waiting(&project, 2, now);
        set(&project, true).unwrap();
        assert!(pass(&project, now).is_empty());
        set(&project, false).unwrap();
        assert_eq!(load(&project), Away::default());
    }

    #[test]
    fn unhandled_inbox_items_raise_a_wedge_alarm_that_repeats_slowly() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        set(&project, true).unwrap();
        crate::inbox::write(&project, "thread-state", "t-0001", "idle", "x", "").unwrap();
        let now = jiff::Timestamp::now();
        assert!(pass(&project, now).is_empty(), "too fresh");
        let t1 = jiff::Timestamp::from_second(now.as_second() + WEDGE_SECS + 5).unwrap();
        let alarm = pass(&project, t1);
        assert_eq!(alarm.len(), 1);
        assert!(alarm[0].1.contains("not picked up 1 inbox item"));
        assert!(pass(&project, t1).is_empty());
        let t2 = jiff::Timestamp::from_second(t1.as_second() + WEDGE_REPEAT_SECS + 1).unwrap();
        assert_eq!(pass(&project, t2).len(), 1);
    }
}
