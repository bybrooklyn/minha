//! Repository instructions, role definitions, and lazy skill discovery.

use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillSource {
    Project,
    User,
    BuiltIn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: Option<PathBuf>,
    pub source: SkillSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDefinition {
    pub name: String,
    pub path: PathBuf,
    pub content: String,
}

/// Discover instructions from `root` through `target`, nearest directories
/// last. At each scope CLAUDE.md is loaded before AGENTS.md so AGENTS.md has
/// final authority. A symlink alias is represented by AGENTS.md only.
pub fn discover_instructions(
    root: impl AsRef<Path>,
    target: impl AsRef<Path>,
) -> io::Result<Vec<InstructionFile>> {
    let root = root.as_ref().canonicalize()?;
    let mut target = target.as_ref().canonicalize()?;
    if target.is_file() {
        target = target
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no parent"))?
            .to_owned();
    }
    if !target.starts_with(&root) {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    let mut current = target.as_path();
    loop {
        dirs.push(current.to_path_buf());
        if current == root {
            break;
        }
        current = current
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target is not below root"))?;
    }
    dirs.reverse();

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (depth, dir) in dirs.into_iter().enumerate() {
        let agents = dir.join("AGENTS.md");
        let agents_key = agents.is_file().then(|| fs::canonicalize(&agents)).transpose()?;
        let mut candidates = Vec::new();
        candidates.extend(instruction_directory_files(&dir.join(".claude"))?);
        candidates.push(("CLAUDE.md".to_owned(), dir.join("CLAUDE.md")));
        candidates.extend(instruction_directory_files(&dir.join(".agents"))?);
        candidates.push(("AGENTS.md".to_owned(), agents));
        for (name, path) in candidates {
            if !path.is_file() {
                continue;
            }
            let key = fs::canonicalize(&path)?;
            // A checked-in symlink can point outside the workspace; reading
            // through it would feed arbitrary host files into the model.
            if !key.starts_with(&root) {
                continue;
            }
            if name == "CLAUDE.md" && agents_key.as_ref() == Some(&key) {
                continue;
            }
            if seen.insert(key) {
                out.push(InstructionFile {
                    path: path.clone(),
                    name,
                    depth,
                    content: fs::read_to_string(path)?,
                });
            }
        }
    }
    Ok(out)
}

fn instruction_directory_files(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let recognized = [
        "AGENTS.md",
        "CLAUDE.md",
        "CONTEXT.md",
        "INSTRUCTIONS.md",
        "RULES.md",
    ];
    let mut files = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    recognized
                        .iter()
                        .any(|candidate| name.eq_ignore_ascii_case(candidate))
                })
        })
        .map(|path| {
            let name = path
                .strip_prefix(dir.parent().unwrap_or(dir))
                .unwrap_or(&path)
                .display()
                .to_string();
            (name, path)
        })
        .collect::<Vec<_>>();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Discover skill metadata only. The SKILL.md body is loaded by `load_skill`
/// after the router selects the skill, keeping unused instructions out of the
/// model context.
pub fn discover_skills(root: impl AsRef<Path>) -> io::Result<Vec<Skill>> {
    let root = root.as_ref().canonicalize()?;
    let mut dirs = vec![
        (root.join(".minha/skills"), SkillSource::Project),
        (root.join(".codex/skills"), SkillSource::Project),
        (root.join(".claude/skills"), SkillSource::Project),
        (root.join(".agents/skills"), SkillSource::Project),
        (root.join("skills"), SkillSource::Project),
    ];
    if let Some(home) = dirs::home_dir() {
        dirs.extend([
            (home.join(".codex/skills"), SkillSource::User),
            (home.join(".claude/skills"), SkillSource::User),
            (home.join(".agents/skills"), SkillSource::User),
        ]);
    }

    let mut seen_paths = HashSet::new();
    let mut seen_names = HashSet::new();
    let mut out = Vec::new();
    for (dir, source) in dirs {
        if !dir.is_dir() {
            continue;
        }
        let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let skill_file = path.join("SKILL.md");
            if !path.is_dir() || !skill_file.is_file() {
                continue;
            }
            let key = fs::canonicalize(&path)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if seen_paths.insert(key) && seen_names.insert(name.clone()) {
                let header = read_prefix(&skill_file, 8 * 1024)?;
                out.push(Skill {
                    description: frontmatter_value(&header, "description").unwrap_or_default(),
                    name,
                    path: Some(skill_file),
                    source: source.clone(),
                });
            }
        }
    }
    for skill in builtin_skills() {
        if seen_names.insert(skill.name.clone()) {
            out.push(skill);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn discover_agents(root: impl AsRef<Path>) -> io::Result<Vec<AgentDefinition>> {
    let root = root.as_ref().canonicalize()?;
    let dirs = [
        root.join(".minha/agents"),
        root.join(".agents"),
        root.join(".agents/agents"),
        root.join(".claude/agents"),
    ];
    let mut seen_paths = HashSet::new();
    let mut seen_names = HashSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let mut paths = fs::read_dir(dir)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "md")
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(is_agent_definition)
            })
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let canonical = fs::canonicalize(&path)?;
            let name = path
                .file_stem()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default();
            if seen_paths.insert(canonical) && seen_names.insert(name.clone()) {
                out.push(AgentDefinition {
                    name,
                    content: fs::read_to_string(&path)?,
                    path,
                });
            }
        }
    }
    Ok(out)
}

fn is_agent_definition(name: &str) -> bool {
    ![
        "AGENTS.md",
        "CLAUDE.md",
        "CONTEXT.md",
        "INSTRUCTIONS.md",
        "RULES.md",
        "SKILL.md",
        "README.md",
    ]
    .iter()
    .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

pub fn load_skill(skill: &Skill) -> io::Result<String> {
    match (&skill.source, &skill.path) {
        (SkillSource::BuiltIn, _) if skill.name == "caveman" => {
            Ok(include_str!("../../../bundled/skills/caveman/SKILL.md").to_owned())
        }
        (SkillSource::BuiltIn, _) if skill.name == "talk" => {
            Ok(include_str!("../../../bundled/skills/talk/SKILL.md").to_owned())
        }
        (_, Some(path)) => fs::read_to_string(path),
        _ => Err(io::Error::new(io::ErrorKind::NotFound, "unknown built-in skill")),
    }
}

fn builtin_skills() -> Vec<Skill> {
    vec![
        Skill {
            name: "caveman".into(),
            description:
                "Ultra-compressed technical communication with lite, full, ultra, and wenyan levels.".into(),
            path: None,
            source: SkillSource::BuiltIn,
        },
        Skill {
            name: "talk".into(),
            description: "Normal conversational output when compressed speech is not appropriate.".into(),
            path: None,
            source: SkillSource::BuiltIn,
        },
    ]
}

fn read_prefix(path: &Path, max_bytes: usize) -> io::Result<String> {
    use std::io::Read;
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(max_bytes as u64).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn frontmatter_value(input: &str, key: &str) -> Option<String> {
    if input.lines().next()?.trim() != "---" {
        return None;
    }
    for line in input.lines().skip(1) {
        if line.trim() == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix(&format!("{key}:")) {
            return Some(value.trim().trim_matches(['\'', '"']).to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[test]
    #[cfg(unix)]
    fn symlinked_instruction_prefers_agents_name() {
        let temp = tempfile::tempdir().expect("test operation should succeed");
        fs::write(temp.path().join("AGENTS.md"), "one").expect("test operation should succeed");
        symlink(temp.path().join("AGENTS.md"), temp.path().join("CLAUDE.md"))
            .expect("test operation should succeed");
        let found = discover_instructions(temp.path(), temp.path()).expect("test operation should succeed");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "AGENTS.md");
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_instruction_outside_the_workspace_is_skipped() {
        let temp = tempfile::tempdir().expect("test operation should succeed");
        let outside = tempfile::tempdir().expect("test operation should succeed");
        fs::write(outside.path().join("secret.md"), "ssh private key material")
            .expect("test operation should succeed");
        symlink(outside.path().join("secret.md"), temp.path().join("AGENTS.md"))
            .expect("test operation should succeed");
        let found = discover_instructions(temp.path(), temp.path()).expect("test operation should succeed");
        assert!(found.is_empty(), "escaped instruction was loaded: {found:?}");
    }

    #[test]
    fn agents_precedes_claude_and_skills_dedup() {
        let temp = tempfile::tempdir().expect("test operation should succeed");
        fs::write(temp.path().join("AGENTS.md"), "agents wins").expect("test operation should succeed");
        fs::write(temp.path().join("CLAUDE.md"), "claude compatibility")
            .expect("test operation should succeed");
        fs::create_dir_all(temp.path().join(".codex/skills/x")).expect("test operation should succeed");
        fs::create_dir_all(temp.path().join(".claude/skills")).expect("test operation should succeed");
        fs::write(
            temp.path().join(".codex/skills/x/SKILL.md"),
            "---\ndescription: x skill\n---\nbody",
        )
        .expect("test operation should succeed");
        create_skill_alias(
            &temp.path().join(".codex/skills/x"),
            &temp.path().join(".claude/skills/x"),
        );
        let instructions =
            discover_instructions(temp.path(), temp.path()).expect("test operation should succeed");
        assert_eq!(instructions[0].name, "CLAUDE.md");
        assert_eq!(instructions[1].name, "AGENTS.md");
        let skills = discover_skills(temp.path()).expect("test operation should succeed");
        assert_eq!(skills.iter().filter(|skill| skill.name == "x").count(), 1);
        assert!(skills.iter().any(|skill| skill.name == "caveman"));
    }

    #[test]
    fn discovers_compatible_agent_directories() {
        let temp = tempfile::tempdir().expect("test operation should succeed");
        fs::create_dir_all(temp.path().join(".claude/agents")).expect("test operation should succeed");
        fs::write(temp.path().join(".claude/agents/reviewer.md"), "review")
            .expect("test operation should succeed");
        let agents = discover_agents(temp.path()).expect("test operation should succeed");
        assert_eq!(agents[0].name, "reviewer");
    }

    #[test]
    fn discovers_native_claude_and_agents_instruction_directories() {
        let temp = tempfile::tempdir().expect("test operation should succeed");
        fs::create_dir_all(temp.path().join(".claude")).expect("test operation should succeed");
        fs::create_dir_all(temp.path().join(".agents")).expect("test operation should succeed");
        fs::write(temp.path().join(".claude/CONTEXT.md"), "claude context")
            .expect("test operation should succeed");
        fs::write(temp.path().join(".agents/RULES.md"), "shared rules")
            .expect("test operation should succeed");
        let found = discover_instructions(temp.path(), temp.path()).expect("test operation should succeed");
        assert_eq!(found.len(), 2);
        assert!(found[0].name.contains(".claude"));
        assert!(found[1].name.contains(".agents"));
    }

    #[cfg(unix)]
    fn create_skill_alias(source: &Path, target: &Path) {
        symlink(source, target).expect("test operation should succeed");
    }

    #[cfg(windows)]
    fn create_skill_alias(source: &Path, target: &Path) {
        fs::create_dir_all(target).expect("test operation should succeed");
        fs::copy(source.join("SKILL.md"), target.join("SKILL.md")).expect("test operation should succeed");
    }
}
