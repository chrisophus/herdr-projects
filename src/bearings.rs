//! `bearings`: one short digest of where a project stands, for a chat reply or
//! a phone screen. Four sections, always in this order: needs you, ready for
//! review, in flight, done recently. It reads thread records and the activity
//! ledger only: no network, no pane reads, so it is fast and safe to run from
//! anywhere. Pull request state is the ticker's last look, not a live query.

use std::fmt::Write as _;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::paths::Ctx;
use crate::project::{self, Project};
use crate::thread::{Group, Thread};
use crate::threads::{self, Row};

/// "Done recently" covers this many hours.
const RECENT_HOURS: i64 = 24;

fn pr_note(t: &Thread) -> String {
    if t.pr.is_empty() {
        return String::new();
    }
    let number = t.pr.rsplit('/').next().unwrap_or("");
    let mut note = format!("PR #{number}");
    if !t.pr_state.is_empty() {
        let _ = write!(note, " {}", t.pr_state.to_lowercase());
    }
    if !t.pr_review.is_empty() {
        let _ = write!(note, ", {}", t.pr_review.to_lowercase().replace('_', " "));
    }
    note
}

fn line(row: &Row) -> String {
    let t = &row.thread;
    let mut parts = vec![row.note.clone()];
    let pr = pr_note(t);
    if !pr.is_empty() {
        parts.push(pr);
    }
    if !t.activity.is_empty() && matches!(row.group, Group::Working | Group::Landing) {
        parts.push(t.activity.clone());
    }
    if let Some(p) = t.percent {
        parts.push(format!("~{p}%"));
    }
    parts.retain(|p| !p.is_empty());
    format!("- {} {} ({})", t.id, t.title, parts.join(" · "))
}

fn recent_done(ledger: &[Map<String, Value>], now: jiff::Timestamp) -> Vec<String> {
    let since = now.as_second() - RECENT_HOURS * 3600;
    let mut out = Vec::new();
    for r in ledger.iter().rev() {
        if r.get("ts").and_then(Value::as_i64).unwrap_or(0) < since {
            break;
        }
        let thread = r.get("thread").and_then(Value::as_str).unwrap_or("");
        let title = r.get("title").and_then(Value::as_str).unwrap_or("");
        let what = match r.get("event").and_then(Value::as_str) {
            Some("pr") if r.get("what").and_then(Value::as_str) == Some("merged") => "PR merged".to_string(),
            Some("resolved") => format!("resolved ({})", r.get("reason").and_then(Value::as_str).unwrap_or("")),
            _ => continue,
        };
        out.push(format!("- {thread} {title} ({what})"));
    }
    out.reverse();
    out
}

pub fn render(project: &Project, rows: &[Row], ledger: &[Map<String, Value>], now: jiff::Timestamp) -> String {
    let (settings, _) = project.read_project_md().unwrap_or_default();
    let name = project::display_name(&settings.name, &project.slug);
    let mut out = String::new();
    let _ = write!(out, "# {name} — bearings");
    if !settings.goal.is_empty() {
        let _ = write!(out, "\n{}", settings.goal);
    }
    let _ = writeln!(out);
    let section = |out: &mut String, title: &str, items: Vec<String>| {
        let _ = writeln!(out, "\n## {title} ({})", items.len());
        if items.is_empty() {
            let _ = writeln!(out, "- nothing");
        }
        for item in items {
            let _ = writeln!(out, "{item}");
        }
    };
    let by = |g: &[Group]| -> Vec<String> { rows.iter().filter(|r| g.contains(&r.group)).map(line).collect() };
    section(&mut out, "Needs you", by(&[Group::WaitingOnYou]));
    section(&mut out, "Ready for review", by(&[Group::ReadyForReview]));
    section(&mut out, "In flight", by(&[Group::Working, Group::Landing, Group::Idle]));
    section(&mut out, &format!("Done in the last {RECENT_HOURS}h"), recent_done(ledger, now));
    out
}

pub fn run(ctx: &Ctx, slug: Option<&str>, file: bool) -> Result<()> {
    let slugs: Vec<String> = match crate::overview::resolve_slug(ctx, slug)? {
        crate::overview::Resolved::Slug(s) => vec![s],
        crate::overview::Resolved::All => project::list_slugs(&ctx.root)
            .into_iter()
            .filter(|s| Project::load(&ctx.root, s).is_ok_and(|p| p.status() != project::Status::Archived))
            .collect(),
    };
    if slugs.is_empty() {
        println!("there are no projects in {}", ctx.root.display());
    }
    let now = jiff::Timestamp::now();
    for (index, slug) in slugs.iter().enumerate() {
        let project = Project::load(&ctx.root, slug)?;
        let text = render(&project, &threads::rows(ctx, &project), &crate::ledger::read(&project), now);
        if index > 0 {
            println!();
        }
        print!("{text}");
        if file {
            let dir = project.dir().join("bearings");
            std::fs::create_dir_all(&dir)?;
            let day = jiff::Zoned::now().strftime("%Y-%m-%d").to_string();
            let path = dir.join(format!("{day}.md"));
            project::write_atomic(&path, text.as_bytes())?;
            println!("\nwritten to {}", path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: &str, group: Group, note: &str) -> Row {
        Row { thread: Thread { id: id.into(), title: format!("title {id}"), ..Thread::default() }, group, note: note.into() }
    }

    #[test]
    fn four_sections_in_order_with_recent_done_from_the_ledger() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "ship it", vec![]).unwrap();
        let now = jiff::Timestamp::now();
        let mut review = row("t-0002", Group::ReadyForReview, "report");
        review.thread.pr = "https://github.com/o/r/pull/7".into();
        review.thread.pr_state = "OPEN".into();
        review.thread.pr_review = "REVIEW_REQUIRED".into();
        let rows = vec![row("t-0001", Group::WaitingOnYou, "blocked"), review, row("t-0003", Group::Working, "working")];
        let ledger = vec![
            json!({"ts": now.as_second() - 100_000, "event": "resolved", "thread": "t-0004", "title": "stale", "reason": "manual"}).as_object().unwrap().clone(),
            json!({"ts": now.as_second() - 5, "event": "pr", "what": "merged", "thread": "t-0009", "title": "old merge"}).as_object().unwrap().clone(),
            json!({"ts": now.as_second() - 3, "event": "report", "thread": "t-0003"}).as_object().unwrap().clone(),
        ];
        let text = render(&project, &rows, &ledger, now);
        let order: Vec<usize> = ["## Needs you (1)", "## Ready for review (1)", "## In flight (1)", "## Done in the last 24h (1)"].iter().map(|h| text.find(h).unwrap_or_else(|| panic!("{h}\n{text}"))).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{text}");
        assert!(text.contains("- t-0002 title t-0002 (report · PR #7 open, review required)"), "{text}");
        assert!(text.contains("- t-0009 old merge (PR merged)"), "{text}");
        assert!(!text.contains("stale"), "{text}");
        assert!(text.contains("ship it"), "{text}");
    }

    #[test]
    fn empty_sections_say_nothing() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let text = render(&project, &[], &[], jiff::Timestamp::now());
        assert_eq!(text.matches("- nothing").count(), 4, "{text}");
    }
}
