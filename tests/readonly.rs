use std::env;

use once_cell::sync::Lazy;
use tiberius::{error::Error, Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

static CONN_STR: Lazy<String> = Lazy::new(|| {
    env::var("TIBERIUS_TEST_CONNECTION_STRING").unwrap_or_else(|_| {
        "server=tcp:localhost,1433;IntegratedSecurity=true;TrustServerCertificate=true".to_owned()
    })
});

/// Connection string pointing at an AG secondary (port 1502).
/// Only set when the AG docker infrastructure is running.
fn readonly_conn_str() -> Option<String> {
    env::var("TIBERIUS_TEST_READONLY_CONNECTION_STRING").ok()
}

macro_rules! skip_if_no_ag {
    () => {
        if readonly_conn_str().is_none() {
            eprintln!("TIBERIUS_TEST_READONLY_CONNECTION_STRING not set — skipping AG test");
            return Ok(());
        }
    };
}

// ── Helpers ─────────────────────────────────────────────────────────────────

async fn log_hadr_status(client: &mut Client<tokio_util::compat::Compat<TcpStream>>, label: &str) {
    let result = client
        .simple_query("SELECT sys.fn_hadr_is_primary_replica(DB_NAME())")
        .await;
    match result {
        Ok(stream) => match stream.into_row().await {
            Ok(Some(row)) => {
                let val: Option<bool> = row.get(0);
                eprintln!("[{label}] fn_hadr_is_primary_replica(DB_NAME()) = {val:?}");
            }
            Ok(None) => eprintln!("[{label}] fn_hadr_is_primary_replica: no row returned"),
            Err(e) => eprintln!("[{label}] fn_hadr_is_primary_replica error: {e}"),
        },
        Err(e) => eprintln!("[{label}] fn_hadr_is_primary_replica query error: {e}"),
    }
}

async fn connect_with(
    conn_str: &str,
) -> anyhow::Result<Client<tokio_util::compat::Compat<TcpStream>>> {
    let config = Config::from_ado_string(conn_str)?;
    let tcp = TcpStream::connect(config.get_addr()).await?;
    tcp.set_nodelay(true)?;
    let client = Client::connect(config, tcp.compat_write()).await?;
    Ok(client)
}

async fn connect_readonly_ado() -> anyhow::Result<Client<tokio_util::compat::Compat<TcpStream>>> {
    let base = CONN_STR.as_str();
    let conn = if base.ends_with(';') {
        format!("{}ApplicationIntent=ReadOnly", base)
    } else {
        format!("{};ApplicationIntent=ReadOnly", base)
    };
    connect_with(&conn).await
}

async fn connect_readonly_api() -> anyhow::Result<Client<tokio_util::compat::Compat<TcpStream>>> {
    let mut config = Config::from_ado_string(&CONN_STR)?;
    config.readonly(true);
    let tcp = TcpStream::connect(config.get_addr()).await?;
    tcp.set_nodelay(true)?;
    let client = Client::connect(config, tcp.compat_write()).await?;
    Ok(client)
}

async fn connect_ag_secondary() -> anyhow::Result<Client<tokio_util::compat::Compat<TcpStream>>> {
    let conn_str =
        readonly_conn_str().expect("TIBERIUS_TEST_READONLY_CONNECTION_STRING must be set");
    connect_with(&conn_str).await
}

/// Try to connect with the given config. Returns the raw tiberius Result so
/// callers can inspect routing errors.
async fn try_connect(
    config: Config,
) -> tiberius::Result<Client<tokio_util::compat::Compat<TcpStream>>> {
    let tcp = TcpStream::connect(config.get_addr()).await?;
    tcp.set_nodelay(true)?;
    Client::connect(config, tcp.compat_write()).await
}

// ── Group 0: Baseline (non-readonly) for HADR comparison ────────────────────

#[tokio::test]
async fn baseline_non_readonly_hadr_status() -> anyhow::Result<()> {
    let mut client = connect_with(&CONN_STR).await?;
    log_hadr_status(&mut client, "baseline_non_readonly").await;
    let row = client.simple_query("SELECT 1").await?.into_row().await?;
    assert!(row.is_some());
    Ok(())
}

// ── Group 1: Standalone server tests (always run) ───────────────────────────

#[tokio::test]
async fn connect_with_readonly_ado_string() -> anyhow::Result<()> {
    let mut client = connect_readonly_ado().await?;
    log_hadr_status(&mut client, "readonly_ado_string").await;
    let row = client.simple_query("SELECT 1").await?.into_row().await?;
    assert!(row.is_some());
    Ok(())
}

#[tokio::test]
async fn connect_with_readonly_config_api() -> anyhow::Result<()> {
    let mut client = connect_readonly_api().await?;
    log_hadr_status(&mut client, "readonly_config_api").await;
    let row = client.simple_query("SELECT 1").await?.into_row().await?;
    assert!(row.is_some());
    Ok(())
}

#[tokio::test]
async fn readonly_allows_reads_on_standalone() -> anyhow::Result<()> {
    let mut client = connect_readonly_ado().await?;
    log_hadr_status(&mut client, "readonly_reads_standalone").await;
    let row = client
        .simple_query("SELECT @@VERSION")
        .await?
        .into_row()
        .await?;
    assert!(row.is_some());
    Ok(())
}

#[tokio::test]
async fn readonly_allows_writes_on_standalone() -> anyhow::Result<()> {
    // Standalone SQL Server ignores the ReadOnly hint — writes should succeed.
    let mut client = connect_readonly_ado().await?;
    log_hadr_status(&mut client, "readonly_writes_standalone").await;
    client
        .simple_query("CREATE TABLE #readonly_tmp (id INT); INSERT INTO #readonly_tmp VALUES (1)")
        .await?
        .into_results()
        .await?;
    let row = client
        .simple_query("SELECT id FROM #readonly_tmp")
        .await?
        .into_row()
        .await?;
    assert!(row.is_some());
    Ok(())
}

// ── Group 2: AG secondary tests (skip when env var unset) ───────────────────

#[tokio::test]
async fn ag_secondary_allows_select() -> anyhow::Result<()> {
    skip_if_no_ag!();
    let mut client = connect_ag_secondary().await?;
    log_hadr_status(&mut client, "ag_secondary_select").await;
    let row = client
        .simple_query("SELECT id, name FROM readonly_test")
        .await?
        .into_row()
        .await?;
    assert!(row.is_some());
    Ok(())
}

/// Helper: execute a write query on the AG secondary and assert it fails
/// with a read-only error. The error may surface from either `simple_query`
/// or `into_results`, depending on when SQL Server flushes the error token.
async fn assert_secondary_rejects(sql: &str) -> anyhow::Result<()> {
    let mut client = connect_ag_secondary().await?;
    log_hadr_status(&mut client, &format!("ag_secondary_rejects({sql})")).await;
    let result = match client.simple_query(sql).await {
        Ok(stream) => stream.into_results().await,
        Err(e) => Err(e),
    };
    assert!(
        result.is_err(),
        "Write query should fail on a read-only secondary: {sql}"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("read-only") || err_msg.contains("3906"),
        "Expected read-only error, got: {err_msg}"
    );
    Ok(())
}

#[tokio::test]
async fn ag_secondary_rejects_insert() -> anyhow::Result<()> {
    skip_if_no_ag!();
    assert_secondary_rejects("INSERT INTO readonly_test (id, name) VALUES (99, 'fail')").await
}

#[tokio::test]
async fn ag_secondary_rejects_update() -> anyhow::Result<()> {
    skip_if_no_ag!();
    assert_secondary_rejects("UPDATE readonly_test SET name = 'fail' WHERE id = 1").await
}

#[tokio::test]
async fn ag_secondary_rejects_delete() -> anyhow::Result<()> {
    skip_if_no_ag!();
    assert_secondary_rejects("DELETE FROM readonly_test WHERE id = 1").await
}

#[tokio::test]
async fn ag_secondary_rejects_ddl() -> anyhow::Result<()> {
    skip_if_no_ag!();
    assert_secondary_rejects("CREATE TABLE should_fail (id INT)").await
}

// ── Group 3: Read-only routing tests (skip when env var unset) ───────────────
//
// With an AG listener on port 1433 (shared with the primary) and
// PRIMARY_ROLE(ALLOW_CONNECTIONS = READ_WRITE) + READ_ONLY_ROUTING_LIST,
// the primary responds to ReadOnly connections with a TDS routing redirect
// (ENVCHANGE token type 20) pointing at the secondary's routing URL.
//
// Tiberius surfaces routing redirects as `Error::Routing { host, port }`
// rather than auto-reconnecting — the caller is responsible for opening a
// new connection to the advertised address. These tests verify that:
//   1. A ReadOnly connection triggers a routing redirect to the secondary.
//   2. Following the redirect yields a working read-only connection.
//   3. A non-ReadOnly connection is accepted normally (no redirect).

#[tokio::test]
async fn ag_readonly_routing_redirect() -> anyhow::Result<()> {
    skip_if_no_ag!();

    // Connect to the primary with ReadOnly + the AG database.
    // The AG listener should respond with a routing redirect to the secondary.
    let base = CONN_STR.as_str();
    let primary_readonly = if base.ends_with(';') {
        format!("{}ApplicationIntent=ReadOnly;database=ag_test_db", base)
    } else {
        format!("{};ApplicationIntent=ReadOnly;database=ag_test_db", base)
    };

    let config = Config::from_ado_string(&primary_readonly)?;
    eprintln!(
        "[ag_readonly_routing] connecting to primary at {} with ReadOnly",
        config.get_addr()
    );

    let result = try_connect(config.clone()).await;

    match result {
        Err(Error::Routing { host, port }) => {
            eprintln!("[ag_readonly_routing] got routing redirect to {host}:{port}");

            // Follow the redirect: build a new config from the original
            // connection string but retarget it at the routed address.
            let readonly_str =
                readonly_conn_str().expect("TIBERIUS_TEST_READONLY_CONNECTION_STRING must be set");
            let redirect_config = Config::from_ado_string(&readonly_str)?;
            let redirect_tcp = TcpStream::connect(redirect_config.get_addr()).await?;
            redirect_tcp.set_nodelay(true)?;

            let mut client = Client::connect(redirect_config, redirect_tcp.compat_write()).await?;
            log_hadr_status(&mut client, "ag_readonly_routing_redirected").await;

            // Verify we can read from the secondary.
            let row = client
                .simple_query("SELECT id, name FROM readonly_test")
                .await?
                .into_row()
                .await?;
            assert!(
                row.is_some(),
                "Should be able to read from routed secondary"
            );

            Ok(())
        }
        Err(other) => {
            anyhow::bail!("Expected routing redirect, got: {other}");
        }
        Ok(_) => {
            anyhow::bail!(
                "Expected ReadOnly connection to be routed, but it was accepted directly. \
                 Is PRIMARY_ROLE(ALLOW_CONNECTIONS = READ_WRITE) configured?"
            );
        }
    }
}

#[tokio::test]
async fn ag_readwrite_no_routing_redirect() -> anyhow::Result<()> {
    skip_if_no_ag!();

    // Connect to the primary WITHOUT ReadOnly — should succeed directly
    // with no routing redirect.
    let base = CONN_STR.as_str();
    let primary_rw = if base.ends_with(';') {
        format!("{}database=ag_test_db", base)
    } else {
        format!("{};database=ag_test_db", base)
    };

    let config = Config::from_ado_string(&primary_rw)?;
    eprintln!(
        "[ag_readwrite_no_redirect] connecting to primary at {} without ReadOnly",
        config.get_addr()
    );

    let mut client = try_connect(config).await?;
    log_hadr_status(&mut client, "ag_readwrite_no_redirect").await;

    let row = client
        .simple_query("SELECT id, name FROM readonly_test")
        .await?
        .into_row()
        .await?;
    assert!(row.is_some());

    Ok(())
}
