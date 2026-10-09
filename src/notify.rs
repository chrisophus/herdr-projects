//! Specific Herdr notifications: one per event that needs the user, titled
//! `<Project> · <thread>`, with a sound only when the user is needed.
//! `mute = true` in PROJECT.md silences everything but errors. With
//! `[telegram]` configured, each notification also goes to Telegram.

use crate::herdr::Herdr;
use crate::paths::Ctx;
use crate::project::{self, Project};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sound {
    None,
    /// A result: a new report, a merge.
    Done,
    /// The user is needed.
    Request,
}

impl Sound {
    fn arg(self) -> &'static str {
        match self {
            Sound::None => "none",
            Sound::Done => "done",
            Sound::Request => "request",
        }
    }
}

pub struct Notifier<'a> {
    herdr: Option<Herdr<'a>>,
    telegram: Option<crate::telegram::Bot<'a>>,
    root: &'a std::path::Path,
    slug: String,
    name: String,
    mute: bool,
}

impl<'a> Notifier<'a> {
    pub fn new(ctx: &'a Ctx, project: &Project) -> Notifier<'a> {
        let settings = project.read_project_md().map(|(s, _)| s).unwrap_or_default();
        let herdr = project.coordinator().filter(|c| !c.socket.is_empty() && std::path::Path::new(&c.socket).exists()).map(|c| Herdr::new(ctx.env.herdr_bin(), c.socket, ctx.runner));
        let telegram = crate::telegram::Bot::configured(ctx);
        Notifier { herdr, telegram, root: &ctx.root, slug: project.slug.clone(), name: project::display_name(&settings.name, &project.slug), mute: settings.mute }
    }

    /// `subject` is a thread id or another short name; `error` notifications
    /// go out even when the project is muted.
    pub fn send(&self, subject: &str, body: &str, sound: Sound, error: bool) {
        if self.mute && !error {
            return;
        }
        let title = if subject.is_empty() { self.name.clone() } else { format!("{} · {subject}", self.name) };
        if let Some(herdr) = &self.herdr {
            let _ = herdr.call(&["notification", "show", &title, "--body", body, "--sound", sound.arg()], crate::herdr::CALL_TIMEOUT);
        }
        // Silent on the phone unless it is a request or a result.
        if let Some(bot) = &self.telegram
            && let Ok(id) = bot.send(&format!("{title}\n{body}"), None, sound == Sound::None)
        {
            crate::telegram::record_route(self.root, id, &self.slug);
        }
    }
}
