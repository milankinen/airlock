# Decrypt the vault with the KDF params it was written with

## Symptom

The encrypted vault stores its Argon2 parameters (memory, iterations,
parallelism) in the file envelope, but decryption always derived the key with
the current compile-time constants and ignored the stored values. The moment a
vault was written with different parameters — a future airlock release that
bumps the constants, or another tool honoring the self-describing envelope —
the correct passphrase could never open it, and the failure surfaced as the
misleading "wrong passphrase or corrupt data".

## Fix

`load()` now derives the key with the parameters recorded in the file
(`blob.kdf.m/t/p`) rather than the constants. `store()` still writes with the
current constants (the intended default). To keep a hostile or corrupt file
from forcing a huge memory allocation, the loaded parameters are bounded
before use (m ≤ 1 GiB, t ≤ 16, p ≤ 16); anything beyond that is rejected with
a clear error instead of attempted.

## Tests

- `load_uses_file_kdf_params_not_constants` hand-writes a vault whose `t`
  differs from the default and confirms it still decrypts.
- `load_rejects_out_of_bounds_kdf_params` confirms an absurd `m` is refused
  rather than attempted.
