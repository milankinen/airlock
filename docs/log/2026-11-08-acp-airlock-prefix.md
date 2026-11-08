# `airlock: ` prefix on ACP proxy texts

Each error, notice and warning that `airlock acp` makes itself now
starts with `airlock: `.

## Why

The editor shows proxy errors and session notices in the same place as
the agent texts. The user could not see if a text came from the agent or
from airlock, for example a refused `fs/write_text_file` or a sandbox
start failure.

## Design

* `messages::acp::from_airlock` adds the prefix. The texts in
  `messages.rs` stay without it.
* `rpc::error_response` (and `error_response_with_data`) adds it. All
  JSON-RPC errors that the proxy makes go through it. This includes the
  refusals of agent requests that go to the agent: agents often relay
  these errors to the user.
* `notice_message` adds it to the notice title. The text fallback for
  clients without notices is `<title>: <detail>`, so it gets the prefix
  too.
* Agent messages that the proxy forwards stay as they are. The login
  hint that `add_auth_hint` adds to an agent `auth_required` error has
  no prefix, because the message starts with the agent text.
* Texts that started with "airlock ..." were reworded, so the result is
  not "airlock: airlock ...".
* Error details that come from the sandbox start (last error line) or
  from the agent (reopen error) are put inside one prefixed message. No
  message gets two prefixes.
