#!/usr/bin/env bash
set -euo pipefail

# Setup script for SQL Server Availability Group (2-node, with read-only routing).
# Run from the repo root after: docker compose -f docker-compose.readonly.yml up -d

COMPOSE="docker compose -f docker-compose.readonly.yml"
SA_PWD='<YourStrong@Passw0rd>'
SQLCMD_ARGS="-U SA -P ${SA_PWD} -C -b"

primary_sql() {
  $COMPOSE exec -T mssql-primary /opt/mssql-tools18/bin/sqlcmd $SQLCMD_ARGS -Q "$1"
}

secondary_sql() {
  $COMPOSE exec -T mssql-secondary /opt/mssql-tools18/bin/sqlcmd $SQLCMD_ARGS -Q "$1"
}

echo "==> Waiting for primary..."
for i in $(seq 1 30); do
  if primary_sql "SELECT 1" >/dev/null 2>&1; then break; fi
  echo "    attempt $i..."
  sleep 2
done

echo "==> Waiting for secondary..."
for i in $(seq 1 30); do
  if secondary_sql "SELECT 1" >/dev/null 2>&1; then break; fi
  echo "    attempt $i..."
  sleep 2
done

# ── Primary: create master key, login, user, certificate ──
echo "==> Primary: creating certificate infrastructure..."
primary_sql "
USE master;
CREATE MASTER KEY ENCRYPTION BY PASSWORD = '${SA_PWD}';
CREATE LOGIN dbm_login WITH PASSWORD = '${SA_PWD}';
CREATE USER dbm_user FOR LOGIN dbm_login;

CREATE CERTIFICATE dbm_certificate
  WITH SUBJECT = 'dbm';

BACKUP CERTIFICATE dbm_certificate
  TO FILE = '/tmp/ag-certs/dbm_certificate.cer'
  WITH PRIVATE KEY (
    FILE = '/tmp/ag-certs/dbm_certificate.pvk',
    ENCRYPTION BY PASSWORD = '${SA_PWD}'
  );
"

# Give the shared volume a moment to sync
sleep 2

# ── Secondary: import certificate ──
echo "==> Secondary: importing certificate..."
secondary_sql "
USE master;
CREATE MASTER KEY ENCRYPTION BY PASSWORD = '${SA_PWD}';
CREATE LOGIN dbm_login WITH PASSWORD = '${SA_PWD}';
CREATE USER dbm_user FOR LOGIN dbm_login;

CREATE CERTIFICATE dbm_certificate
  AUTHORIZATION dbm_user
  FROM FILE = '/tmp/ag-certs/dbm_certificate.cer'
  WITH PRIVATE KEY (
    FILE = '/tmp/ag-certs/dbm_certificate.pvk',
    DECRYPTION BY PASSWORD = '${SA_PWD}'
  );
"

# ── Both: create HADR endpoint ──
echo "==> Primary: creating endpoint..."
primary_sql "
CREATE ENDPOINT Hadr_endpoint
  STATE = STARTED
  AS TCP (LISTENER_PORT = 5022, LISTENER_IP = ALL)
  FOR DATABASE_MIRRORING (
    ROLE = ALL,
    AUTHENTICATION = CERTIFICATE dbm_certificate,
    ENCRYPTION = REQUIRED ALGORITHM AES
  );
GRANT CONNECT ON ENDPOINT::Hadr_endpoint TO dbm_login;
"

echo "==> Secondary: creating endpoint..."
secondary_sql "
CREATE ENDPOINT Hadr_endpoint
  STATE = STARTED
  AS TCP (LISTENER_PORT = 5022, LISTENER_IP = ALL)
  FOR DATABASE_MIRRORING (
    ROLE = ALL,
    AUTHENTICATION = CERTIFICATE dbm_certificate,
    ENCRYPTION = REQUIRED ALGORITHM AES
  );
GRANT CONNECT ON ENDPOINT::Hadr_endpoint TO dbm_login;
"

# ── Primary: create the AG ──
echo "==> Primary: creating availability group..."
primary_sql "
CREATE AVAILABILITY GROUP ag_test
  WITH (CLUSTER_TYPE = NONE)
  FOR REPLICA ON
    N'mssql-primary' WITH (
      ENDPOINT_URL = N'TCP://mssql-primary:5022',
      AVAILABILITY_MODE = SYNCHRONOUS_COMMIT,
      FAILOVER_MODE = MANUAL,
      SEEDING_MODE = AUTOMATIC,
      SECONDARY_ROLE (ALLOW_CONNECTIONS = ALL)
    ),
    N'mssql-secondary' WITH (
      ENDPOINT_URL = N'TCP://mssql-secondary:5022',
      AVAILABILITY_MODE = SYNCHRONOUS_COMMIT,
      FAILOVER_MODE = MANUAL,
      SEEDING_MODE = AUTOMATIC,
      SECONDARY_ROLE (ALLOW_CONNECTIONS = ALL)
    );
"

# ── Secondary: join the AG ──
echo "==> Secondary: joining availability group..."
secondary_sql "
ALTER AVAILABILITY GROUP ag_test JOIN WITH (CLUSTER_TYPE = NONE);
ALTER AVAILABILITY GROUP ag_test GRANT CREATE ANY DATABASE;
"

# ── Primary: create test database and add to AG ──
echo "==> Primary: creating test database..."
primary_sql "
CREATE DATABASE ag_test_db;
"

primary_sql "
ALTER DATABASE ag_test_db SET RECOVERY FULL;
BACKUP DATABASE ag_test_db TO DISK = '/tmp/ag_test_db.bak';
ALTER AVAILABILITY GROUP ag_test ADD DATABASE ag_test_db;
"

# ── Wait for the database to appear on the secondary ──
echo "==> Waiting for database to sync to secondary..."
for i in $(seq 1 60); do
  SYNCED=$(secondary_sql "
    SELECT COUNT(*) FROM sys.dm_hadr_database_replica_states
    WHERE is_local = 1 AND is_primary_replica = 0 AND synchronization_state_desc = 'SYNCHRONIZED';
  " 2>/dev/null | tr -d '[:space:]' | grep -o '[0-9]*' | tail -1)

  if [ "${SYNCED:-0}" -ge 1 ]; then
    echo "    Database synchronized."
    break
  fi
  echo "    attempt $i (synced=$SYNCED)..."
  sleep 3
done

# ── Primary: insert test data ──
echo "==> Primary: inserting test data..."
primary_sql "
USE ag_test_db;
CREATE TABLE readonly_test (
  id INT PRIMARY KEY,
  name NVARCHAR(100) NOT NULL
);
INSERT INTO readonly_test (id, name) VALUES (1, 'alice'), (2, 'bob'), (3, 'charlie');
"

# Wait for data to replicate
sleep 3

# ── Configure read-only routing ──
# The routing URLs use host-mapped ports so the test client (on the host)
# can follow the redirect. SQL Server sends these URLs to the client as-is;
# it does not validate reachability from the server side.
echo "==> Configuring read-only routing..."

# Set routing URLs (sent to client as redirect targets).
# Each ALTER must modify a single option — SQL Server rejects combined options.
primary_sql "
ALTER AVAILABILITY GROUP ag_test
MODIFY REPLICA ON N'mssql-primary' WITH (
  SECONDARY_ROLE (READ_ONLY_ROUTING_URL = N'TCP://localhost:1501')
);
"

primary_sql "
ALTER AVAILABILITY GROUP ag_test
MODIFY REPLICA ON N'mssql-secondary' WITH (
  SECONDARY_ROLE (READ_ONLY_ROUTING_URL = N'TCP://localhost:1502')
);
"

# Primary only accepts READ_WRITE connections, forcing ReadOnly to be routed.
primary_sql "
ALTER AVAILABILITY GROUP ag_test
MODIFY REPLICA ON N'mssql-primary' WITH (
  PRIMARY_ROLE (ALLOW_CONNECTIONS = READ_WRITE)
);
"

primary_sql "
ALTER AVAILABILITY GROUP ag_test
MODIFY REPLICA ON N'mssql-primary' WITH (
  PRIMARY_ROLE (READ_ONLY_ROUTING_LIST = (N'mssql-secondary'))
);
"

# ── Add AG listener ──
# The listener shares port 1433 with SQL Server on the primary. With
# CLUSTER_TYPE = NONE on Linux, no cluster manager is needed — the listener
# metadata is enough for routing to work.
echo "==> Adding AG listener on port 1433..."
primary_sql "
ALTER AVAILABILITY GROUP ag_test
ADD LISTENER N'ag_listener' (
  WITH IP ((N'0.0.0.0', N'255.255.255.0')),
  PORT = 1433
);
"

LISTENER_COUNT=$(primary_sql "SELECT COUNT(*) FROM sys.availability_group_listeners;" 2>/dev/null | tr -d '[:space:]' | grep -o '[0-9]*' | tail -1)
if [ "${LISTENER_COUNT:-0}" -ge 1 ]; then
  echo "    Listener created successfully."
else
  echo "    WARNING: Listener was not created. Read-only routing tests may fail."
fi

echo "==> AG setup complete (with read-only routing)."
