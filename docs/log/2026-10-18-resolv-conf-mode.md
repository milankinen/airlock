# Fix DNS for images with an unreadable resolv.conf

With the `curlimages/curl` image, no name resolved in the sandbox:

```
curl: (6) Could not resolve host: mcp.airlock
```

The VM bats tests of `mcp.airlock` failed with this error.

## Cause

The image layer has `/etc/resolv.conf` with mode `0700`, owner root.
The image runs as `curl_user`. At boot, the guest wrote the in-VM DNS
server address into the existing file. A write keeps the mode of the
file, so the user could not read it. musl then used its default name
server `127.0.0.1:53`, where nothing listens, and all names failed.

The DNS server itself was correct: the same server code, run on the
host, gave `127.0.0.1` for `mcp.airlock` to the curl of this image.

## Fix

* Remove the `resolv.conf` of the image and make a new file with mode
  `0644`.
* The remove also stops a symlink in the image from sending the write
  out of the container rootfs.
* The `mcp.airlock` bats tests use this image, so they cover the fix.

## Second listener on 127.0.0.1:53

musl and glibc use `127.0.0.1:53` when `resolv.conf` is missing or
cannot be read. The DNS server now also listens there, so such images
still resolve names. It does not listen on `0.0.0.0:53`:

* A wildcard socket holds port 53 on all addresses. Sandbox programs
  could then not bind port 53 on any address, for example `127.0.0.53`.
* A wildcard UDP socket replies from an address that the kernel picks
  from the route. musl drops a reply from an address that it did not
  ask, for example a query to `127.0.0.53` and a reply from `127.0.0.1`.
