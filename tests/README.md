# Integration Test Configurations

## Quick Start

```bash
# Start databases (all or pick one)
docker compose -f tests/docker-compose.yml up -d mysql
docker compose -f tests/docker-compose.yml up -d oracle
docker compose -f tests/docker-compose.yml up -d gaussdb

# Wait for Oracle (can take ~30s on first start)
docker logs -f hepta-oracle-test

# Create MySQL test user
docker exec hepta-mysql-test mysql -u root -ptestpass -e "
  CREATE USER IF NOT EXISTS 'mcp'@'%' IDENTIFIED WITH caching_sha2_password BY 'testpass';
  GRANT ALL PRIVILEGES ON *.* TO 'mcp'@'%';
  FLUSH PRIVILEGES;
  CREATE DATABASE IF NOT EXISTS testdb;
  GRANT ALL PRIVILEGES ON testdb.* TO 'mcp'@'%';
"
```

## Config Files

| File | Backend | Connection Name |
|------|---------|-----------------|
| `docker-mysql.toml` | MySQL only | `local` |
| `docker-oracle.toml` | Oracle only | `oracle` |
| `docker-gaussdb.toml` | GaussDB only | `gaussdb` |
| `docker-all.toml` | MySQL + Oracle + GaussDB | `mysql` (default), `oracle`, `gaussdb` |

## Regression Tests

Four test suites under `dbcli/tests/`:

```bash
# MySQL (requires Docker MySQL container)
HEPTA_DBCLI_TEST_URL=mysql://mcp:testpass@127.0.0.1:3306/testdb \
  cargo test --features integration --test regress_mysql

# Oracle (requires Docker Oracle container)
POLARDB_ORACLE_TEST_URL=oracle://system:testpass@127.0.0.1:1521/FREEPDB1 \
  cargo test --features "oracle,integration" --test regress_oracle

# GaussDB (requires Docker GaussDB container)
GAUSSDB_TEST_URL="host=127.0.0.1 port=5432 user=gaussdb password=testpass@123 dbname=testdb" \
  cargo test --features "gaussdb,integration" --test regress_gaussdb

# DuckDB (embedded — no Docker, no env var; uses tempfile fixtures)
cargo test --features "duckdb,integration" --test regress_duckdb
```

Each suite covers: database_info, list_tables, table_columns, table_indexes,
execute_query, add_limit, build_explain, read_only_prefixes, query_error.

## Cleanup

```bash
docker compose -f tests/docker-compose.yml down -v
```
