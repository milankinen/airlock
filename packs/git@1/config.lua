-- Git: commit author and committer from the host.

-- Read a key from the global git config of the host
local function git_config(key)
    local cmd = io.popen("git config --global --includes --get " .. key .. " 2>/dev/null")
    if not cmd then
        return nil
    end
    local value = cmd:read("l")
    cmd:close()
    if value == nil or value == "" then
        return nil
    end
    return value
end

-- Pick first non-empty value: the host env var, the role key
-- (for example `author.name`), then the user key (`user.name`).
local function ident(role, field)
    local env = os.getenv("GIT_" .. role:upper() .. "_" .. field:upper())
    if env ~= nil and env ~= "" then
        return env
    end
    return git_config(role .. "." .. field) or git_config("user." .. field)
end

-- Env values are `${VAR}` templates. Escape `\` and `$`, thus a name such
-- as "Jo $HOME" stays as it is.
local function literal(value)
    return (value:gsub("[\\$]", "\\%0"))
end

config.env = {}
for _, role in ipairs({ "author", "committer" }) do
    for _, field in ipairs({ "name", "email" }) do
        local value = ident(role, field)
        if value ~= nil then
            config.env["GIT_" .. role:upper() .. "_" .. field:upper()] = literal(value)
        end
    end
end
