-- Python: point the TLS stacks at the system bundle, which has the
-- airlock CA. python-build-standalone builds (uv, mise) look for
-- /etc/ssl/cert.pem, which is missing on Debian. uv trusts only its own
-- CA list without SSL_CERT_FILE. requests and pip have their own CA
-- lists. With `allow-pypi`, PyPI.

-- The setup script gives the version to `uv python install`. An option
-- there (a leading `-`) or a character outside a uv Python request is
-- an error.
local version = pack.args["python-version"]
if version:sub(1, 1) == "-" or version:find("[^%w.@+_-]") then
    fail("python-version `" .. version .. "` is not a Python request for uv "
        .. "(for example 3.12, 3.12.4, 3.13t or pypy@3.11)")
end

local bundle = "/etc/ssl/certs/ca-certificates.crt"
config.env = {
    SSL_CERT_FILE = bundle,
    REQUESTS_CA_BUNDLE = bundle,
    PIP_CERT = bundle,
}

if pack.args["allow-pypi"] then
    config.network = {
        rules = {
            ["python-packages"] = {
                allow = {
                    "pypi.org",
                    "pypi.python.org",
                    "files.pythonhosted.org",
                    "pythonhosted.org",
                },
            },
        },
    }
end
