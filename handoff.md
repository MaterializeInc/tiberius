## SEC-248 Handoff: tiberius rustls migration

### What was done

The `rustls` feature in tiberius has been migrated from rustls 0.21 (ring) to rustls 0.23 (aws-lc-rs). The work is on branch `rustls-0.23-upgrade` at `jasonhernandez/tiberius` (commit `90e3628`).

**Dependency changes:**

| Crate | Before | After |
|-------|--------|-------|
| `rustls` | 0.21 (implicit via tokio-rustls) | 0.23 (explicit) |
| `tokio-rustls` | 0.24 | 0.26 |
| `rustls-pemfile` | 1 | 2 |
| `rustls-native-certs` | 0.6 | 0.8 |
| Crypto backend | ring | aws-lc-rs |

**Key design choice:** tiberius checks for a process-wide `CryptoProvider` first, falling back to aws-lc-rs. If Materialize installs a provider at startup (e.g., `mz_ore::crypto::fips_crypto_provider()`), tiberius will use it automatically.

### What the Materialize agent needs to do

1. **Update the tiberius git reference** in the Materialize workspace `Cargo.toml`:
   ```toml
   tiberius = { git = "https://github.com/jasonhernandez/tiberius", branch = "rustls-0.23-upgrade" }
   ```

2. **Verify the consuming crates** (`mz-sql-server-util`, `mz-storage-types`, `mz-storage`) still compile. They use:
   ```toml
   tiberius = { version = "0.12", features = ["sql-browser-tokio", "tds73", "rustls"], default-features = false }
   ```
   No feature name changes -- `rustls` is still `rustls`.

3. **Verify no ring in the workspace tree** from this path:
   ```bash
   cargo tree -i ring --target all --depth 2 2>&1 | grep tiberius
   # should return nothing
   ```

4. **Check for openssl-sys removal** -- if tiberius was the last crate pulling in openssl-sys, confirm it's gone. Other crates (rdkafka, postgres, etc.) may still hold it.

5. **Update `deny.toml` / `about.toml`** if needed -- aws-lc-rs and aws-lc-sys are new transitive deps that may need license or ban entries.

### Gotchas

- The `native-tls` feature still compiles and is unchanged -- no risk to other tiberius consumers.
- tiberius's dev-dependencies still pull in ring (via `reqwest` 0.11 used in test fixtures). This does **not** affect downstream consumers.
- If Materialize's CI cross-compiles with static linking, ensure aws-lc-sys builds cleanly for the target. It uses cmake -- the CI builder image needs cmake installed (it likely already has it for aws-lc-rs usage elsewhere).
