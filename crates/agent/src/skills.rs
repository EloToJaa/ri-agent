//! Explicitly invoked Markdown skills. Discovery reads metadata, never executes code.
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_SKILL_BYTES: usize = 128 * 1024;
const MAX_EXPANDED_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Clone, Default)]
pub struct Skills {
    entries: BTreeMap<String, Skill>,
    pub warnings: Vec<String>,
}

#[derive(Deserialize)]
struct Metadata {
    name: String,
    description: String,
}

fn read_skill(path: &Path) -> Result<String> {
    let mut text = String::new();
    fs::File::open(path)?
        .take((MAX_SKILL_BYTES + 1) as u64)
        .read_to_string(&mut text)?;
    if text.len() > MAX_SKILL_BYTES {
        bail!("Skill exceeds {MAX_SKILL_BYTES} bytes");
    }
    Ok(text)
}

fn parse(text: &str) -> Result<(Metadata, String)> {
    let text = text.replace("\r\n", "\n");
    let rest = text
        .strip_prefix("---\n")
        .context("Skill must start with YAML frontmatter")?;
    let (yaml, body) = rest
        .split_once("\n---\n")
        .context("Missing skill frontmatter closing delimiter")?;
    let metadata: Metadata = serde_yaml_ng::from_str(yaml).context("Invalid skill metadata")?;
    if metadata.name.is_empty()
        || metadata.name.len() > 64
        || metadata.name.starts_with('-')
        || metadata.name.ends_with('-')
        || !metadata
            .name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    {
        bail!(
            "Skill name must be 1–64 lowercase letters, digits or hyphens, without leading/trailing hyphens"
        );
    }
    if metadata.description.trim().is_empty() || body.trim().is_empty() {
        bail!("Skill description and instructions must not be empty");
    }
    Ok((metadata, body.trim().to_owned()))
}

impl Skills {
    /// Later roots override earlier roots by skill name. Only direct child directories are scanned.
    pub fn discover(roots: &[PathBuf]) -> Self {
        let mut skills = Self::default();
        for root in roots {
            let entries = match fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    skills.warnings.push(format!("{}: {error}", root.display()));
                    continue;
                }
            };
            let mut paths = Vec::new();
            for entry in entries {
                match entry {
                    Ok(entry) => {
                        let path = entry.path().join("SKILL.md");
                        if path.is_file() {
                            paths.push(path);
                        }
                    }
                    Err(error) => skills.warnings.push(format!("{}: {error}", root.display())),
                }
            }
            paths.sort();
            let mut names = BTreeSet::new();
            for path in paths {
                let result = (|| -> Result<Skill> {
                    let (metadata, _) = parse(&read_skill(&path)?)?;
                    if !names.insert(metadata.name.clone()) {
                        bail!(
                            "Duplicate skill name '{}' in {}",
                            metadata.name,
                            root.display()
                        );
                    }
                    Ok(Skill {
                        name: metadata.name,
                        description: metadata.description,
                        path: path.canonicalize()?,
                    })
                })();
                match result {
                    Ok(skill) => {
                        skills.entries.insert(skill.name.clone(), skill);
                    }
                    Err(error) => skills
                        .warnings
                        .push(format!("{}: {error:#}", path.display())),
                }
            }
        }
        skills
    }

    pub fn entries(&self) -> impl Iterator<Item = &Skill> {
        self.entries.values()
    }

    pub fn suggestions(&self, prefix: &str) -> Vec<&Skill> {
        self.entries
            .values()
            .filter(|skill| skill.name.starts_with(prefix))
            .collect()
    }

    /// Load requested instructions at submission time; store the expansion in conversation history.
    pub async fn expand(&self, prompt: String) -> Result<String> {
        let mut requested = Vec::new();
        let mut seen = BTreeSet::new();
        for token in prompt.split_whitespace() {
            let Some(rest) = token.strip_prefix('$') else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
                .collect();
            // Do not interpret shell variables such as $HOME or escaped \$name as skills.
            if name.is_empty()
                || rest.get(name.len()..).is_some_and(|suffix| {
                    suffix.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
                })
            {
                continue;
            }
            let skill = self.entries.get(&name).with_context(|| {
                format!("Unknown skill '${name}'. Install it in ~/.ri/skills or .ri/skills")
            })?;
            if seen.insert(name) {
                requested.push(skill.clone());
            }
        }
        if requested.is_empty() {
            return Ok(prompt);
        }
        tokio::task::spawn_blocking(move || {
            let mut expanded = prompt;
            for skill in requested {
                let (metadata, body) = parse(&read_skill(&skill.path).with_context(|| format!("Loading skill '{}' from {}", skill.name, skill.path.display()))?)?;
                if metadata.name != skill.name { bail!("Skill '{}' was renamed; restart ri to rediscover skills", skill.name); }
                let directory = skill.path.parent().context("Skill has no parent directory")?;
                write!(expanded, "\n\n--- Skill: {} ---\nSkill directory: {}\nResolve relative paths in these instructions against this directory.\n\n{}\n--- End skill: {} ---", skill.name, directory.display(), body, skill.name)?;
                if expanded.len() > MAX_EXPANDED_BYTES { bail!("Expanded skill prompt exceeds {MAX_EXPANDED_BYTES} bytes"); }
            }
            Ok(expanded)
        }).await.context("Skill loader stopped")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(root: &Path, directory: &str, name: &str, body: &str) -> Result<()> {
        let path = root.join(directory);
        fs::create_dir_all(&path)?;
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: >\n  A useful skill\n---\n{body}"),
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn discovers_overrides_and_expands_only_explicit_skills() -> Result<()> {
        let root = tempfile::tempdir()?;
        let user = root.path().join("user");
        let project = root.path().join("project");
        install(&user, "review", "review", "User instructions")?;
        install(&project, "review", "review", "Project instructions")?;
        install(&user, "tests", "tests", "Test instructions")?;
        install(&user, "invalid", "UPPERCASE", "Invalid")?;
        let skills = Skills::discover(&[user.clone(), project]);
        assert_eq!(skills.entries().count(), 2);
        assert_eq!(skills.warnings.len(), 1);
        assert_eq!(skills.suggestions("rev").len(), 1);
        let text = skills
            .expand("$review and $tests then $review".into())
            .await?;
        assert!(text.contains("Project instructions"));
        assert!(!text.contains("User instructions"));
        assert_eq!(text.matches("--- Skill: review ---").count(), 1);
        assert!(text.contains("Skill directory:"));
        assert!(skills.expand("$missing".into()).await.is_err());
        assert_eq!(
            skills
                .expand("$HOME \\$review ordinary text".into())
                .await?,
            "$HOME \\$review ordinary text"
        );
        fs::remove_file(user.join("tests/SKILL.md"))?;
        assert!(skills.expand("$tests".into()).await.is_err());
        Ok(())
    }

    #[test]
    fn rejects_invalid_and_oversized_skills_and_accepts_crlf() -> Result<()> {
        assert!(parse("no frontmatter").is_err());
        assert!(parse("---\nname: empty\ndescription: test\n---\n").is_err());
        assert!(parse("---\r\nname: test\r\ndescription: Test\r\n---\r\nInstructions").is_ok());
        let root = tempfile::tempdir()?;
        install(root.path(), "large", "large", &"x".repeat(MAX_SKILL_BYTES))?;
        let skills = Skills::discover(&[root.path().to_owned()]);
        assert_eq!(skills.entries().count(), 0);
        assert_eq!(skills.warnings.len(), 1);
        Ok(())
    }
}
