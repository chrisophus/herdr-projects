//! Roles: `roles/<name>.md` in the project folder. A role is a prompt a thread
//! gets before its task, plus defaults for how it is started. The file belongs
//! to the user; the binary only reads it, and writes the defaults once.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::project::{self, Project};

/// The front matter between `+++` lines. Every field is optional; a file with
/// no front matter is a role with a prompt and no defaults.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
struct Front {
    description: String,
    /// Default Herdr agent kind (`thread start --agent` wins).
    agent: String,
    /// Default model flag (`--agent-arg` wins). Checked by the same
    /// model-only rule as `--agent-arg`, so a role cannot add a launch flag.
    agent_args: Vec<String>,
    /// Default placement (`--kind` wins): worktree, tab or checkout.
    kind: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Role {
    pub name: String,
    pub description: String,
    pub agent: String,
    pub agent_args: Vec<String>,
    pub kind: String,
    /// The prompt, inserted in the brief before the task.
    pub body: String,
}

/// A default role written by `new` and `role init`.
pub struct Default_ {
    pub name: &'static str,
    pub text: &'static str,
}

pub const DEFAULTS: [Default_; 4] = [
    Default_ {
        name: "implementer",
        text: "+++\ndescription = \"Builds the change on its own branch and opens a pull request\"\n+++\n\
You are the implementer. Make the change the task describes, on your own branch, in small commits that say what they change. Run the project's checks before you finish. Open a pull request when the work is ready for review, and put its URL on the first line of your report. Say what you verified and what you did not.\n",
    },
    Default_ {
        name: "reviewer",
        text: "+++\ndescription = \"Reviews a diff or pull request and reports findings; changes nothing\"\n+++\n\
You are the reviewer. Read the diff, branch or pull request the task names and judge it against the task's goal and the project instructions. Do not edit files, commit, or push: findings go in your report only. List each finding with its file and line, most serious first, and end the `## Report` section with one line `Verdict: approve` or `Verdict: request-changes`.\n",
    },
    Default_ {
        name: "scout",
        text: "+++\ndescription = \"Investigates a question in the code and reports what it found; changes nothing\"\n+++\n\
You are the scout. Answer the question the task asks by reading the code, the history and the docs. Do not edit files, commit, or push. Your report is the deliverable: what you found, with file paths, what you could not determine, and what you recommend the coordinator do next.\n",
    },
    Default_ {
        name: "verifier",
        text: "+++\ndescription = \"Runs the checks and the acceptance criteria against a branch; changes nothing\"\n+++\n\
You are the verifier. Check the branch or pull request the task names against the acceptance criteria in the task: run the tests and the project's checks, exercise the change, and record exactly what passed and what failed with the command and its output. Do not fix anything; if something fails, say precisely what and where. End the `## Report` section with one line `Verdict: pass` or `Verdict: fail`.\n",
    },
];

fn roles_dir(project: &Project) -> PathBuf {
    project.dir().join("roles")
}

pub fn path(project: &Project, name: &str) -> PathBuf {
    roles_dir(project).join(format!("{name}.md"))
}

pub fn validate_name(name: &str) -> Result<()> {
    project::validate_slug(name).context("a role name follows the slug rule")
}

pub fn parse(name: &str, text: &str) -> Result<Role> {
    validate_name(name)?;
    let (front, body) = match text.strip_prefix("+++\n") {
        Some(rest) => {
            let (front, body) = rest
                .split_once("\n+++\n")
                .or_else(|| rest.strip_suffix("\n+++").map(|f| (f, "")))
                .context("no closing `+++` line")?;
            (toml::from_str::<Front>(front).context("front matter does not parse")?, body)
        }
        None => (Front::default(), text),
    };
    if !front.kind.is_empty() {
        crate::thread::Kind::parse(&front.kind).context("`kind` in the front matter")?;
    }
    if !front.agent.is_empty() && !crate::agents::is_kind(&front.agent) {
        bail!("`{}` in the front matter is not a Herdr agent kind", front.agent);
    }
    let body = body.trim();
    if body.is_empty() {
        bail!("the role has no prompt after the front matter");
    }
    Ok(Role {
        name: name.to_string(),
        description: front.description.trim().to_string(),
        agent: front.agent.trim().to_string(),
        agent_args: front.agent_args,
        kind: front.kind.trim().to_string(),
        body: body.to_string(),
    })
}

/// The role `roles/<name>.md` holds. A missing file is an error that names it.
pub fn load(project: &Project, name: &str) -> Result<Role> {
    validate_name(name)?;
    let path = path(project, name);
    let text = std::fs::read_to_string(&path).with_context(|| format!("no role `{name}` in `{}` (expected {})", project.slug, path.display()))?;
    parse(name, &text).with_context(|| format!("roles/{name}.md"))
}

pub struct Broken {
    pub file: String,
    pub error: String,
}

/// Every role in `roles/`, sorted by name, and the files that do not parse.
pub fn list(project: &Project) -> (Vec<Role>, Vec<Broken>) {
    let mut roles = Vec::new();
    let mut broken = Vec::new();
    let Ok(entries) = std::fs::read_dir(roles_dir(project)) else {
        return (roles, broken);
    };
    let mut files: Vec<String> = entries.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.ends_with(".md") && !n.starts_with('.')).collect();
    files.sort();
    for file in files {
        let path = roles_dir(project).join(&file);
        // Regular files only: a symbolic link in roles/ is never followed.
        if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match parse(file.trim_end_matches(".md"), &text) {
            Ok(role) => roles.push(role),
            Err(error) => broken.push(Broken { file: format!("roles/{file}"), error: format!("{error:#}") }),
        }
    }
    (roles, broken)
}

/// Writes the default roles that do not exist yet; returns the names written.
/// An existing file, edited or not, is never touched.
pub fn write_defaults(project: &Project) -> Result<Vec<String>> {
    std::fs::create_dir_all(roles_dir(project))?;
    let mut written = Vec::new();
    for role in &DEFAULTS {
        let path = path(project, role.name);
        if path.exists() {
            continue;
        }
        project::write_atomic(&path, role.text.as_bytes())?;
        written.push(role.name.to_string());
    }
    Ok(written)
}

/// `role list`: one line per role, then the broken files.
pub fn print_list(project: &Project) {
    let (roles, broken) = list(project);
    if roles.is_empty() && broken.is_empty() {
        println!("no roles in roles/; `role init {}` writes the defaults", project.slug);
    }
    for role in &roles {
        let mut defaults = Vec::new();
        if !role.agent.is_empty() {
            defaults.push(format!("agent {}", role.agent));
        }
        if !role.agent_args.is_empty() {
            defaults.push(format!("model {}", role.agent_args.join(" ")));
        }
        if !role.kind.is_empty() {
            defaults.push(format!("kind {}", role.kind));
        }
        let defaults = if defaults.is_empty() { String::new() } else { format!(" [{}]", defaults.join(", ")) };
        println!("{}\t{}{defaults}", role.name, role.description);
    }
    for b in &broken {
        println!("config-error: {}: {}", b.file, b.error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_carry_their_names() {
        for d in &DEFAULTS {
            let role = parse(d.name, d.text).unwrap();
            assert_eq!(role.name, d.name);
            assert!(!role.description.is_empty(), "{}", d.name);
            assert!(role.body.starts_with("You are the "), "{}", d.name);
        }
    }

    #[test]
    fn front_matter_is_optional_and_checked() {
        let bare = parse("plain", "Just a prompt.\n").unwrap();
        assert_eq!(bare.body, "Just a prompt.");
        assert!(bare.agent.is_empty() && bare.kind.is_empty() && bare.description.is_empty());

        let full = parse("rev", "+++\ndescription = \"Reviews\"\nagent = \"codex\"\nagent_args = [\"--model\", \"x\"]\nkind = \"tab\"\n+++\n\nLook.\n").unwrap();
        assert_eq!((full.agent.as_str(), full.kind.as_str(), full.description.as_str()), ("codex", "tab", "Reviews"));
        assert_eq!(full.agent_args, ["--model", "x"]);
        assert_eq!(full.body, "Look.");

        assert!(parse("Bad Name", "x").is_err());
        assert!(parse("empty", "+++\n\n+++\n\n\n").unwrap_err().to_string().contains("no prompt"));
        assert!(parse("unclosed", "+++\n+++\n\n\n").unwrap_err().to_string().contains("closing"));
        assert!(parse("k", "+++\nkind = \"pane\"\n+++\nx").is_err());
        assert!(parse("a", "+++\nagent = \"not-an-agent\"\n+++\nx").is_err());
        assert!(parse("u", "+++\nlaunch = \"--yolo\"\n+++\nx").unwrap_err().to_string().contains("front matter"));
        assert!(parse("open", "+++\ndescription = \"x\"\nno close").is_err());
    }

    #[test]
    fn write_defaults_never_overwrites() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "Demo", "", vec![]).unwrap();
        // `create` already wrote them; a second call writes nothing.
        assert!(write_defaults(&project).unwrap().is_empty());
        std::fs::write(path(&project, "reviewer"), "Mine.\n").unwrap();
        std::fs::remove_file(path(&project, "scout")).unwrap();
        assert_eq!(write_defaults(&project).unwrap(), ["scout"]);
        assert_eq!(load(&project, "reviewer").unwrap().body, "Mine.");
        let (roles, broken) = list(&project);
        assert_eq!(roles.len(), 4);
        assert!(broken.is_empty());
        assert!(load(&project, "nope").unwrap_err().to_string().contains("no role `nope`"));
    }
}
