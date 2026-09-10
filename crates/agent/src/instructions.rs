//! Project Markdown is context, never executable configuration.
use std::{
    fmt::Write as _,
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

const MAX_FILE_BYTES: usize = 128 * 1024;
const MAX_TOTAL_BYTES: usize = 512 * 1024;
const MAX_DIRECTORIES: usize = 4096;
const SKIP_DIRECTORIES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    ".direnv",
    ".cache",
    ".venv",
    "__pycache__",
];

#[derive(Debug, thiserror::Error)]
pub enum InstructionError {
    #[error("Cannot load project instructions at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Instruction file {0} is not a regular file")]
    NotFile(PathBuf),
    #[error("Instruction file {0} exceeds 128 KiB")]
    FileTooLarge(PathBuf),
    #[error("Combined project instructions exceed 512 KiB")]
    TotalTooLarge,
    #[error(
        "Project instruction discovery exceeds 4096 directories; narrow the working directory or use --no-project-instructions"
    )]
    TooManyDirectories,
    #[error("Project instruction loader stopped: {0}")]
    Task(#[from] tokio::task::JoinError),
}

type Result<T> = std::result::Result<T, InstructionError>;

fn io<T>(path: &Path, result: std::io::Result<T>) -> Result<T> {
    result.map_err(|source| InstructionError::Io {
        path: path.to_owned(),
        source,
    })
}

pub struct ProjectInstructions {
    pub paths: Vec<PathBuf>,
    text: String,
}

impl ProjectInstructions {
    pub async fn load(directory: PathBuf) -> Result<Self> {
        tokio::task::spawn_blocking(move || Self::discover(&directory)).await?
    }

    fn discover(directory: &Path) -> Result<Self> {
        let cwd = io(directory, directory.canonicalize())?;
        // A linked worktree uses a .git file, so existence matters, not its type.
        let mut root = cwd.as_path();
        for ancestor in cwd.ancestors() {
            match fs::symlink_metadata(ancestor.join(".git")) {
                Ok(_) => {
                    root = ancestor;
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(InstructionError::Io {
                        path: ancestor.join(".git"),
                        source,
                    });
                }
            }
        }
        let mut ancestors: Vec<_> = cwd
            .ancestors()
            .take_while(|path| path.starts_with(root))
            .collect();
        ancestors.reverse();
        let mut instructions = Self {
            paths: Vec::new(),
            text: format!(
                "\n\n--- Current project instructions ---\nWorking directory: {}\nThis snapshot replaces project instructions from earlier prompts, including removed files. Explicit user requests take precedence. Each AGENTS.md applies only to its containing directory and descendants; deeper files override ancestor rules only within that subtree. Sibling rules do not apply outside their scope. Read applicable instructions before changing files.\n",
                cwd.display()
            ),
        };
        for ancestor in ancestors {
            instructions.add(ancestor)?;
        }
        let mut visited = 0;
        let mut pending = vec![cwd];
        while let Some(directory) = pending.pop() {
            visited += 1;
            if visited > MAX_DIRECTORIES {
                return Err(InstructionError::TooManyDirectories);
            }
            let mut children = Vec::new();
            for entry in io(&directory, fs::read_dir(&directory))? {
                let entry = io(&directory, entry)?;
                let kind = io(&entry.path(), entry.file_type())?;
                if kind.is_dir()
                    && !SKIP_DIRECTORIES
                        .iter()
                        .any(|skip| entry.file_name() == *skip)
                {
                    if visited + pending.len() + children.len() >= MAX_DIRECTORIES {
                        return Err(InstructionError::TooManyDirectories);
                    }
                    children.push(entry.path());
                }
            }
            children.sort();
            for child in &children {
                instructions.add(child)?;
            }
            pending.extend(children.into_iter().rev());
        }
        if instructions.paths.is_empty() {
            instructions
                .text
                .push_str("No AGENTS.md files are currently present in the discovery scope.\n");
        }
        instructions
            .text
            .push_str("--- End project instructions ---");
        if instructions.text.len() > MAX_TOTAL_BYTES {
            return Err(InstructionError::TotalTooLarge);
        }
        Ok(instructions)
    }

    fn add(&mut self, directory: &Path) -> Result<()> {
        let path = directory.join("AGENTS.md");
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            result => {
                io(&path, result)?;
            }
        }
        if !io(&path, fs::metadata(&path))?.is_file() {
            return Err(InstructionError::NotFile(path));
        }
        let file = io(&path, fs::File::open(&path))?;
        let mut body = String::new();
        io(
            &path,
            file.take((MAX_FILE_BYTES + 1) as u64)
                .read_to_string(&mut body),
        )?;
        if body.len() > MAX_FILE_BYTES {
            return Err(InstructionError::FileTooLarge(path));
        }
        let _ = write!(
            self.text,
            "\nInstruction file: {}\nScope: {} and descendants\n\n{}\n",
            path.display(),
            directory.display(),
            body
        );
        if self.text.len() > MAX_TOTAL_BYTES {
            return Err(InstructionError::TotalTooLarge);
        }
        self.paths.push(path);
        Ok(())
    }

    pub fn append_to(self, mut prompt: String) -> String {
        prompt.push_str(&self.text);
        prompt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) -> anyhow::Result<()> {
        fs::create_dir_all(path)?;
        fs::write(path.join("AGENTS.md"), text)?;
        Ok(())
    }

    #[tokio::test]
    async fn loads_ancestor_and_nested_scopes_without_siblings_or_generated_directories()
    -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        write(temp.path(), "outside repository")?;
        write(&root, "root rules")?;
        fs::write(root.join(".git"), "gitdir: unused")?;
        write(&root.join("src"), "source rules")?;
        write(&root.join("src/nested"), "nested rules")?;
        write(&root.join("other"), "sibling rules")?;
        write(&root.join("src/target"), "generated rules")?;
        let instructions = ProjectInstructions::load(root.join("src")).await?;
        assert_eq!(
            instructions.paths,
            [
                root.join("AGENTS.md"),
                root.join("src/AGENTS.md"),
                root.join("src/nested/AGENTS.md")
            ]
        );
        let text = instructions.append_to("request".into());
        assert!(text.contains("Explicit user requests take precedence"));
        assert!(text.contains("Scope:"));
        assert!(!text.contains("outside repository"));
        assert!(!text.contains("sibling rules"));
        assert!(!text.contains("generated rules"));
        Ok(())
    }

    #[tokio::test]
    async fn reloads_changes_and_uses_cwd_as_root_without_git() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write(temp.path(), "unrelated parent")?;
        let cwd = temp.path().join("plain");
        write(&cwd, "original")?;
        let first = ProjectInstructions::load(cwd.clone()).await?;
        assert_eq!(first.paths.len(), 1);
        assert!(first.append_to(String::new()).contains("original"));
        fs::remove_file(cwd.join("AGENTS.md"))?;
        let second = ProjectInstructions::load(cwd).await?;
        assert!(second.paths.is_empty());
        let text = second.append_to(String::new());
        assert!(text.contains("No AGENTS.md files"));
        assert!(!text.contains("unrelated parent"));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_oversized_and_non_utf8_files() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        write(temp.path(), &"x".repeat(MAX_FILE_BYTES + 1))?;
        assert!(matches!(
            ProjectInstructions::load(temp.path().into()).await,
            Err(InstructionError::FileTooLarge(_))
        ));
        fs::write(temp.path().join("AGENTS.md"), [0xff])?;
        assert!(ProjectInstructions::load(temp.path().into()).await.is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skips_directory_symlinks_and_reads_instruction_file_symlinks() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        fs::create_dir(&root)?;
        fs::write(temp.path().join("rules.md"), "linked rules")?;
        std::os::unix::fs::symlink(temp.path().join("rules.md"), root.join("AGENTS.md"))?;
        std::os::unix::fs::symlink(&root, root.join("loop"))?;
        let instructions = ProjectInstructions::load(root.clone()).await?;
        assert_eq!(instructions.paths, [root.join("AGENTS.md")]);
        assert!(
            instructions
                .append_to(String::new())
                .contains("linked rules")
        );
        Ok(())
    }

    #[tokio::test]
    async fn rejects_nonregular_files_and_excessive_combined_context() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        fs::create_dir(temp.path().join("AGENTS.md"))?;
        assert!(matches!(
            ProjectInstructions::load(temp.path().into()).await,
            Err(InstructionError::NotFile(_))
        ));
        fs::remove_dir(temp.path().join("AGENTS.md"))?;
        for index in 0..5 {
            write(
                &temp.path().join(format!("scope-{index}")),
                &"x".repeat(MAX_FILE_BYTES),
            )?;
        }
        assert!(matches!(
            ProjectInstructions::load(temp.path().into()).await,
            Err(InstructionError::TotalTooLarge)
        ));
        Ok(())
    }
}
