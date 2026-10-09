//! The turn-end guard: a Claude Code `Stop` hook for a project's coordinator.
//!
//! A coordinator that ends its turn while the ticker has left inbox items it
//! has not been shown ends it blind: the user is told nothing until the ticker
//! decides to nudge, which waits for the pane to sit idle. The guard answers
//! the stop once with a reason that names the new items and the command that
//! shows them, so the coordinator reads them before it goes quiet.
//!
//! Loop safety: the harness sets `stop_hook_active` on a stop that follows a
//! block, and the guard never blocks that one. It also never blocks outside a
//! project's coordinator pane (a thread, a plain Claude session, a pane with no
//! Herdr), when nothing new is waiting, or on any error.

use anyhow::Result;
use serde_json::{Value, json};

use crate::inbox;
use crate::paths::Ctx;
use crate::project::{self, Project};

/// The block answer, or `None` to let the turn end.
pub fn decide(event: &Value, unseen: usize, prefix: &str, slug: &str) -> Option<Value> {
    if event["stop_hook_active"].as_bool().unwrap_or(false) || unseen == 0 {
        return None;
    }
    let plural = if unseen == 1 { "item" } else { "items" };
    Some(json!({
        "decision": "block",
        "reason": format!("[hp guard] {unseen} new inbox {plural} arrived that you have not been shown. Run `{prefix} context {slug}`, handle what needs you, then finish your turn."),
    }))
}

/// The unseen inbox items of the project whose coordinator runs in `pane_id`.
fn coordinator_project(ctx: &Ctx, pane_id: &str) -> Option<Project> {
    project::list_slugs(&ctx.root)
        .into_iter()
        .filter_map(|slug| Project::load(&ctx.root, &slug).ok())
        .filter(|p| p.status() == project::Status::Active)
        .find(|p| p.coordinator().is_some_and(|c| c.pane_id == pane_id) || crate::coordinator::live(p).iter().any(|l| l.pane_id == pane_id))
}

pub fn unseen(project: &Project) -> usize {
    let seen = inbox::seen(project);
    inbox::unhandled(project).iter().filter(|i| !seen.contains(&i.id)).count()
}

/// `hook --agent claude` on a `Stop` event.
pub fn stop(ctx: &Ctx, event: &Value) -> Result<()> {
    if event.get("agent_id").is_some_and(|v| !v.is_null()) {
        return Ok(()); // a subagent's stop
    }
    let Some(pane_id) = ctx.env.var("HERDR_PANE_ID") else {
        return Ok(());
    };
    let Some(project) = coordinator_project(ctx, pane_id) else {
        return Ok(());
    };
    let prefix = crate::coordinator::current_prefix(&ctx.root)?;
    if let Some(answer) = decide(event, unseen(&project), &prefix, &project.slug) {
        println!("{answer}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_once_when_items_are_unseen() {
        let answer = decide(&json!({"hook_event_name": "Stop"}), 2, "hp --root /r", "demo").unwrap();
        assert_eq!(answer["decision"], "block");
        let reason = answer["reason"].as_str().unwrap();
        assert!(reason.contains("2 new inbox items"), "{reason}");
        assert!(reason.contains("`hp --root /r context demo`"), "{reason}");
        assert!(decide(&json!({"stop_hook_active": true}), 2, "hp", "demo").is_none(), "never twice in a row");
        assert!(decide(&json!({}), 0, "hp", "demo").is_none());
        assert!(decide(&json!({}), 1, "hp", "demo").unwrap()["reason"].as_str().unwrap().contains("1 new inbox item arrived"));
    }

    #[test]
    fn counts_only_items_context_has_not_shown() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let a = inbox::write(&project, "thread-state", "t-0001", "idle", "x", "").unwrap();
        inbox::write(&project, "thread-state", "t-0002", "idle", "y", "").unwrap();
        assert_eq!(unseen(&project), 2);
        inbox::mark_seen(&project, &[a]).unwrap();
        assert_eq!(unseen(&project), 1);
    }
}
