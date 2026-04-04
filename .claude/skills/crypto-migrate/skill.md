---
name: crypto-migrate
description: "Migrate a Rust crate or fork from openssl/native-tls/ring to rustls/aws-lc-rs. Handles dependency analysis, feature flag changes, symbol conflict detection, and validation. Triggers on 'migrate crypto', 'migrate TLS', 'switch to rustls', 'remove openssl', 'remove native-tls', 'FIPS migration'."
argument-hint: "<crate or fork name> [--analyze | --migrate | --validate]"
---

# Crypto Migration Skill

Migrate a Rust crate, workspace dependency, or fork from legacy crypto backends (openssl, native-tls, ring) to the FIPS-ready stack (rustls + aws-lc-rs). This skill encodes hard-won lessons from cross-cutting crypto migrations in large Rust workspaces.

## Arguments

| Argument | Description |
|----------|-------------|
| `<crate>` | Crate name (e.g., `tiberius`, `reqwest`, `rdkafka`) |
| `--analyze` | Analysis only: dependency tree, symbol conflicts, migration path (default) |
| `--migrate` | Perform the migration |
| `--validate` | Run post-migration validation checks |

---

## Phase 1: Dependency Analysis

Before touching any code, understand the full dependency picture.

### 1a. Map the current crypto dependency tree

```bash
# What pulls in openssl-sys? (the root of most conflicts)
cargo tree -i openssl-sys --target all --depth 3 2>&1

# What pulls in native-tls? (the usual path to openssl-sys on Linux)
cargo tree -i native-tls --target all --depth 2 2>&1

# What pulls in ring? (conflicts with aws-lc-rs assembly symbols)
cargo tree -i ring --target all --depth 2 2>&1

# What version of rustls is in use? (0.21 uses ring, 0.23+ supports aws-lc-rs)
cargo tree -i rustls --target all --depth 1 2>&1
```

Record which workspace crates and third-party crates pull in each crypto backend. This is the migration map.

### 1b. Check for symbol conflict risk

**Critical rule**: `openssl-sys` (vendored or static) and `rdkafka-sys` with `ssl-awslc` (bundled BoringSSL/AWS-LC) CANNOT coexist in the same link unit. They both export `SSL_new`, `SSL_CTX_ctrl`, `SSL_CIPHER_get_name`, etc. The linker will fail with `duplicate symbol` errors.

Similarly, `ring` and `aws-lc-fips-sys` share assembly symbol names (`bignum_add_p384`, `bignum_sqr_p521`, etc.) causing duplicate symbol errors when both are linked.

```bash
# Check if openssl-sys will be statically linked (vendored feature or static system lib)
cargo tree -i openssl-sys --target x86_64-unknown-linux-gnu -e features -f '{p} [{f}]' 2>&1 | grep openssl-sys

# Check if any crate bundles its own SSL (rdkafka-sys, aws-lc-sys, etc.)
cargo tree --target x86_64-unknown-linux-gnu 2>&1 | grep -E 'rdkafka-sys|aws-lc-sys|aws-lc-fips-sys'
```

**If both openssl-sys and a bundled SSL library are present**: you MUST remove one before the other can be added. Plan the migration order accordingly.

### 1c. Check feature unification

Cargo's feature unification means that if ANY crate in the workspace enables a feature, it's enabled for ALL crates that depend on that package. This is the #1 source of unexpected crypto conflicts.

```bash
# Check what features are actually resolved for a specific crate
cargo tree -p <crate> --target all -e features 2>&1 | head -40

# Check if native-tls or ssl-vendored is being activated by feature unification
cargo tree -p reqwest --target all -e features 2>&1 | grep -E 'native-tls|rustls|ssl'
```

**Common trap**: One crate enables `reqwest/native-tls` while another enables `reqwest/rustls-tls`. Feature unification activates BOTH, pulling in both openssl-sys and rustls. Use `default-features = false` on ALL reqwest dependencies and explicitly choose one TLS backend.

### 1d. Determine migration path for the target crate

For each crate being migrated, determine:

1. **Does it support rustls natively?** Check the crate's Cargo.toml features.
2. **Which rustls version?** rustls 0.21 uses ring (bad). rustls 0.23+ supports aws-lc-rs (good).
3. **Is it a fork?** If so, can you update the fork, or do you need upstream changes?
4. **What API changes are needed?** e.g., `Identity::from_pkcs12` (native-tls) → `Identity::from_pem` (rustls), different `ClientConfig` builders, etc.

---

## Phase 2: Migration

### 2a. Choose the right reqwest TLS feature

For reqwest-based migrations, use this decision tree:

| Situation | Feature to use |
|-----------|---------------|
| Normal dependency | `rustls-tls-webpki-roots-no-provider` + `default-features = false` |
| Needs system CA certs | `rustls-tls-native-roots-no-provider` + `default-features = false` |
| Needs client certs | Add `rustls-tls-webpki-roots-no-provider` + use `Identity::from_pem()` |

The `-no-provider` suffix is critical: it avoids bundling ring as a crypto provider. Instead, the process-wide `CryptoProvider` (aws-lc-rs) installed at startup is used.

**Always set `default-features = false`** on reqwest to prevent feature unification from re-enabling native-tls.

### 2b. Install the process-wide CryptoProvider

Any binary using rustls with `-no-provider` features needs a global CryptoProvider installed early in startup:

```rust
use rustls::crypto::aws_lc_rs;

fn main() {
    let _ = aws_lc_rs::default_provider().install_default();
    // ... rest of startup
}
```

In the Materialize codebase, use `mz_ore::crypto::fips_crypto_provider()` which handles this and returns a cached `Arc<CryptoProvider>` for explicit use.

For build scripts (`build.rs`) that use reqwest at build time:
```rust
fn main() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    // ... build script logic
}
```

### 2c. Migrate Cargo.toml dependencies

For each crate being migrated:

```toml
# BEFORE (native-tls)
reqwest = { version = "0.12", features = ["native-tls-vendored"] }

# AFTER (rustls, no provider bundled)
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls-webpki-roots-no-provider"] }
```

For crates with their own TLS features:
```toml
# BEFORE
mysql_async = { version = "0.36", default-features = false, features = ["minimal", "native-tls-tls"] }

# AFTER
mysql_async = { version = "0.36", default-features = false, features = ["minimal", "rustls-tls"] }
```

### 2d. Handle workspace patch overrides

If the crate is a fork referenced in workspace `Cargo.toml` `[patch]` section:

1. Update the fork's code to support the new crypto backend
2. Update the `[patch]` reference (branch/rev)
3. Run `cargo update -p <crate>` to refresh `Cargo.lock`

### 2e. Update deny.toml

If the migration adds or removes crypto crates from the dependency tree:

```bash
# Check if ring wrappers need updating
cargo deny check bans 2>&1

# Check if new licenses were introduced (e.g., webpki-roots uses CDLA-Permissive-2.0)
cargo deny check licenses 2>&1
```

Common additions needed:
- `ring` wrapper for crates that transitively pull it in via AWS SDK
- `rustls` and `rustls-webpki` as ring wrappers (they use ring as a fallback provider in some configurations)
- `CDLA-Permissive-2.0` license for `webpki-roots`

### 2f. Update API callsites

Common API changes when migrating from native-tls/openssl to rustls:

**Certificate identity (client certs):**
```rust
// BEFORE (native-tls)
let identity = Identity::from_pkcs12(&pkcs12_der, password)?;
let connector = TlsConnector::builder().identity(identity).build()?;

// AFTER (rustls)
let identity = Identity::from_pem(&pem_bundle)?;  // cert + key concatenated
// Or use reqwest's built-in:
let client = reqwest::Client::builder()
    .identity(Identity::from_pem(&pem_data)?)
    .build()?;
```

**TLS connector for postgres:**
```rust
// BEFORE (openssl)
use postgres_openssl::MakeTlsConnector;
let connector = MakeTlsConnector::new(ssl_connector);

// AFTER (rustls)
use tokio_postgres_rustls::MakeRustlsConnect;
let config = rustls::ClientConfig::builder_with_provider(provider)
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_root_certificates(root_store)
    .with_no_client_auth();
let connector = MakeRustlsConnect::new(config);
```

**Server TLS config:**
```rust
// BEFORE (openssl)
let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls())?;
builder.set_private_key_file(&key_path, SslFiletype::PEM)?;
builder.set_certificate_chain_file(&cert_path)?;

// AFTER (rustls)
let provider = mz_ore::crypto::fips_crypto_provider();
let config = rustls::ServerConfig::builder_with_provider(provider)
    .with_no_client_auth()
    .with_single_cert(certs, key)?;
```

---

## Phase 3: Validation

### 3a. Check for duplicate symbols (MOST IMPORTANT)

```bash
# Verify openssl-sys isn't still being pulled in unexpectedly
cargo tree -i openssl-sys --target all --depth 1 2>&1

# Verify ring isn't being pulled in unexpectedly
cargo tree -i ring --target all --depth 1 2>&1

# If openssl-sys is still present, check if it conflicts with any bundled SSL
cargo tree --target x86_64-unknown-linux-gnu 2>&1 | grep -E 'rdkafka-sys|aws-lc-sys'
```

If both openssl-sys and a bundled SSL library (rdkafka-sys with ssl-awslc, aws-lc-fips-sys) are in the tree, the build WILL fail in CI with duplicate symbol errors even if it compiles locally (local builds may use dynamic linking while CI uses static/cross-compilation).

### 3b. Local compilation checks

```bash
# Basic check
cargo check --all-targets 2>&1

# Check with foundationdb feature (used in CI builds)
cargo check -p mz-testdrive --features foundationdb 2>&1

# Check deny rules
cargo deny check bans 2>&1
cargo deny check licenses 2>&1
```

### 3c. Cross-compilation awareness

The CI builds cross-compile for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` using a sysroot with static OpenSSL. This means:

- `openssl-sys` will statically link OpenSSL from the sysroot (bundling .o files in the .rlib)
- Any bundled SSL library (BoringSSL from rdkafka, AWS-LC) will also have .o files in its .rlib
- Both sets of .o files go into the final binary → duplicate symbols

**This is why a change can compile locally but fail in CI.** Locally, you may have dynamic system OpenSSL. In CI, everything is static.

### 3d. Test the `--all-features` build

CI's `cargo-test-2` job uses `--all-features`. This can activate both `ssl-vendored` AND `ssl-awslc` features simultaneously via feature unification, causing conflicts that don't appear in normal builds.

```bash
# Simulate the all-features build
cargo check --all-features 2>&1 | head -50
```

### 3e. Verify CryptoProvider is installed

If you used `-no-provider` features, verify that every binary entrypoint installs a CryptoProvider before any TLS operation:

```bash
# Check for CryptoProvider installation in binary entrypoints
grep -rn 'install_default\|fips_crypto_provider' src/*/src/bin/ src/*/src/main.rs src/*/build.rs --include='*.rs' 2>&1
```

Tests that start servers (environmentd, balancerd) need the provider installed before the server creates any TLS connections. Missing providers cause `SIGABRT` / panic at runtime.

---

## Common Pitfalls

### 1. "It compiles locally but fails in CI"
CI cross-compiles with static linking. Check `cargo tree -i openssl-sys --target x86_64-unknown-linux-gnu` — the target matters.

### 2. Feature unification pulls in both TLS backends
One crate's `reqwest/native-tls` activates native-tls for ALL crates using reqwest. Always use `default-features = false`.

### 3. Old rustls (0.21) uses ring, not aws-lc-rs
Crates that support "rustls" may use rustls 0.21 which depends on ring. Check the actual version: `cargo tree -p <crate> 2>&1 | grep rustls`. Only rustls 0.23+ supports aws-lc-rs.

### 4. Build scripts need their own CryptoProvider
`build.rs` scripts run in a separate process. If they use reqwest (e.g., downloading assets), they need `install_default()` in the build script itself.

### 5. macOS CI is different from Linux CI
macOS CI runs on bare metal (no Docker). Different toolchains, different pre-installed packages. Always check both Linux and macOS clippy results.

### 6. cargo deny has two files to update
Both `deny.toml` (cargo deny) and `about.toml` (cargo about) must list any new licenses. Missing one will pass locally but fail the other check in CI.

### 7. Migration ordering matters
You cannot add a bundled SSL library (rdkafka ssl-awslc) while openssl-sys is still in the tree. Remove all openssl-sys paths FIRST, then add the new backend. The correct order is:
1. Migrate all `native-tls` → `rustls` (removes openssl-sys paths one by one)
2. Migrate all `ring` → `aws-lc-rs` (once openssl-sys is gone)
3. Switch bundled SSL (rdkafka ssl-vendored → ssl-awslc) last

### 8. Test with the CI builder image
When in doubt, test inside the CI builder container:
```bash
bin/ci-builder run stable bash
cargo check -p <crate> --target x86_64-unknown-linux-gnu
```
