# Masked secrets in HTTP Basic credentials

## Problem

A masked secret as the user name or password of HTTP Basic auth did not
get injected. The client sends `Authorization: Basic base64(user:pass)`.
The header unmask is a byte search for the surrogate, so it did not find
the surrogate in the base64 text. The upstream got the surrogate and
refused the request. Users wrote a Lua middleware to fix it.

## Change

`inject::unmask_request` now runs the normal header rewrite, then
`rewrite_basic_auth`. It checks each `Authorization` value. If the scheme
is `Basic` (case insensitive) and the token decodes as standard base64,
it replaces the surrogates in the decoded bytes with the same unmask
pairs and encodes the result again as `Basic <token>`.

The value stays untouched when the token is not valid base64, or when
the decoded credentials have no surrogate. Thus the proxy does not
change the scheme case or the padding of a header that it does not
need to change.

## Not done

- The response masking does not find the encoded real credentials. An
  upstream that echoes the `Authorization` header sends the base64 of
  the real credentials to the guest. This is the same known limit as
  for other encoded forms. A first version added the encoded real
  credentials as extra mask pairs for each request. It was dropped as
  too complex for the gain.
- Unpadded or URL-safe base64 tokens and `Proxy-Authorization` are not
  decoded.
