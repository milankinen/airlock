# TLS interception: certificates for strict X.509 checks

## Symptom

Python 3.13 and later set `VERIFY_X509_STRICT` in
`ssl.create_default_context()`. In a sandbox, `urllib`, `requests` and
other clients on the default context failed on each intercepted host:

```
certificate verify failed: Missing Authority Key Identifier
```

pip (through `truststore`) and uv worked, so package installs succeeded
and the error showed later, in the project code.

## Cause

`openssl verify -x509_strict` on the leaf and the sandbox CA gave three
errors:

```
error 85 at 0 depth lookup: Missing Authority Key Identifier
error 81 at 1 depth lookup: Path length given without key usage keyCertSign
error 92 at 1 depth lookup: CA cert does not include key usage extension
```

- The leaf had only the SAN. rcgen adds the Authority Key Identifier
  only when `use_authority_key_identifier_extension` is set.
- The CA has `pathlen:0` but no Key Usage. Strict mode needs
  `keyCertSign` on a CA with a path length.

Python stops at the first error, so only the leaf error showed. A fix
of the leaf alone moves the error to the CA.

## Fix

- Leaf: Authority Key Identifier, Subject Key Identifier and
  `CA:FALSE` (`IsCa::ExplicitNoCa`), Key Usage `digitalSignature`,
  Extended Key Usage `serverAuth`. Strict mode needs only the Authority
  Key Identifier. The others are the RFC 5280 profile of a TLS server
  leaf, so other strict clients also accept it. The leaf key is ECDSA
  P-256, so `keyEncipherment` is not necessary.
- CA: Key Usage `keyCertSign` and `cRLSign` (critical, as rcgen writes
  it).

The Authority Key Identifier comes from the Subject Key Identifier of the
CA certificate: `Issuer::from_ca_cert_pem` reads it as a pre-specified
key id. Thus the leaf matches a CA from an older airlock version too.

## Existing sandboxes

`ca.json` stays for the life of a sandbox, so an old sandbox keeps the
CA with no Key Usage. Its new leaves get the Authority Key Identifier,
and strict mode then refuses the CA (error 81) instead of the leaf. That
is no regression: strict clients failed before too. Non-strict clients
see no change.

airlock does not change an existing CA in place. A pack can run a trust
update tool (`update-ca-certificates`) that writes a bundle with the old
CA to the persistent upper layer. A changed CA could then disagree with
that bundle. The manual tells the user to remove the sandbox to get a
new CA.

## Tests

- `test_tls_mitm::mitm_certificate_chain_has_extensions_for_strict_verification`
  sends an HTTPS request through the MITM and checks the chain that the
  guest gets: leaf AKI equal to CA SKI, CA Key Usage `keyCertSign`
  (critical) with `pathlen:0`, leaf `CA:FALSE`, `digitalSignature` and
  `serverAuth`. It fails without the leaf change.
- The network test harness now makes its MITM CA with the same function
  as a real sandbox, not with the generic test CA. All TLS MITM tests
  thus run with the real CA profile.
- Manual check (not in the suite): Python 3.13.18 (OpenSSL 3.5.9) and
  3.14.7 (OpenSSL 3.6.4) with the default context against the
  interceptor. An old-profile CA gives `Path length given without key
  usage keyCertSign`. The new CA connects.
