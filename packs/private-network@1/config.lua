-- Private network: one rule that denies or allows the private addresses.
-- The `except` addresses get the other mode. A deny always wins, thus the
-- deny list must not have the allowed addresses in it.

local except = {}
if pack.args["except"] ~= "none" then
    for item in pack.args["except"]:gmatch("[^,%s]+") do
        table.insert(except, item)
    end
end

-- `exclude` also refuses an `except` entry that is not valid.
local private = ip.exclude(ip.private_ranges(), except)

local rule = {}
if pack.args["mode"] == "deny" then
    rule.deny = private
    -- An empty Lua table is not a list in the config. Set only a list
    -- with entries.
    if #except > 0 then
        rule.allow = except
    end
else
    rule.allow = private
    if #except > 0 then
        rule.deny = except
    end
end

config.network = { rules = { ["private-network"] = rule } }
