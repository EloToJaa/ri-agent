mod reasoning;
pub use reasoning::ReasoningEffort;

use anyhow::{Context, Result, bail};
use mlua::{Function, Lua, LuaSerdeExt, Table};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashSet, env, path::PathBuf, sync::Arc};

/// Default directory for harness configuration and persistent state.
pub fn harness_directory() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .context(
            "HOME is not set; supply explicit --config and --session-db paths (or --no-save)",
        )?;
    Ok(PathBuf::from(home).join(".ri"))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub base_url: Option<String>,
    pub max_turns: Option<std::num::NonZeroUsize>,
    pub command_timeout: Option<std::num::NonZeroU64>,
    pub max_output_bytes: Option<std::num::NonZeroUsize>,
}

pub struct LuaConfig {
    lua: Lua,
    pub settings: Settings,
    pub definitions: Vec<Value>,
    pub has_after_response_hook: bool,
}

impl LuaConfig {
    pub fn load(path: Option<PathBuf>) -> Result<Arc<Self>> {
        let explicit = path.is_some();
        let path = match path {
            Some(path) => path,
            None => harness_directory()?.join("config.lua"),
        };
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if !explicit && error.kind() == std::io::ErrorKind::NotFound => {
                return Self::from_source("return {}", "default");
            }
            Err(error) => return Err(error).with_context(|| format!("Reading {}", path.display())),
        };
        Self::from_source(&source, &path.display().to_string())
            .with_context(|| format!("Loading Lua configuration {}", path.display()))
    }

    /// Execute trusted Lua source returning a configuration table.
    pub fn from_source(source: &str, name: &str) -> Result<Arc<Self>> {
        let lua = Lua::new();
        let root: Table = lua.load(source).set_name(name).eval()?;
        for pair in root.pairs::<String, mlua::Value>() {
            let (key, _) = pair?;
            if !["settings", "hooks", "tools"].contains(&key.as_str()) {
                bail!("Unknown configuration key: {key}");
            }
        }
        let settings = match root.get::<Option<Table>>("settings")? {
            Some(table) => lua.from_value(mlua::Value::Table(table))?,
            None => Settings::default(),
        };
        if let Some(hooks) = root.get::<Option<Table>>("hooks")? {
            for pair in hooks.pairs::<String, Function>() {
                let (name, _) = pair?;
                if !["before_prompt", "after_response"].contains(&name.as_str()) {
                    bail!("Unknown hook: {name}");
                }
            }
        }
        let mut names: HashSet<String> = ["Read", "Write", "Edit", "Bash", "Search", "Find"]
            .map(String::from)
            .into();
        let mut definitions = Vec::new();
        let handlers = lua.create_table()?;
        if let Some(tools) = root.get::<Option<Table>>("tools")? {
            for tool in tools.sequence_values::<Table>() {
                let tool = tool?;
                let name: String = tool.get("name")?;
                if name.is_empty() || !names.insert(name.clone()) {
                    bail!("Empty or duplicate tool name: {name}");
                }
                let description: String = tool.get("description")?;
                let parameters: Value = lua.from_value(tool.get("parameters")?)?;
                if parameters.get("type").and_then(Value::as_str) != Some("object") {
                    bail!("Tool {name} parameters must be an object JSON schema");
                }
                handlers.set(name.clone(), tool.get::<Function>("execute")?)?;
                definitions.push(json!({"type": "function", "function": {
                    "name": name, "description": description, "parameters": parameters
                }}));
            }
        }
        let has_after_response_hook = root
            .get::<Option<Table>>("hooks")?
            .map(|hooks| hooks.contains_key("after_response"))
            .transpose()?
            .unwrap_or(false);
        lua.set_named_registry_value("config", root)?;
        lua.set_named_registry_value("handlers", handlers)?;
        Ok(Arc::new(Self {
            lua,
            settings,
            definitions,
            has_after_response_hook,
        }))
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.definitions.iter().any(|definition| {
            definition.pointer("/function/name").and_then(Value::as_str) == Some(name)
        })
    }

    pub async fn hook(self: &Arc<Self>, name: &'static str, text: String) -> Result<String> {
        let config = Arc::clone(self);
        tokio::task::spawn_blocking(move || -> Result<String> {
            let root: Table = config.lua.named_registry_value("config")?;
            let Some(hooks) = root.get::<Option<Table>>("hooks")? else {
                return Ok(text);
            };
            let Some(hook) = hooks.get::<Option<Function>>(name)? else {
                return Ok(text);
            };
            let replacement: Option<String> = hook.call(text.clone())?;
            Ok(replacement.unwrap_or(text))
        })
        .await
        .context("Lua hook task failed")?
        .with_context(|| format!("Lua hook {name} failed"))
    }

    pub async fn execute(self: &Arc<Self>, name: String, arguments: String) -> Result<String> {
        let config = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let arguments: Value =
                serde_json::from_str(&arguments).context("Invalid Lua tool arguments")?;
            let handlers: Table = config.lua.named_registry_value("handlers")?;
            let function: Function = handlers.get(name)?;
            let result: mlua::Value = function.call(config.lua.to_value(&arguments)?)?;
            if let mlua::Value::String(text) = result {
                return Ok(text.to_str()?.to_owned());
            }
            let value: Value = config.lua.from_value(result)?;
            Ok(serde_json::to_string(&value)?)
        })
        .await
        .context("Lua tool task failed")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loads_settings_hooks_and_custom_tools() -> Result<()> {
        let config = LuaConfig::from_source(
            r"return {
            settings = { model = 'mock', max_turns = 3 },
            hooks = { before_prompt = function(s) return 'prefix: ' .. s end },
            tools = {{ name = 'Echo', description = 'Echo input',
                parameters = { type = 'object' },
                execute = function(args) return args.text end }}
        }",
            "test",
        )?;
        assert_eq!(config.settings.model.as_deref(), Some("mock"));
        assert_eq!(
            config.hook("before_prompt", "hello".into()).await?,
            "prefix: hello"
        );
        assert_eq!(
            config.hook("after_response", "hello".into()).await?,
            "hello"
        );
        assert_eq!(
            config
                .execute("Echo".into(), r#"{"text":"hi"}"#.into())
                .await?,
            "hi"
        );
        Ok(())
    }

    #[tokio::test]
    async fn example_configuration_loads_and_callback_errors_are_reported() -> Result<()> {
        let config =
            LuaConfig::from_source(include_str!("../../../examples/config.lua"), "example")?;
        assert!(config.has_tool("Echo"));
        assert!(config.execute("Echo".into(), "{}".into()).await.is_err());
        assert!(
            config
                .execute("Echo".into(), "invalid json".into())
                .await
                .is_err()
        );
        let config = LuaConfig::from_source(
            "return {hooks={before_prompt=function() error('hook failed') end}}",
            "test",
        )?;
        let error = config
            .hook("before_prompt", "hello".into())
            .await
            .err()
            .context("Expected a hook error")?;
        assert!(format!("{error:#}").contains("hook failed"));
        Ok(())
    }

    #[test]
    fn rejects_invalid_configuration() {
        for source in [
            "invalid lua",
            "return {settings={max_turns=0}}",
            "return {typo=true}",
            "return {hooks={unknown=function() end}}",
            "return {tools={{name='Read'}}}",
            "return {tools={{name='Edit'}}}",
        ] {
            assert!(LuaConfig::from_source(source, "test").is_err());
        }
    }
}
