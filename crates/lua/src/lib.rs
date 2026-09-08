mod reasoning;
pub use reasoning::ReasoningEffort;

use anyhow::{Context, Result, bail};
use mlua::{Function, Lua, LuaSerdeExt, Table};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashSet, env, path::PathBuf, sync::Arc};

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
}

impl LuaConfig {
    pub fn load(path: Option<PathBuf>) -> Result<Arc<Self>> {
        let explicit = path.is_some();
        let path = path.or_else(|| {
            env::var_os("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
                .map(|base| base.join("ri-agent/config.lua"))
        });
        let Some(path) = path else {
            return Self::from_source("return {}", "default");
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
        for pair in root.clone().pairs::<String, mlua::Value>() {
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
        let mut names: HashSet<String> = ["Read", "Write", "Bash"].map(String::from).into();
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
        lua.set_named_registry_value("config", root)?;
        lua.set_named_registry_value("handlers", handlers)?;
        Ok(Arc::new(Self {
            lua,
            settings,
            definitions,
        }))
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.definitions
            .iter()
            .any(|definition| definition["function"]["name"] == name)
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
    async fn loads_settings_hooks_and_custom_tools() {
        let config = LuaConfig::from_source(
            r#"return {
            settings = { model = 'mock', max_turns = 3 },
            hooks = { before_prompt = function(s) return 'prefix: ' .. s end },
            tools = {{ name = 'Echo', description = 'Echo input',
                parameters = { type = 'object' },
                execute = function(args) return args.text end }}
        }"#,
            "test",
        )
        .unwrap();
        assert_eq!(config.settings.model.as_deref(), Some("mock"));
        assert_eq!(
            config.hook("before_prompt", "hello".into()).await.unwrap(),
            "prefix: hello"
        );
        assert_eq!(
            config.hook("after_response", "hello".into()).await.unwrap(),
            "hello"
        );
        assert_eq!(
            config
                .execute("Echo".into(), r#"{"text":"hi"}"#.into())
                .await
                .unwrap(),
            "hi"
        );
    }

    #[tokio::test]
    async fn example_configuration_loads_and_callback_errors_are_reported() {
        let config =
            LuaConfig::from_source(include_str!("../../../examples/config.lua"), "example")
                .unwrap();
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
        )
        .unwrap();
        let error = config
            .hook("before_prompt", "hello".into())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("hook failed"));
    }

    #[test]
    fn rejects_invalid_configuration() {
        for source in [
            "invalid lua",
            "return {settings={max_turns=0}}",
            "return {typo=true}",
            "return {hooks={unknown=function() end}}",
            "return {tools={{name='Read'}}}",
        ] {
            assert!(LuaConfig::from_source(source, "test").is_err());
        }
    }
}
