//! The system prompt, built as pi 1.5 builds it but without pi's fixed wording (its
//! preamble, its two closing rules and its pointers to pi's own documentation). Fixed
//! wording belongs in AGENTS files. What the core adds is what changes: the tools on
//! offer and their guidelines, the AGENTS files found, the skills list and the working
//! directory.
//!
//! Like pi, every part after the preamble is a named section `<name>\n...\n</name>`, and
//! parts are joined with a blank line. The runner's `instructions` take pi's preamble
//! place, and its `sections` follow the core's (one with a core section's name replaces it).

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::Value;

/// Where the prompt's files come from.
#[derive(Clone, Debug)]
pub struct Sources {
    /// The global folder: its AGENTS file comes first, its `skills/` folder is searched.
    pub agent_dir: PathBuf,
    /// More skill folders or files (`--skills`).
    pub skill_paths: Vec<PathBuf>,
}

impl Sources {
    /// `~/.config/conversation-core` unless `--agent-dir` names another.
    pub fn new(agent_dir: Option<PathBuf>, skill_paths: Vec<PathBuf>) -> Sources {
        let agent_dir = agent_dir.unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| Path::new(&h).join(".config/conversation-core"))
                .unwrap_or_else(|| PathBuf::from(".config/conversation-core"))
        });
        Sources {
            agent_dir,
            skill_paths,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: String,
    pub disable_model_invocation: bool,
}

/// What a tool adds to the prompt: one line in `<tools>`, and rules in `<rules>`.
#[derive(Clone, Debug, Default)]
pub struct ToolPrompt {
    pub name: String,
    pub snippet: Option<String>,
    pub guidelines: Vec<String>,
}

/// pi 1.5's prompt contributions for its built-in tools. bash's one guideline is about
/// pi's own environment variables, so it is left out like the rest of pi's own wording.
pub fn builtin(name: &str) -> ToolPrompt {
    let (snippet, guidelines): (&str, &[&str]) = match name {
        "read" => (
            "Read file contents",
            &["Use read to examine files instead of cat or sed."],
        ),
        "bash" => ("Execute bash commands (ls, grep, find, etc.)", &[]),
        "edit" => (
            "Make precise file edits with exact text replacement, including multiple disjoint edits in one call",
            &[
                "Use edit for targeted changes. Batch changes to one file in a single call.",
                "Each edits[].oldText must exactly and uniquely match the original file. Keep matches short and non-overlapping; merge nearby changes without including large unchanged regions.",
            ],
        ),
        "write" => (
            "Create or overwrite files",
            &["Use write only for new files or complete rewrites."],
        ),
        "grep" => (
            "Search file contents for patterns (respects .gitignore)",
            &[],
        ),
        "find" => ("Find files by glob pattern (respects .gitignore)", &[]),
        "ls" => ("List directory contents", &[]),
        _ => {
            return ToolPrompt {
                name: name.into(),
                ..Default::default()
            };
        }
    };
    ToolPrompt {
        name: name.into(),
        snippet: Some(snippet.into()),
        guidelines: guidelines.iter().map(|g| g.to_string()).collect(),
    }
}

/// A runner tool's `promptSnippet` and `promptGuidelines` (pi's tool fields), taken off the
/// definition so they are not sent to the provider.
pub fn take_tool_prompt(tool: &mut Value) -> ToolPrompt {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let Some(object) = tool.as_object_mut() else {
        return ToolPrompt {
            name,
            ..Default::default()
        };
    };
    let snippet = object
        .remove("promptSnippet")
        .and_then(|s| s.as_str().map(|s| s.trim().replace(['\r', '\n'], " ")))
        .filter(|s| !s.is_empty());
    let guidelines = match object.remove("promptGuidelines") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|g| g.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    ToolPrompt {
        name,
        snippet,
        guidelines,
    }
}

/// node's path.resolve: absolute and normalized without touching the filesystem.
fn resolve(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Some(
        text.strip_prefix('\u{feff}')
            .map(str::to_string)
            .unwrap_or(text),
    )
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn context_file_in(dir: &Path) -> Option<ContextFile> {
    for name in [
        "AGENTS.override.md",
        "AGENTS.md",
        "AGENTS.MD",
        "CLAUDE.md",
        "CLAUDE.MD",
    ] {
        let path = dir.join(name);
        if !path.exists() || !path.is_file() {
            continue;
        }
        if let Some(content) = read_text(&path) {
            return Some(ContextFile {
                path: text(&path),
                content,
            });
        }
    }
    None
}

/// pi's findGitPaths: the nearest folder with `.git`, and the repository's common git folder.
fn git_paths(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dir = cwd.to_path_buf();
    loop {
        let git = dir.join(".git");
        if git.exists() {
            if git.is_file() {
                let content = read_text(&git)?;
                let content = content.trim();
                let target = content.strip_prefix("gitdir: ")?;
                let git_dir = resolve(&dir, Path::new(target.trim()));
                if !git_dir.join("HEAD").exists() {
                    return None;
                }
                let common = match read_text(&git_dir.join("commondir")) {
                    Some(c) => resolve(&git_dir, Path::new(c.trim())),
                    None => git_dir,
                };
                return Some((dir, common));
            }
            if git.is_dir() {
                if !git.join("HEAD").exists() {
                    return None;
                }
                return Some((dir, git));
            }
            return None;
        }
        dir = dir.parent()?.to_path_buf();
    }
}

fn within(parent: &Path, child: &Path) -> bool {
    child.starts_with(parent)
}

/// The main checkout's AGENTS file that a nested linked worktree's own copy shadows, so the
/// same instructions are not given twice (pi's findShadowedContextFile).
fn shadowed_context_file(cwd: &Path) -> Option<PathBuf> {
    let (repo_dir, common) = git_paths(cwd)?;
    let common = canonical(&common);
    let worktree_root = canonical(&repo_dir);
    let main_root = common.parent()?.to_path_buf();
    if !within(&main_root, &worktree_root) || main_root == worktree_root {
        return None;
    }
    if canonical(&main_root.join(".git")) != common {
        return None;
    }
    let own = context_file_in(&worktree_root)?;
    Some(main_root.join(Path::new(&own.path).file_name()?))
}

/// pi's loadProjectContextFiles: the global AGENTS file, then those from the filesystem root
/// down to the working directory.
pub fn context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let cwd = resolve(Path::new("/"), cwd);
    let agent_dir = resolve(&cwd, agent_dir);
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    if let Some(global) = context_file_in(&agent_dir) {
        seen.insert(global.path.clone());
        files.push(global);
    }
    let shadowed = shadowed_context_file(&cwd);
    let mut ancestors = Vec::new();
    let mut dir = cwd.clone();
    loop {
        if let Some(file) = context_file_in(&dir) {
            let is_shadowed = shadowed.as_ref().is_some_and(|s| {
                Path::new(&file.path)
                    .file_name()
                    .is_some_and(|name| canonical(&dir).join(name) == *s)
            });
            if !is_shadowed && seen.insert(file.path.clone()) {
                ancestors.insert(0, file);
            }
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => break,
        }
    }
    files.extend(ancestors);
    files
}

/// YAML frontmatter between a leading `---` and the next `\n---`, as pi's parseFrontmatter.
/// None when the frontmatter does not parse.
fn frontmatter(content: &str) -> Option<serde_json::Map<String, Value>> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return Some(serde_json::Map::new());
    }
    let Some(end) = normalized[3..].find("\n---").map(|i| i + 3) else {
        return Some(serde_json::Map::new());
    };
    let yaml = normalized.get(4..end).unwrap_or("");
    if yaml.is_empty() {
        return Some(serde_json::Map::new());
    }
    match serde_yaml::from_str::<Value>(yaml) {
        Ok(Value::Object(map)) => Some(map),
        Ok(Value::Null) => Some(serde_json::Map::new()),
        Ok(_) => Some(serde_json::Map::new()),
        Err(_) => None,
    }
}

/// pi's loadSkillFromFile, without its warnings.
fn skill_from_file(path: &Path) -> Option<Skill> {
    let content = read_text(path)?;
    let front = frontmatter(&content)?;
    let description = front
        .get("description")
        .and_then(Value::as_str)
        .filter(|d| !d.trim().is_empty());
    let description = description?.to_string();
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());
    let name = front
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or(parent)
        .unwrap_or_default();
    Some(Skill {
        name,
        description,
        file_path: text(path),
        disable_model_invocation: front.get("disable-model-invocation") == Some(&Value::Bool(true)),
    })
}

/// Ignore files read so far, innermost last (pi applies .gitignore, .ignore and .fdignore).
fn with_ignores(stack: &[Gitignore], dir: &Path) -> Vec<Gitignore> {
    let mut stack = stack.to_vec();
    let mut builder = GitignoreBuilder::new(dir);
    let mut any = false;
    for name in [".gitignore", ".ignore", ".fdignore"] {
        let file = dir.join(name);
        if file.is_file() {
            any |= builder.add(file).is_none();
        }
    }
    if any && let Ok(matcher) = builder.build() {
        stack.push(matcher);
    }
    stack
}

fn ignored(stack: &[Gitignore], path: &Path, is_dir: bool) -> bool {
    for matcher in stack.iter().rev() {
        match matcher.matched(path, is_dir) {
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => return false,
            Match::None => {}
        }
    }
    false
}

/// pi's loadSkillsFromDirInternal: a folder with SKILL.md is one skill; otherwise its
/// subfolders are searched, and in the top folder loose .md files with a description count.
fn skills_in_dir(dir: &Path, top: bool, stack: &[Gitignore], out: &mut Vec<Skill>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let stack = with_ignores(stack, dir);
    let mut entries: Vec<_> = read.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in &entries {
        if entry.file_name() != "SKILL.md" {
            continue;
        }
        let path = entry.path();
        if !path.is_file() || ignored(&stack, &path, false) {
            continue;
        }
        if let Some(skill) = skill_from_file(&path) {
            out.push(skill);
        }
        return;
    }
    for entry in &entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if ignored(&stack, &path, meta.is_dir()) {
            continue;
        }
        if meta.is_dir() {
            skills_in_dir(&path, false, &stack, out);
        } else if meta.is_file()
            && top
            && name.ends_with(".md")
            && let Some(skill) = skill_from_file(&path)
        {
            out.push(skill);
        }
    }
}

/// pi's loadSkills: `<agent dir>/skills`, then each `--skills` path; the first skill of a
/// name wins.
pub fn skills(cwd: &Path, sources: &Sources) -> Vec<Skill> {
    let cwd = resolve(Path::new("/"), cwd);
    let mut found = Vec::new();
    skills_in_dir(
        &resolve(&cwd, &sources.agent_dir.join("skills")),
        true,
        &[],
        &mut found,
    );
    for raw in &sources.skill_paths {
        let path = resolve(&cwd, raw);
        if path.is_dir() {
            skills_in_dir(&path, true, &[], &mut found);
        } else if path.is_file() && path.extension().is_some_and(|e| e == "md") {
            found.extend(skill_from_file(&path));
        }
    }
    let mut names = HashSet::new();
    let mut real = HashSet::new();
    found
        .into_iter()
        .filter(|s| {
            let key = canonical(Path::new(&s.file_path));
            if real.contains(&key) {
                return false;
            }
            if !names.insert(s.name.clone()) {
                return false;
            }
            real.insert(key);
            true
        })
        .collect()
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// pi's formatSkillsForPrompt, trimmed as pi uses it.
fn skills_section(skills: &[Skill], reader: &str) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "The following skills provide specialized instructions for specific tasks.".to_string(),
        match reader {
            "read" => "Use the read tool to load a skill's file when the task matches its description.",
            "bash" => "Use bash to load a skill's file when the task matches its description.",
            _ => "Load a skill's file when the task matches its description.",
        }
        .to_string(),
        "Resolve relative paths against the skill's directory (parent of SKILL.md). Use absolute paths in tool calls."
            .to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in visible {
        lines.push("  <skill>".into());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path)
        ));
        lines.push("  </skill>".into());
    }
    lines.push("</available_skills>".into());
    lines.join("\n").trim().to_string()
}

fn rules(tools: &[ToolPrompt]) -> String {
    let names: HashSet<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    let mut rules: Vec<String> = Vec::new();
    let mut add = |rule: &str| {
        let rule = rule.trim();
        if !rule.is_empty() && !rules.iter().any(|r| r == rule) {
            rules.push(rule.to_string());
        }
    };
    let (bash, powershell) = (names.contains("bash"), names.contains("powershell"));
    if (bash || powershell)
        && !names.contains("grep")
        && !names.contains("find")
        && !names.contains("ls")
    {
        add(match (bash, powershell) {
            (true, true) => {
                "Use bash or PowerShell for file operations like listing, searching, and finding files"
            }
            (false, true) => {
                "Use PowerShell for file operations like listing, searching, and finding files"
            }
            _ => "Use bash for file operations like ls, rg, find",
        });
    }
    for tool in tools {
        for rule in &tool.guidelines {
            add(rule);
        }
    }
    rules
        .iter()
        .map(|r| format!("- {r}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn valid_section_name(name: &str) -> bool {
    let mut chars = name.chars();
    name != "preamble"
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The whole system prompt: the runner's preamble, the core's sections, then the runner's.
pub fn build(
    preamble: &str,
    tools: &[ToolPrompt],
    files: &[ContextFile],
    skills: &[Skill],
    cwd: &Path,
    extra: &[(String, String)],
) -> String {
    let mut sections: Vec<(String, String)> = Vec::new();
    let listed: Vec<String> = tools
        .iter()
        .filter_map(|t| t.snippet.as_ref().map(|s| format!("- {}: {s}", t.name)))
        .collect();
    if !listed.is_empty() {
        sections.push(("tools".into(), listed.join("\n")));
    }
    let rules = rules(tools);
    if !rules.is_empty() {
        sections.push(("rules".into(), rules));
    }
    if !files.is_empty() {
        let mut parts = vec!["Project-specific instructions and guidelines:".to_string()];
        parts.extend(files.iter().map(|f| {
            format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                f.path, f.content
            )
        }));
        sections.push(("project_context".into(), parts.join("\n\n")));
    }
    let reader = ["read", "bash"]
        .into_iter()
        .find(|r| tools.iter().any(|t| t.name == *r));
    if let Some(reader) = reader {
        let text = skills_section(skills, reader);
        if !text.is_empty() {
            sections.push(("skills".into(), text));
        }
    }
    sections.push(("cwd".into(), text(cwd).replace('\\', "/")));
    for (name, content) in extra {
        if content.is_empty() {
            continue;
        }
        match sections.iter_mut().find(|(n, _)| n == name) {
            Some(existing) => existing.1 = content.clone(),
            None => sections.push((name.clone(), content.clone())),
        }
    }
    let mut parts = vec![preamble.to_string()];
    parts.extend(
        sections
            .into_iter()
            .map(|(name, content)| format!("<{name}>\n{content}\n</{name}>")),
    );
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("core-prompt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn agents_files_global_first_then_root_down() {
        let root = temp("agents");
        let agent = root.join("agent");
        let deep = root.join("a/b");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(agent.join("AGENTS.md"), "global").unwrap();
        std::fs::write(root.join("a/CLAUDE.md"), "\u{feff}outer").unwrap();
        std::fs::write(deep.join("AGENTS.override.md"), "inner").unwrap();
        std::fs::write(deep.join("AGENTS.md"), "not used").unwrap();
        let files = context_files(&deep, &agent);
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, ["global", "outer", "inner"]);
    }

    #[test]
    fn prompt_has_only_the_changing_parts() {
        let tools = vec![
            builtin("read"),
            ToolPrompt {
                name: "ipython".into(),
                snippet: None,
                guidelines: vec!["Use Python.".into()],
            },
        ];
        let files = vec![ContextFile {
            path: "/w/AGENTS.md".into(),
            content: "Be kind.".into(),
        }];
        let skills = vec![Skill {
            name: "a&b".into(),
            description: "Does <x>".into(),
            file_path: "/s/a/SKILL.md".into(),
            disable_model_invocation: false,
        }];
        let prompt = build(
            "Hello.",
            &tools,
            &files,
            &skills,
            Path::new("/w"),
            &[("scratch".into(), "Use /tmp.".into())],
        );
        assert_eq!(
            prompt,
            "Hello.\n\n<tools>\n- read: Read file contents\n</tools>\n\n<rules>\n- Use read to examine files instead of cat or sed.\n- Use Python.\n</rules>\n\n<project_context>\nProject-specific instructions and guidelines:\n\n<project_instructions path=\"/w/AGENTS.md\">\nBe kind.\n</project_instructions>\n</project_context>\n\n<skills>\nThe following skills provide specialized instructions for specific tasks.\nUse the read tool to load a skill's file when the task matches its description.\nResolve relative paths against the skill's directory (parent of SKILL.md). Use absolute paths in tool calls.\n\n<available_skills>\n  <skill>\n    <name>a&amp;b</name>\n    <description>Does &lt;x&gt;</description>\n    <location>/s/a/SKILL.md</location>\n  </skill>\n</available_skills>\n</skills>\n\n<cwd>\n/w\n</cwd>\n\n<scratch>\nUse /tmp.\n</scratch>"
        );
        assert!(!prompt.contains("pi"), "no pi wording");
    }

    #[test]
    fn no_skills_without_a_reader() {
        let skills = vec![Skill {
            name: "s".into(),
            description: "d".into(),
            file_path: "/s".into(),
            disable_model_invocation: false,
        }];
        let prompt = build(
            "",
            &[ToolPrompt {
                name: "ipython".into(),
                ..Default::default()
            }],
            &[],
            &skills,
            Path::new("/w"),
            &[],
        );
        assert_eq!(prompt, "<cwd>\n/w\n</cwd>");
    }

    #[test]
    fn skills_found_like_pi() {
        let root = temp("skills");
        let skills_dir = root.join("agent/skills");
        std::fs::create_dir_all(skills_dir.join("one/nested")).unwrap();
        std::fs::create_dir_all(skills_dir.join("two")).unwrap();
        std::fs::create_dir_all(skills_dir.join("hidden")).unwrap();
        std::fs::write(
            skills_dir.join("one/SKILL.md"),
            "---\nname: one\ndescription: >\n  First\n  skill\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            skills_dir.join("one/nested/SKILL.md"),
            "---\ndescription: never\n---\n",
        )
        .unwrap();
        std::fs::write(
            skills_dir.join("two/SKILL.md"),
            "---\ndescription: Second\ndisable-model-invocation: true\n---\n",
        )
        .unwrap();
        std::fs::write(
            skills_dir.join("hidden/SKILL.md"),
            "---\ndescription: ignored\n---\n",
        )
        .unwrap();
        std::fs::write(skills_dir.join(".gitignore"), "hidden/\n").unwrap();
        std::fs::write(
            skills_dir.join("loose.md"),
            "---\ndescription: Loose\n---\n",
        )
        .unwrap();
        std::fs::write(skills_dir.join("plain.md"), "no frontmatter").unwrap();
        let found = skills(&root, &Sources::new(Some(root.join("agent")), vec![]));
        let names: Vec<(&str, &str)> = found
            .iter()
            .map(|s| (s.name.as_str(), s.description.as_str()))
            .collect();
        // A loose file without a name is named after its folder, as in pi.
        assert_eq!(
            names,
            [
                ("skills", "Loose"),
                ("one", "First skill"),
                ("two", "Second")
            ]
        );
        assert!(found[2].disable_model_invocation);
    }

    #[test]
    fn runner_tool_prompt_fields_are_taken_off() {
        let mut tool = serde_json::json!({"type": "function", "name": "ipython", "promptSnippet": "Run\nPython", "promptGuidelines": ["Use it."], "parameters": {}});
        let prompt = take_tool_prompt(&mut tool);
        assert_eq!(prompt.snippet.as_deref(), Some("Run Python"));
        assert_eq!(prompt.guidelines, ["Use it."]);
        assert_eq!(
            tool,
            serde_json::json!({"type": "function", "name": "ipython", "parameters": {}})
        );
    }
}
