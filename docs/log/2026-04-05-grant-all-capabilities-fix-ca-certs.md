# Grant all capabilities, fix CA certs

Three fixes to make containers work properly:

1. **All Linux capabilities**: crun drops capabilities by default. Tools
   like `apt-get` need `CAP_SETUID`/`CAP_SETGID` to drop privileges.
   Since the VM is the security boundary, grant all capabilities in the
   OCI spec — no reason to restrict inside the VM.

2. **CA cert paths**: The MITM CA cert was only installed at the Debian
   path (`/etc/ssl/certs/ca-certificates.crt`). Alpine's LibreSSL reads
   `/etc/ssl/cert.pem`. Now writes to all common distro paths.
