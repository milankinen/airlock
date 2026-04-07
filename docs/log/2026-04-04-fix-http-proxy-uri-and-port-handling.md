# Fix HTTP proxy URI and port handling

The HTTP proxy was sending absolute URIs (`GET http://host:port/path`)
to h1 origin servers. HTTP/1.1 origin servers expect relative paths
(`GET /path`) with the authority in the Host header — absolute URIs
are only for forward proxies. This caused Python SimpleHTTPServer
(and likely other servers) to 404 on every request.

Also fixed missing port in the outgoing URI for h2.

Now: h1 uses relative path, h2 uses absolute URI (needed for
`:authority`/`:scheme` pseudo-headers). Protocol is passed through
from ALPN detection to the request handler.
