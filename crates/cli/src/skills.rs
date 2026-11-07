//! `rudder skills`: put the Rudder skill where AI coding agents look for skills, so they know
//! what Rudder is and how to drive it.
//!
//! The skill text is built into the binary (`skills/rudder/SKILL.md`), so it always matches the
//! CLI that installs it. A file we wrote carries a marker; we update or remove those, and leave
//! a file someone else wrote alone unless `--force` is given.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};

const SKILL: &str = include_str!("../../../skills/rudder/SKILL.md");
const MARKER: &str = "rudder-skill:";
const NAME: &str = "rudder";

pub const HELP: &str = "\
Usage:
  rudder skills install [--agent claude|codex|cursor|agents|all]... [--project] [--dir DIR] [--force] [--dry-run]
  rudder skills list    [--project]
  rudder skills remove  [--agent ...]... [--project] [--force]
  rudder skills print                                     print the skill text

Installs a skill that teaches AI coding agents what Rudder is and how to use it (the rudder CLI,
the MCP server, and how to search the docs of your panel).

  --agent      claude  ~/.claude/skills/rudder/SKILL.md          (project: .claude/skills)
               codex   ~/.codex/skills/rudder/SKILL.md           (project: .agents/skills)
               agents  ~/.agents/skills/rudder/SKILL.md          (project: .agents/skills)
               cursor  .cursor/rules/rudder.mdc                  (project only)
               Without --agent, every agent found on this machine is used.
  --project    install into the current directory instead of your home directory
  --dir DIR    install into DIR/rudder/SKILL.md for any other agent that reads SKILL.md folders
  --force      replace or remove a file that rudder did not write
  --dry-run    show what would change";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Agent {
    Claude,
    Codex,
    Cursor,
    Agents,
}

impl Agent {
    fn parse(value: &str) -> anyhow::Result<Vec<Agent>> {
        Ok(match value {
            "claude" | "claude-code" => vec![Agent::Claude],
            "codex" => vec![Agent::Codex],
            "cursor" => vec![Agent::Cursor],
            "agents" | "generic" => vec![Agent::Agents],
            "all" => vec![Agent::Claude, Agent::Codex, Agent::Agents, Agent::Cursor],
            other => bail!("unknown agent '{other}'; use claude, codex, cursor, agents, or all"),
        })
    }

    fn label(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Cursor => "cursor",
            Agent::Agents => "agents",
        }
    }
}

/// Where to look; the binary fills it from the environment, tests from a temp directory.
pub struct Env {
    pub home: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
    pub cwd: PathBuf,
}

impl Env {
    pub fn from_env() -> Self {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        Self {
            home: var("HOME").or_else(|| var("USERPROFILE")),
            codex_home: var("CODEX_HOME"),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Target {
    label: String,
    path: PathBuf,
    cursor_rule: bool,
}

struct Options {
    agents: Vec<Agent>,
    project: bool,
    dir: Option<PathBuf>,
    force: bool,
    dry_run: bool,
    /// `--agent all` was used: agents without a folder in this scope are skipped, not an error.
    all: bool,
}

fn parse(args: Vec<String>) -> anyhow::Result<Options> {
    let mut options = Options {
        agents: Vec::new(),
        project: false,
        dir: None,
        force: false,
        dry_run: false,
        all: false,
    };
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--agent" | "-a" => {
                let value = iter.next().context("--agent needs a value")?;
                options.all |= value == "all";
                for agent in Agent::parse(&value)? {
                    if !options.agents.contains(&agent) {
                        options.agents.push(agent);
                    }
                }
            }
            "--project" => options.project = true,
            "--dir" => {
                options.dir = Some(PathBuf::from(iter.next().context("--dir needs a value")?))
            }
            "--force" => options.force = true,
            "--dry-run" => options.dry_run = true,
            other => {
                if let Some(value) = other.strip_prefix("--agent=") {
                    options.all |= value == "all";
                    for agent in Agent::parse(value)? {
                        if !options.agents.contains(&agent) {
                            options.agents.push(agent);
                        }
                    }
                } else if let Some(value) = other.strip_prefix("--dir=") {
                    options.dir = Some(PathBuf::from(value));
                } else {
                    bail!("unknown option '{other}'; see rudder skills --help");
                }
            }
        }
    }
    Ok(options)
}

fn skill_file(base: &Path) -> PathBuf {
    base.join(NAME).join("SKILL.md")
}

/// The file an agent reads in the chosen scope, or `None` when it has none there.
fn path_for(agent: Agent, project: bool, env: &Env) -> Option<PathBuf> {
    if project {
        return Some(match agent {
            Agent::Claude => skill_file(&env.cwd.join(".claude").join("skills")),
            Agent::Codex | Agent::Agents => skill_file(&env.cwd.join(".agents").join("skills")),
            Agent::Cursor => env.cwd.join(".cursor").join("rules").join("rudder.mdc"),
        });
    }
    match agent {
        Agent::Claude => Some(skill_file(
            &env.home.as_ref()?.join(".claude").join("skills"),
        )),
        Agent::Codex => {
            let base = env
                .codex_home
                .clone()
                .or_else(|| env.home.as_ref().map(|home| home.join(".codex")))?;
            Some(skill_file(&base.join("skills")))
        }
        Agent::Agents => Some(skill_file(
            &env.home.as_ref()?.join(".agents").join("skills"),
        )),
        Agent::Cursor => None,
    }
}

/// Whether the agent's config folder exists, which is how "every agent on this machine" is found.
fn detected(agent: Agent, project: bool, env: &Env) -> bool {
    let exists = |path: Option<PathBuf>| path.is_some_and(|p| p.is_dir());
    if project {
        return match agent {
            Agent::Claude => exists(Some(env.cwd.join(".claude"))),
            Agent::Cursor => exists(Some(env.cwd.join(".cursor"))),
            Agent::Codex | Agent::Agents => exists(Some(env.cwd.join(".agents"))),
        };
    }
    match agent {
        Agent::Claude => exists(env.home.as_ref().map(|h| h.join(".claude"))),
        Agent::Codex => exists(
            env.codex_home
                .clone()
                .or_else(|| env.home.as_ref().map(|h| h.join(".codex"))),
        ),
        Agent::Agents => exists(env.home.as_ref().map(|h| h.join(".agents"))),
        Agent::Cursor => false,
    }
}

fn targets(options: &Options, env: &Env) -> anyhow::Result<Vec<Target>> {
    let mut out: Vec<Target> = Vec::new();
    let mut push = |label: &str, path: PathBuf, cursor_rule: bool| {
        if !out.iter().any(|existing| existing.path == path) {
            out.push(Target {
                label: label.to_owned(),
                path,
                cursor_rule,
            });
        }
    };
    if let Some(dir) = &options.dir {
        push("custom", skill_file(dir), false);
    }
    let explicit = !options.agents.is_empty();
    let wanted: Vec<Agent> = if explicit {
        options.agents.clone()
    } else if options.dir.is_some() {
        Vec::new()
    } else {
        let found: Vec<Agent> = [Agent::Claude, Agent::Codex, Agent::Agents, Agent::Cursor]
            .into_iter()
            .filter(|agent| detected(*agent, options.project, env))
            .collect();
        if found.is_empty() {
            if options.project {
                vec![Agent::Agents]
            } else {
                bail!(
                    "no AI agent folder found in your home directory; pass --agent claude|codex|agents, \
                     --project for this repository, or --dir DIR"
                );
            }
        } else {
            found
        }
    };
    for agent in wanted {
        match path_for(agent, options.project, env) {
            Some(path) => push(agent.label(), path, agent == Agent::Cursor),
            None if explicit && !options.all => bail!(
                "{} has no user-level skills folder; use --project to install into this repository",
                agent.label()
            ),
            None => {}
        }
    }
    Ok(out)
}

/// The text written for a target.
fn render(cursor_rule: bool) -> String {
    let body = SKILL.replace("{{VERSION}}", rudder_core::VERSION);
    if !cursor_rule {
        return body;
    }
    let rest = body
        .strip_prefix("---\n")
        .or_else(|| body.strip_prefix("---\r\n"))
        .and_then(|after| after.split_once("\n---"))
        .map(|(_, rest)| rest.trim_start_matches(['\r', '\n']))
        .unwrap_or(&body);
    format!(
        "---\ndescription: Operate a self-hosted Rudder panel: rudder CLI, MCP server, deploys, logs, cache, and the panel's documentation\nalwaysApply: false\n---\n\n{rest}"
    )
}

#[derive(PartialEq, Eq, Debug)]
enum State {
    Missing,
    Current,
    Outdated,
    Foreign,
}

fn state_of(target: &Target) -> anyhow::Result<State> {
    let existing = match std::fs::read_to_string(&target.path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(State::Missing),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", target.path.display()))
        }
    };
    if !existing.contains(MARKER) {
        return Ok(State::Foreign);
    }
    // Compare ignoring line-ending differences that an editor or git may have introduced.
    let normalize = |text: &str| text.replace("\r\n", "\n");
    Ok(
        if normalize(&existing) == normalize(&render(target.cursor_rule)) {
            State::Current
        } else {
            State::Outdated
        },
    )
}

pub fn run(args: Vec<String>, env: &Env, out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    let mut args = args.into_iter();
    let command = args.next();
    let rest: Vec<String> = args.collect();
    if rest.iter().any(|a| a == "--help" || a == "-h") {
        writeln!(out, "{HELP}")?;
        return Ok(());
    }
    match command.as_deref() {
        None | Some("--help" | "-h" | "help") => {
            writeln!(out, "{HELP}")?;
            Ok(())
        }
        Some("print") => {
            write!(out, "{}", render(false))?;
            Ok(())
        }
        Some("install") => install(&parse(rest)?, env, out),
        Some("list" | "status") => list(&parse(rest)?, env, out),
        Some("remove" | "uninstall") => remove(&parse(rest)?, env, out),
        Some(other) => bail!("unknown skills command '{other}'; see rudder skills --help"),
    }
}

fn install(options: &Options, env: &Env, out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    let list = targets(options, env)?;
    let mut changed = 0;
    for target in &list {
        let state = state_of(target)?;
        let verb = match state {
            State::Current => {
                writeln!(
                    out,
                    "= {:<7} up to date   {}",
                    target.label,
                    target.path.display()
                )?;
                continue;
            }
            State::Foreign if !options.force => {
                writeln!(
                    out,
                    "! {:<7} skipped      {} exists and was not written by rudder (use --force to replace it)",
                    target.label,
                    target.path.display()
                )?;
                continue;
            }
            State::Missing => "installed",
            State::Outdated | State::Foreign => "updated",
        };
        if !options.dry_run {
            if let Some(parent) = target.path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("could not create {}", parent.display()))?;
            }
            std::fs::write(&target.path, render(target.cursor_rule))
                .with_context(|| format!("could not write {}", target.path.display()))?;
        }
        changed += 1;
        let dry = if options.dry_run { " (dry run)" } else { "" };
        writeln!(
            out,
            "+ {:<7} {verb:<12} {}{dry}",
            target.label,
            target.path.display()
        )?;
    }
    if changed > 0 && !options.dry_run {
        writeln!(
            out,
            "\nRestart the agent (or start a new session) so it picks the skill up. It will then\n\
             search your panel's docs with `rudder docs search` or the MCP docs tools."
        )?;
    }
    Ok(())
}

fn list(options: &Options, env: &Env, out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    let mut options = Options {
        agents: options.agents.clone(),
        project: options.project,
        dir: options.dir.clone(),
        force: false,
        dry_run: false,
        all: true,
    };
    if options.agents.is_empty() && options.dir.is_none() {
        options.agents = Agent::parse("all")?;
    }
    for target in targets(&options, env)? {
        let state = match state_of(&target)? {
            State::Missing => "not installed",
            State::Current => "installed, up to date",
            State::Outdated => "installed, outdated (run rudder skills install)",
            State::Foreign => "present, not written by rudder",
        };
        writeln!(
            out,
            "{:<7} {state:<48} {}",
            target.label,
            target.path.display()
        )?;
    }
    Ok(())
}

fn remove(options: &Options, env: &Env, out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    let mut options = Options {
        agents: options.agents.clone(),
        project: options.project,
        dir: options.dir.clone(),
        force: options.force,
        dry_run: options.dry_run,
        all: true,
    };
    if options.agents.is_empty() && options.dir.is_none() {
        options.agents = Agent::parse("all")?;
    }
    for target in targets(&options, env)? {
        match state_of(&target)? {
            State::Missing => continue,
            State::Foreign if !options.force => {
                writeln!(
                    out,
                    "! {:<7} kept         {} was not written by rudder (use --force)",
                    target.label,
                    target.path.display()
                )?;
                continue;
            }
            _ => {}
        }
        if !options.dry_run {
            std::fs::remove_file(&target.path)
                .with_context(|| format!("could not remove {}", target.path.display()))?;
            if !target.cursor_rule {
                if let Some(dir) = target.path.parent() {
                    // Only the empty folder we created; never anything with other files in it.
                    let _ = std::fs::remove_dir(dir);
                }
            }
        }
        writeln!(
            out,
            "- {:<7} removed      {}",
            target.label,
            target.path.display()
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn sandbox() -> (Env, PathBuf) {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "rudder-skills-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let cwd = root.join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        (
            Env {
                home: Some(home),
                codex_home: None,
                cwd,
            },
            root,
        )
    }

    fn run_to_string(args: &[&str], env: &Env) -> anyhow::Result<String> {
        let mut out: Vec<u8> = Vec::new();
        run(
            args.iter().map(|a| (*a).to_owned()).collect(),
            env,
            &mut out,
        )?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn the_skill_has_frontmatter_a_marker_and_a_version_placeholder() {
        assert!(
            SKILL.starts_with("---\nname: rudder\n") || SKILL.starts_with("---\r\nname: rudder")
        );
        assert!(SKILL.contains(MARKER));
        assert!(!render(false).contains("{{VERSION}}"));
        assert!(render(false).contains(rudder_core::VERSION));
    }

    #[test]
    fn cursor_rule_swaps_the_frontmatter() {
        let rule = render(true);
        assert!(rule.starts_with("---\ndescription:"));
        assert!(!rule.contains("name: rudder"));
        assert!(rule.contains(MARKER));
    }

    #[test]
    fn installs_only_where_an_agent_is_found() {
        let (env, root) = sandbox();
        std::fs::create_dir_all(env.home.as_ref().unwrap().join(".claude")).unwrap();
        let text = run_to_string(&["install"], &env).unwrap();
        assert!(text.contains("installed"));
        let claude = env
            .home
            .as_ref()
            .unwrap()
            .join(".claude/skills/rudder/SKILL.md");
        assert!(claude.is_file());
        assert!(!env.home.as_ref().unwrap().join(".agents").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn install_is_idempotent_and_detects_outdated_files() {
        let (env, root) = sandbox();
        std::fs::create_dir_all(env.home.as_ref().unwrap().join(".claude")).unwrap();
        run_to_string(&["install"], &env).unwrap();
        assert!(run_to_string(&["install"], &env)
            .unwrap()
            .contains("up to date"));
        let file = env
            .home
            .as_ref()
            .unwrap()
            .join(".claude/skills/rudder/SKILL.md");
        let old = std::fs::read_to_string(&file)
            .unwrap()
            .replace(rudder_core::VERSION, "0.0.1");
        std::fs::write(&file, old).unwrap();
        assert!(run_to_string(&["list"], &env).unwrap().contains("outdated"));
        assert!(run_to_string(&["install"], &env)
            .unwrap()
            .contains("updated"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_file_rudder_did_not_write_is_kept_unless_forced() {
        let (env, root) = sandbox();
        let file = env.cwd.join(".claude/skills/rudder/SKILL.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "my own notes").unwrap();
        let text = run_to_string(&["install", "--project", "--agent", "claude"], &env).unwrap();
        assert!(text.contains("skipped"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "my own notes");
        let kept = run_to_string(&["remove", "--project", "--agent", "claude"], &env).unwrap();
        assert!(kept.contains("kept"));
        assert!(file.exists());
        run_to_string(
            &["install", "--project", "--agent", "claude", "--force"],
            &env,
        )
        .unwrap();
        assert!(std::fs::read_to_string(&file).unwrap().contains(MARKER));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_install_writes_cursor_rule_and_remove_cleans_up() {
        let (env, root) = sandbox();
        run_to_string(
            &[
                "install",
                "--project",
                "--agent",
                "cursor",
                "--agent",
                "codex",
            ],
            &env,
        )
        .unwrap();
        assert!(env.cwd.join(".cursor/rules/rudder.mdc").is_file());
        assert!(env.cwd.join(".agents/skills/rudder/SKILL.md").is_file());
        run_to_string(&["remove", "--project", "--agent", "all"], &env).unwrap();
        assert!(!env.cwd.join(".cursor/rules/rudder.mdc").exists());
        assert!(!env.cwd.join(".agents/skills/rudder").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dry_run_writes_nothing_and_errors_are_clear() {
        let (env, root) = sandbox();
        let text = run_to_string(&["install", "--agent", "claude", "--dry-run"], &env).unwrap();
        assert!(text.contains("dry run"));
        assert!(!env.home.as_ref().unwrap().join(".claude").exists());
        let none = run_to_string(&["install"], &env).unwrap_err().to_string();
        assert!(none.contains("no AI agent folder"));
        let cursor = run_to_string(&["install", "--agent", "cursor"], &env)
            .unwrap_err()
            .to_string();
        assert!(cursor.contains("--project"));
        assert!(run_to_string(&["install", "--agent", "nope"], &env).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn custom_dir_gets_a_skill_folder() {
        let (env, root) = sandbox();
        let dir = root.join("elsewhere");
        run_to_string(&["install", "--dir", dir.to_str().unwrap()], &env).unwrap();
        assert!(dir.join("rudder/SKILL.md").is_file());
        let _ = std::fs::remove_dir_all(root);
    }
}
