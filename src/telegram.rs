//! Telegram: a bot that mirrors notifications to one private chat and carries
//! the user's messages from that chat to a project's coordinator.
//!
//! Configured by the user in `<config_dir>/config.toml`:
//!
//! ```toml
//! [telegram]
//! bot_token = "123456:ABC..."
//! chat_id = 123456789
//! ```
//!
//! Messages from any other chat are ignored. Every API call goes through
//! `curl` with its configuration on standard input, so the token never shows
//! up in a process listing.
//!
//! Inbound messages are queued per project (`.state/telegram.json`) and
//! delivered by the ticker under the same rule as a nudge: only to a
//! coordinator that has been idle for a minute, because a prompt merges with
//! text the user has half-typed in the pane.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::herdr::Herdr;
use crate::paths::Ctx;
use crate::project::{self, Project, Status};
use crate::runner::{Cmd, Runner};

const CALL_TIMEOUT: Duration = Duration::from_secs(20);
/// Telegram's limit is 4096 characters per message.
const CHUNK: usize = 4000;
/// How many sent notifications are remembered for routing replies.
const ROUTES_KEPT: usize = 500;
/// The marker that starts every prompt carrying the user's Telegram text.
pub const PROMPT_MARK: &str = "[telegram]";

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub bot_token: String,
    pub chat_id: i64,
    /// `"away"` (the default): notifications go to the phone only while away
    /// mode is on for the project. `"always"`: every one does.
    pub notify: String,
}

/// The `[telegram]` table, `None` when it is absent or incomplete.
pub fn load_config(config_dir: &Path) -> Result<Option<Config>> {
    #[derive(Deserialize, Default)]
    struct File {
        #[serde(default)]
        telegram: Option<Config>,
    }
    let path = config_dir.join("config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let file: File = toml::from_str(&text).with_context(|| format!("{} does not parse", path.display()))?;
    Ok(file.telegram.filter(|c| !c.bot_token.trim().is_empty() && c.chat_id != 0))
}

pub struct Bot<'a> {
    config: Config,
    runner: &'a dyn Runner,
}

/// Quotes a value for a curl config file.
fn curl_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
}

impl<'a> Bot<'a> {
    /// Whether this project's notifications are mirrored to the chat now.
    pub fn mirrors(&self, project: &Project) -> bool {
        self.config.notify == "always" || crate::away::load(project).on
    }

    pub fn new(config: Config, runner: &'a dyn Runner) -> Bot<'a> {
        Bot { config, runner }
    }

    /// The bot for this setup, when `[telegram]` is configured.
    pub fn configured(ctx: &'a Ctx) -> Option<Bot<'a>> {
        load_config(&ctx.config_dir).ok().flatten().map(|c| Bot::new(c, ctx.runner))
    }

    pub fn chat_id(&self) -> i64 {
        self.config.chat_id
    }

    /// One Bot API call; returns `result`.
    pub fn call(&self, method: &str, body: &Value) -> Result<Value> {
        let config = format!(
            "url = {}\nheader = \"Content-Type: application/json\"\ndata-binary = {}\nmax-time = 15\n",
            curl_quote(&format!("https://api.telegram.org/bot{}/{method}", self.config.bot_token.trim())),
            curl_quote(&body.to_string()),
        );
        let cmd = Cmd::new("curl", CALL_TIMEOUT).args(["-sS", "--config", "-"]).stdin(config);
        let output = self.runner.run(&cmd).context("could not run curl")?;
        if !output.success() && output.stdout.trim().is_empty() {
            bail!("telegram {method}: {}", output.error_text());
        }
        let reply: Value = serde_json::from_str(output.stdout.trim()).with_context(|| format!("telegram {method}: not JSON"))?;
        if reply["ok"].as_bool() != Some(true) {
            bail!("telegram {method}: {}", reply["description"].as_str().unwrap_or("failed"));
        }
        Ok(reply["result"].clone())
    }

    /// Sends `text` to the configured chat, split into Telegram-sized parts.
    /// Returns the id of the last message sent.
    pub fn send(&self, text: &str, reply_to: Option<i64>, silent: bool) -> Result<i64> {
        let mut last = 0;
        for (i, part) in chunks(text).iter().enumerate() {
            let mut body = json!({"chat_id": self.config.chat_id, "text": part, "disable_notification": silent});
            if let Some(id) = reply_to.filter(|_| i == 0) {
                body["reply_parameters"] = json!({"message_id": id, "allow_sending_without_reply": true});
            }
            last = self.call("sendMessage", &body)?["message_id"].as_i64().unwrap_or(0);
        }
        Ok(last)
    }
}

fn chunks(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(CHUNK).map(|c| c.iter().collect()).collect()
}

/// Root-level state: the update offset, which project each sent message
/// belongs to (so a reply goes back to it) and the project last written to.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RootState {
    pub offset: i64,
    pub routes: Vec<(i64, String)>,
    pub last: String,
}

fn root_state_path(root: &Path) -> PathBuf {
    root.join(".telegram.json")
}

pub fn load_root_state(root: &Path) -> RootState {
    project::read_json(&root_state_path(root)).unwrap_or_default()
}

fn save_root_state(root: &Path, state: &RootState) -> Result<()> {
    project::write_json(&root_state_path(root), state)
}

/// Remembers that `message_id` was about `slug`.
pub fn record_route(root: &Path, message_id: i64, slug: &str) {
    if message_id == 0 {
        return;
    }
    let mut state = load_root_state(root);
    state.routes.push((message_id, slug.to_string()));
    let excess = state.routes.len().saturating_sub(ROUTES_KEPT);
    state.routes.drain(..excess);
    let _ = save_root_state(root, &state);
}

/// A project's queue of the user's messages waiting for an idle coordinator.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Queue {
    pub pending: Vec<Pending>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Pending {
    pub text: String,
    pub message_id: i64,
}

fn queue_path(project: &Project) -> PathBuf {
    project.state_dir().join("telegram.json")
}

pub fn load_queue(project: &Project) -> Queue {
    project::read_json(&queue_path(project)).unwrap_or_default()
}

fn save_queue(project: &Project, queue: &Queue) -> Result<()> {
    let _lock = project.lock()?;
    project::write_json(&queue_path(project), queue)
}

/// Where a message goes.
#[derive(Debug, PartialEq)]
pub enum Route {
    Project(String, String),
    Reply(String),
    /// `/away [on|off] [project]`: `None` asks for the state.
    Away(Option<bool>, Option<String>),
}

/// What the user wrote, and where it goes: a reply to a message about a
/// project goes there; `/to <slug> <text>` names one; otherwise the only
/// active project, else the project last written to.
pub fn route(text: &str, reply_to: Option<i64>, state: &RootState, active: &[String]) -> Route {
    let text = text.trim();
    let (command, rest) = match text.split_once(char::is_whitespace) {
        Some((c, r)) => (c, r.trim()),
        None => (text, ""),
    };
    // `/cmd@botname` is how Telegram writes commands in some clients.
    let command = command.split('@').next().unwrap_or(command);
    match command {
        "/start" | "/help" => return Route::Reply(help(active)),
        "/projects" => return Route::Reply(String::new()),
        "/away" => {
            let mut words = rest.split_whitespace();
            let state = match words.next() {
                None => None,
                Some("on") => Some(true),
                Some("off") => Some(false),
                Some(_) => return Route::Reply("Usage: /away on|off [project]".into()),
            };
            return Route::Away(state, words.next().map(str::to_string));
        }
        "/to" => {
            let (slug, message) = match rest.split_once(char::is_whitespace) {
                Some((s, m)) => (s, m.trim()),
                None => (rest, ""),
            };
            if slug.is_empty() {
                return Route::Reply("Usage: /to <project> <message>".into());
            }
            return Route::Project(slug.to_string(), message.to_string());
        }
        _ => {}
    }
    if let Some(id) = reply_to
        && let Some((_, slug)) = state.routes.iter().rev().find(|(m, _)| *m == id)
    {
        return Route::Project(slug.clone(), text.to_string());
    }
    if active.len() == 1 {
        return Route::Project(active[0].clone(), text.to_string());
    }
    if !state.last.is_empty() && active.contains(&state.last) {
        return Route::Project(state.last.clone(), text.to_string());
    }
    Route::Reply(format!("Which project? Reply to one of its notifications, or write /to <project> <message>.\n\n{}", list_line(active)))
}

fn list_line(active: &[String]) -> String {
    if active.is_empty() { "There are no active projects.".into() } else { format!("Active projects: {}", active.join(", ")) }
}

fn help(active: &[String]) -> String {
    format!(
        "herdr-projects\n\nNotifications from your projects arrive here. Write to a project's coordinator by replying to one of its notifications, or with /to <project> <message>; later messages go to the same project. With one active project, any message goes to it.\n\n/projects lists the projects and what needs you. /away on or /away off [project] switches away mode: while it is on, notifications come here.\n\n{}",
        list_line(active)
    )
}

fn projects_text(root: &Path) -> String {
    let mut lines = Vec::new();
    for slug in project::list_slugs(root) {
        let Ok(project) = Project::load(root, &slug) else {
            continue;
        };
        let status = project.status();
        if status == Status::Archived {
            continue;
        }
        let name = project.read_project_md().map(|(s, _)| project::display_name(&s.name, &slug)).unwrap_or(slug.clone());
        let line = crate::sidebar::project_line(&crate::sidebar::recorded_groups(&project), status == Status::Paused);
        let coordinator = if crate::coordinator::live(&project).is_empty() { " · no coordinator" } else { "" };
        lines.push(format!("{name} ({slug}): {}{coordinator}", if line.is_empty() { "idle".to_string() } else { line }));
    }
    if lines.is_empty() { "There are no projects.".into() } else { lines.join("\n") }
}

/// `/away`: switches away mode for the named project, else for every active
/// one, and says what it did. Away mode grants nothing (it only changes what is
/// alerted and where), so the chat may switch it.
fn away_text(root: &Path, state: Option<bool>, slug: Option<&str>, active: &[String]) -> String {
    let slugs: Vec<String> = match slug {
        Some(slug) => vec![slug.to_string()],
        None => active.to_vec(),
    };
    if slugs.is_empty() {
        return list_line(active);
    }
    let mut lines = Vec::new();
    for slug in slugs {
        let Ok(project) = Project::load(root, &slug) else {
            lines.push(format!("There is no project `{slug}`. {}", list_line(active)));
            continue;
        };
        let away = match state {
            Some(on) => crate::away::set(&project, on).map(|a| a.on),
            None => Ok(crate::away::load(&project).on),
        };
        lines.push(match away {
            Ok(true) => format!("{slug}: away"),
            Ok(false) => format!("{slug}: present"),
            Err(error) => format!("{slug}: {error:#}"),
        });
    }
    lines.join("\n")
}

fn active_slugs(root: &Path) -> Vec<String> {
    project::list_slugs(root)
        .into_iter()
        .filter(|slug| Project::load(root, slug).is_ok_and(|p| p.status() == Status::Active))
        .collect()
}

/// Reads new messages and queues or answers each one. Run once per tick.
pub fn poll(ctx: &Ctx) -> Vec<anyhow::Error> {
    let Some(bot) = Bot::configured(ctx) else {
        return Vec::new();
    };
    let mut errors = Vec::new();
    let mut state = load_root_state(&ctx.root);
    let updates = match bot.call("getUpdates", &json!({"offset": state.offset, "timeout": 0, "allowed_updates": ["message"]})) {
        Ok(Value::Array(updates)) => updates,
        Ok(_) => return Vec::new(),
        Err(error) => return vec![error],
    };
    if updates.is_empty() {
        return Vec::new();
    }
    for update in &updates {
        let id = update["update_id"].as_i64().unwrap_or(0);
        state.offset = state.offset.max(id + 1);
        let message = &update["message"];
        // Anyone can write to a bot: only the configured chat is heard.
        if message["chat"]["id"].as_i64() != Some(bot.chat_id()) {
            continue;
        }
        let message_id = message["message_id"].as_i64().unwrap_or(0);
        let Some(text) = message["text"].as_str() else {
            errors.extend(bot.send("Only text messages are passed on.", Some(message_id), true).err());
            continue;
        };
        let reply_to = message["reply_to_message"]["message_id"].as_i64();
        let active = active_slugs(&ctx.root);
        let answer = match route(text, reply_to, &state, &active) {
            Route::Reply(text) if text.is_empty() => Some(projects_text(&ctx.root)),
            Route::Reply(text) => Some(text),
            Route::Away(state, slug) => Some(away_text(&ctx.root, state, slug.as_deref(), &active)),
            Route::Project(slug, text) => match handle(ctx, &slug, &text, message_id) {
                Ok(answer) => {
                    state.last = slug;
                    answer
                }
                Err(error) => Some(format!("{error:#}")),
            },
        };
        if let Some(answer) = answer {
            errors.extend(bot.send(&answer, Some(message_id), true).err());
        }
    }
    // Saved before anything else can fail, so no message is handled twice.
    errors.extend(save_root_state(&ctx.root, &state).err());
    errors
}

/// Queues `text` for `slug`'s coordinator; returns what to answer now, if
/// anything (a delivered message is acknowledged on delivery).
fn handle(ctx: &Ctx, slug: &str, text: &str, message_id: i64) -> Result<Option<String>> {
    let project = Project::load(&ctx.root, slug).map_err(|_| anyhow::anyhow!("There is no project `{slug}`. {}", list_line(&active_slugs(&ctx.root))))?;
    let name = project.read_project_md().map(|(s, _)| project::display_name(&s.name, slug)).unwrap_or(slug.to_string());
    match project.status() {
        Status::Active => {}
        Status::Paused => bail!("{name} is paused; nothing was sent."),
        Status::Archived => bail!("{name} is archived; nothing was sent."),
    }
    if text.is_empty() {
        return Ok(Some(format!("Messages now go to {name}.")));
    }
    let mut queue = load_queue(&project);
    queue.pending.push(Pending { text: text.to_string(), message_id });
    save_queue(&project, &queue)?;
    if crate::coordinator::live(&project).is_empty() {
        return Ok(Some(format!("{name} has no coordinator running. Your message waits until one starts (`herdr-projects open {slug}`).")));
    }
    Ok(None)
}

/// The prompt for the queued messages.
pub fn prompt_text(slug: &str, prefix: &str, pending: &[Pending]) -> String {
    let mut text = String::new();
    for p in pending {
        text.push_str(PROMPT_MARK);
        text.push(' ');
        text.push_str(p.text.trim());
        text.push_str("\n\n");
    }
    text.push_str(&format!("(The user wrote this on Telegram and may be away from this pane: answer with `{prefix} telegram send {slug} --text-file -`.)"));
    text
}

/// Delivers the queued messages to the coordinator in `ready_pane` as one
/// prompt. Returns whether anything was delivered.
pub fn deliver(ctx: &Ctx, project: &Project, herdr: &Herdr, ready_pane: Option<&str>) -> Result<bool> {
    let queue = load_queue(project);
    if queue.pending.is_empty() {
        return Ok(false);
    }
    let Some(pane) = ready_pane else {
        return Ok(false);
    };
    let prefix = crate::coordinator::current_prefix(&ctx.root)?;
    herdr.agent_prompt(pane, &prompt_text(&project.slug, &prefix, &queue.pending))?;
    // Only what was delivered is removed: a message queued meanwhile stays.
    {
        let _lock = project.lock()?;
        let mut now = load_queue(project);
        now.pending.retain(|p| !queue.pending.contains(p));
        project::write_json(&queue_path(project), &now)?;
    }
    if let Some(bot) = Bot::configured(ctx) {
        let name = project.read_project_md().map(|(s, _)| project::display_name(&s.name, &project.slug)).unwrap_or(project.slug.clone());
        let last = queue.pending.last().map(|p| p.message_id);
        if let Ok(id) = bot.send(&format!("→ {name}"), last, true) {
            record_route(&ctx.root, id, &project.slug);
        }
    }
    Ok(true)
}

/// `telegram send <slug>`: the coordinator's answer to the user.
pub fn send(ctx: &Ctx, slug: &str, text: &str) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let text = text.trim();
    if text.is_empty() {
        bail!("the text is empty");
    }
    let bot = Bot::configured(ctx).context("Telegram is not configured: add a [telegram] table with bot_token and chat_id to config.toml (`herdr-projects telegram setup` finds the chat id)")?;
    let name = project.read_project_md().map(|(s, _)| project::display_name(&s.name, slug)).unwrap_or(slug.to_string());
    let id = bot.send(&format!("{name}\n{text}"), None, false)?;
    record_route(&ctx.root, id, slug);
    println!("sent to Telegram");
    Ok(())
}

/// `telegram setup`: with `bot_token` set, finds the private chat that last
/// wrote to the bot and writes its id into config.toml.
pub fn setup(ctx: &Ctx) -> Result<()> {
    let path = ctx.config_dir.join("config.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc = text.parse::<toml_edit::DocumentMut>().with_context(|| format!("{} does not parse", path.display()))?;
    let token = doc.get("telegram").and_then(|t| t.get("bot_token")).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if token.is_empty() {
        bail!(
            "first create a bot with @BotFather in Telegram and add its token to {}:\n\n[telegram]\nbot_token = \"<token>\"\n\nthen send the bot any message and run this again",
            path.display()
        );
    }
    let bot = Bot::new(Config { bot_token: token, chat_id: 0, ..Config::default() }, ctx.runner);
    let me = bot.call("getMe", &json!({}))?;
    let updates = bot.call("getUpdates", &json!({"timeout": 0, "allowed_updates": ["message"]}))?;
    let chat = updates
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .map(|u| &u["message"]["chat"])
        .find(|c| c["type"].as_str() == Some("private"))
        .with_context(|| format!("no private message to @{} found: send the bot any message from your own Telegram account, then run this again (the ticker must not be running with an older chat id meanwhile)", me["username"].as_str().unwrap_or("your bot")))?;
    let chat_id = chat["id"].as_i64().context("the chat has no id")?;
    let who = chat["username"].as_str().map(|u| format!("@{u}")).or_else(|| chat["first_name"].as_str().map(str::to_string)).unwrap_or_default();
    doc["telegram"]["chat_id"] = toml_edit::value(chat_id);
    std::fs::create_dir_all(&ctx.config_dir)?;
    project::write_atomic(&path, doc.to_string().as_bytes())?;
    let bot = Bot::new(Config { bot_token: bot.config.bot_token, chat_id, ..Config::default() }, ctx.runner);
    bot.send("herdr-projects is connected. Write /help for how to talk to your projects.", None, false)?;
    println!("Telegram: chat {chat_id} ({who}) written to {}; the ticker picks it up on its next tick.", path.display());
    Ok(())
}

/// `telegram test`: sends one message.
pub fn test(ctx: &Ctx) -> Result<()> {
    let bot = Bot::configured(ctx).context("Telegram is not configured (see `herdr-projects telegram setup`)")?;
    bot.send("herdr-projects test message", None, false)?;
    println!("sent a test message to chat {}", bot.chat_id());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(routes: &[(i64, &str)], last: &str) -> RootState {
        RootState { offset: 0, routes: routes.iter().map(|(m, s)| (*m, s.to_string())).collect(), last: last.into() }
    }

    fn project(slug: &str, text: &str) -> Route {
        Route::Project(slug.into(), text.into())
    }

    #[test]
    fn routing_prefers_a_reply_then_to_then_the_only_project_then_the_last() {
        let active = vec!["billing".to_string(), "docs".to_string()];
        let s = state(&[(10, "billing"), (11, "docs")], "");
        assert_eq!(route("ship it", Some(11), &s, &active), project("docs", "ship it"));
        assert_eq!(route("/to billing  go ahead", Some(11), &s, &active), project("billing", "go ahead"));
        assert_eq!(route("/to@hp_bot docs", None, &s, &active), project("docs", ""));
        assert!(matches!(route("hello", None, &s, &active), Route::Reply(t) if t.starts_with("Which project?")));
        assert!(matches!(route("hello", Some(99), &s, &active), Route::Reply(_)));
        let s = state(&[], "docs");
        assert_eq!(route("hello", None, &s, &active), project("docs", "hello"));
        // The last project no longer active: ask again.
        assert!(matches!(route("hello", None, &s, &["billing".into(), "x".into()]), Route::Reply(_)));
        assert_eq!(route("hello", None, &s, &["billing".into()]), project("billing", "hello"));
        assert!(matches!(route("/help", None, &s, &active), Route::Reply(t) if t.contains("/projects")));
        assert_eq!(route("/projects", None, &s, &active), Route::Reply(String::new()));
        assert_eq!(route("/away on", None, &s, &active), Route::Away(Some(true), None));
        assert_eq!(route("/away@hp_bot off docs", None, &s, &active), Route::Away(Some(false), Some("docs".into())));
        assert_eq!(route("/away", None, &s, &active), Route::Away(None, None));
        assert!(matches!(route("/away maybe", None, &s, &active), Route::Reply(t) if t.starts_with("Usage")));
        assert!(matches!(route("/to", None, &s, &active), Route::Reply(t) if t.starts_with("Usage")));
    }

    #[test]
    fn curl_config_quoting_and_chunks() {
        assert_eq!(curl_quote(r#"{"text":"a \"b\"\\n"}"#), r#""{\"text\":\"a \\\"b\\\"\\\\n\"}""#);
        assert_eq!(chunks("").len(), 1);
        assert_eq!(chunks(&"é".repeat(CHUNK + 1)).len(), 2);
    }

    #[test]
    fn the_prompt_marks_every_message_and_says_how_to_answer() {
        let pending = vec![Pending { text: " first ".into(), message_id: 1 }, Pending { text: "second".into(), message_id: 2 }];
        let text = prompt_text("billing", "/bin/hp --root /r", &pending);
        assert!(text.starts_with("[telegram] first\n\n[telegram] second\n\n"));
        assert!(text.ends_with("`/bin/hp --root /r telegram send billing --text-file -`.)"));
    }

    #[test]
    fn config_needs_both_token_and_chat() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_config(dir.path()).unwrap(), None);
        std::fs::write(dir.path().join("config.toml"), "[telegram]\nbot_token = \"t\"\n").unwrap();
        assert_eq!(load_config(dir.path()).unwrap(), None);
        std::fs::write(dir.path().join("config.toml"), "[telegram]\nbot_token = \"t\"\nchat_id = 42\n").unwrap();
        assert_eq!(load_config(dir.path()).unwrap(), Some(Config { bot_token: "t".into(), chat_id: 42, notify: String::new() }));
    }

    #[test]
    fn routes_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        for i in 1..=(ROUTES_KEPT as i64 + 5) {
            record_route(dir.path(), i, "p");
        }
        let s = load_root_state(dir.path());
        assert_eq!(s.routes.len(), ROUTES_KEPT);
        assert_eq!(s.routes[0].0, 6);
    }
}
