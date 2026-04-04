# Tiberius — Materialize Fork

This is Materialize's fork of [tiberius](https://github.com/prisma/tiberius), a TDS (MSSQL/SQL Server) driver for Rust.

## Current Migration Task (SEC-248)

**Goal**: Migrate from `native-tls` (openssl) to `rustls 0.23` + `aws-lc-rs` so that `openssl-sys` can be removed from the Materialize workspace.

### Current state

- Default feature: `native-tls` (via `async-native-tls` → `openssl`)
- Existing `rustls` feature uses `tokio-rustls 0.24` → `rustls 0.21` → **`ring`** (also banned in Materialize)
- The `rustls` feature must be upgraded to `rustls 0.23+` which supports `aws-lc-rs`

### What needs to change

1. **Upgrade rustls dependencies**:
   - `tokio-rustls` from `0.24` (rustls 0.21/ring) → `0.26` (rustls 0.23/aws-lc-rs)
   - `rustls-pemfile` from current → `2.x`
   - `rustls-native-certs` from current → `0.8.x` (compatible with rustls 0.23)
   - Remove `async-native-tls` dependency (only needed for native-tls feature)

2. **Update TLS connection code**: The rustls 0.21 → 0.23 API changed significantly:
   - `rustls::ClientConfig::builder()` → `rustls::ClientConfig::builder_with_provider(provider)`
   - `rustls::Certificate` → `rustls::pki_types::CertificateDer`
   - `rustls::PrivateKey` → `rustls::pki_types::PrivateKeyDer`
   - `rustls::ServerName` → `rustls::pki_types::ServerName`
   - Root cert store API changed

3. **Keep the `native-tls` feature compiling** (for other users of tiberius), but Materialize will only use the `rustls` feature.

4. **Test**: SQL Server connectivity must still work — Materialize uses tiberius for SQL Server CDC sources.

### Key files to modify

- `Cargo.toml` — dependency versions and features
- `src/client/tls_stream.rs` or similar — TLS connection setup
- Any file importing from `tokio_rustls`, `rustls`, `rustls_pemfile`, `rustls_native_certs`

### Constraints

- The updated `rustls` feature must NOT depend on `ring`. It must use `aws-lc-rs` as the crypto provider.
- The crate should accept a `CryptoProvider` from the caller (or use the process-wide default) rather than hardcoding one.
- **IMPORTANT: Push to the `personal` remote, NOT `origin`.** Origin is the shared MaterializeInc fork — pushing there affects other developers' builds. Use `git push personal <branch>` for all pushes. Work on a feature branch (e.g., `rustls-0.23-upgrade`). The branch will be integrated into Materialize via a git reference to `jasonhernandez/tiberius` in `Cargo.toml` after validation.

### How Materialize consumes this

In `materialize/Cargo.toml`:
```toml
tiberius = { git = "https://github.com/MaterializeInc/tiberius", rev="<new-rev>" }
```

In workspace crates (`mz-sql-server-util`, `mz-storage-types`, `mz-storage`):
```toml
tiberius = { version = "0.12", features = ["sql-browser-tokio", "tds73", "rustls"], default-features = false }
```

### Validation

After making changes:
```bash
cargo check --features rustls
cargo test --features rustls
# Verify ring is NOT in the dependency tree
cargo tree -i ring --features rustls 2>&1  # should print "nothing to print"
# Verify rustls version is 0.23+
cargo tree -p rustls --features rustls
```
