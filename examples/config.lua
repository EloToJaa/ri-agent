-- Copy to ~/.config/ri-agent/config.lua, or pass --config examples/config.lua.
-- This file is executable, trusted Lua 5.4. Never load config from untrusted sources.
return {
    settings = {
        model = "anthropic/claude-haiku-4.5",
        -- reasoning_effort = "high", -- Optional; must be advertised by the selected model.
        -- Omit reasoning_effort to preserve provider defaults.
        base_url = "https://openrouter.ai/api/v1",
        max_turns = 20,
        command_timeout = 60, -- Built-in Bash only, in seconds.
        max_output_bytes = 32768,
    },
    hooks = {
        -- Return a replacement string, or nil to keep the original.
        before_prompt = function(prompt)
            return prompt
        end,
        -- Called for each assistant message containing text, including tool turns.
        -- The replacement is displayed AND stored in the conversation.
        after_response = function(response)
            return response
        end,
    },
    tools = {
        {
            name = "Echo",
            description = "Return the supplied text",
            parameters = {
                type = "object",
                properties = { text = { type = "string" } },
                required = { "text" },
                additionalProperties = false,
            },
            execute = function(args)
                assert(type(args.text) == "string", "text must be a string")
                return args.text -- Strings returned verbatim; other values encoded as JSON.
            end,
        },
    },
}
