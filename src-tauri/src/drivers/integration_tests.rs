//! Tests against real servers. They only run with these environment variables:
//!
//! PostgreSQL: `LQ_PG_HOST`, `LQ_PG_PORT`, `LQ_PG_USER`, `LQ_PG_PASSWORD`, `LQ_PG_DB`
//! Oracle:     `LQ_ORA_HOST`, `LQ_ORA_PORT`, `LQ_ORA_SERVICE`, `LQ_ORA_USER`, `LQ_ORA_PASSWORD`
//!
//! `cargo test -- --ignored --test-threads=1`

use super::Session;
use crate::error::ErrorKind;
use crate::model::{ConnectionProfile, Engine, ExecRequest, OracleConnectType, SslMode};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn pg_profile() -> Option<(ConnectionProfile, Option<String>)> {
    let host = env("LQ_PG_HOST")?;
    Some((
        ConnectionProfile {
            name: "test".into(),
            engine: Engine::Postgres,
            host,
            port: env("LQ_PG_PORT")
                .and_then(|p| p.parse().ok())
                .unwrap_or(5432),
            user: env("LQ_PG_USER").unwrap_or_else(|| "postgres".into()),
            database: env("LQ_PG_DB").unwrap_or_else(|| "postgres".into()),
            ssl_mode: SslMode::Prefer,
            ..Default::default()
        },
        env("LQ_PG_PASSWORD"),
    ))
}

fn ora_profile() -> Option<(ConnectionProfile, Option<String>)> {
    let host = env("LQ_ORA_HOST")?;
    Some((
        ConnectionProfile {
            name: "test".into(),
            engine: Engine::Oracle,
            host,
            port: env("LQ_ORA_PORT")
                .and_then(|p| p.parse().ok())
                .unwrap_or(1521),
            user: env("LQ_ORA_USER")?,
            database: env("LQ_ORA_SERVICE").unwrap_or_else(|| "FREEPDB1".into()),
            oracle_connect_type: OracleConnectType::Service,
            ..Default::default()
        },
        env("LQ_ORA_PASSWORD"),
    ))
}

fn req(sql: &str, max_rows: usize) -> ExecRequest {
    ExecRequest {
        sql: sql.into(),
        max_rows,
    }
}

async fn one_row(s: &Session, sql: &str) -> Vec<Option<String>> {
    let r = s
        .execute(&req(sql, 10))
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows.into_iter().next().expect("one row")
}

#[tokio::test]
async fn refused_connections_fail_fast() {
    for engine in [
        Engine::Postgres,
        Engine::Oracle,
        Engine::Mysql,
        Engine::Sqlserver,
    ] {
        let profile = ConnectionProfile {
            name: "x".into(),
            engine,
            host: "127.0.0.1".into(),
            port: 1,
            user: "x".into(),
            database: "x".into(),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let err = match Session::open(&profile, Some("x")).await {
            Ok(_) => panic!("{engine:?}: connected to a closed port"),
            Err(e) => e,
        };
        assert_eq!(err.kind, ErrorKind::Connection, "{engine:?}: {err}");
        assert!(started.elapsed().as_secs() < 20, "{engine:?} took too long");
    }
}

#[tokio::test]
#[ignore]
async fn postgres_types_limits_and_transactions() {
    let Some((profile, pw)) = pg_profile() else {
        return;
    };
    let s = Session::open(&profile, pw.as_deref())
        .await
        .expect("connection");

    let row = one_row(
        &s,
        "SELECT 1::int2, 2::int4, 3::int8, 1.50::numeric(10,2), 'NaN'::numeric,
                12345678901234567890.123456789::numeric, 1.5::float4, 'Infinity'::float8,
                true, 'texto'::text, NULL::text, '2024-02-29'::date, 'infinity'::date,
                '2024-02-29 13:45:10.25'::timestamp, '1 year 2 mons 3 days 04:05:06'::interval,
                '{\"a\": [1, 2]}'::jsonb, 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid,
                '\\xdeadbeef'::bytea, ARRAY[1, NULL, 3], ARRAY['a b', 'c'],
                '[1,10)'::int4range, '192.168.0.1/24'::inet, '13:00:01'::time,
                ROW(1, 'x'), 12.34::money",
    )
    .await;
    let v: Vec<&str> = row.iter().map(|c| c.as_deref().unwrap_or("NULL")).collect();
    assert_eq!(
        v,
        [
            "1",
            "2",
            "3",
            "1.50",
            "NaN",
            "12345678901234567890.123456789",
            "1.5",
            "Infinity",
            "true",
            "texto",
            "NULL",
            "2024-02-29",
            "infinity",
            "2024-02-29 13:45:10.25",
            "1 year 2 mons 3 days 04:05:06",
            "{\"a\": [1, 2]}",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "\\xdeadbeef",
            "{1,NULL,3}",
            "{\"a b\",c}",
            "[1,10)",
            "192.168.0.1/24",
            "13:00:01",
            "(1,x)",
            "12.34",
        ]
    );

    // Row limit via portal.
    let r = s
        .execute(&req("SELECT g FROM generate_series(1, 1000) g", 100))
        .await
        .unwrap();
    assert_eq!(r.rows.len(), 100);
    assert!(r.truncated);
    let r = s
        .execute(&req("SELECT g FROM generate_series(1, 100) g", 100))
        .await
        .unwrap();
    assert!(!r.truncated);

    // Error position.
    let e = s
        .execute(&req("SELECT * FROM missing_table", 10))
        .await
        .unwrap_err();
    assert_eq!(e.code.as_deref(), Some("42P01"));
    assert_eq!(e.position, Some(14));

    // Server NOTICE.
    let r = s
        .execute(&req("DO $$ BEGIN RAISE NOTICE 'olá %', 42; END $$", 0))
        .await
        .unwrap();
    assert!(
        r.notices.iter().any(|n| n.contains("olá 42")),
        "{:?}",
        r.notices
    );

    // Manual transaction: rollback undoes, commit persists.
    s.execute(&req("CREATE TEMP TABLE lq_t (id int)", 0))
        .await
        .unwrap();
    s.set_autocommit(false).await.unwrap();
    let r = s
        .execute(&req("INSERT INTO lq_t VALUES (1), (2)", 0))
        .await
        .unwrap();
    assert_eq!(r.rows_affected, Some(2));
    assert!(r.in_transaction);
    let r = s
        .execute(&req("SELECT count(*) FROM lq_t", 0))
        .await
        .unwrap();
    assert_eq!(r.rows[0][0].as_deref(), Some("2"));
    assert!(!s.end_transaction(false).await.unwrap().in_transaction);
    let r = s
        .execute(&req("SELECT count(*) FROM lq_t", 0))
        .await
        .unwrap();
    assert_eq!(r.rows[0][0].as_deref(), Some("0"));
    s.execute(&req("INSERT INTO lq_t VALUES (3)", 0))
        .await
        .unwrap();
    s.end_transaction(true).await.unwrap();
    s.set_autocommit(true).await.unwrap();
    let r = s
        .execute(&req("SELECT count(*) FROM lq_t", 0))
        .await
        .unwrap();
    assert_eq!(r.rows[0][0].as_deref(), Some("1"));

    // An error inside a manual transaction rolls back and frees the session.
    s.set_autocommit(false).await.unwrap();
    s.execute(&req("INSERT INTO lq_t VALUES (4)", 0))
        .await
        .unwrap();
    assert!(s.execute(&req("SELECT 1/0", 0)).await.is_err());
    assert!(!s.tx_state().in_transaction);
    let r = s
        .execute(&req("SELECT count(*) FROM lq_t", 0))
        .await
        .unwrap();
    assert_eq!(r.rows[0][0].as_deref(), Some("1"));
    s.end_transaction(false).await.unwrap();

    // Metadata.
    let schemas = s.list_schemas().await.unwrap();
    assert!(schemas.iter().any(|x| x == "pg_catalog"));
    let objects = s.list_objects("pg_catalog").await.unwrap();
    assert!(objects
        .iter()
        .any(|o| o.name == "pg_class" && o.kind == "table"));
    let cols = s.list_columns("pg_catalog", "pg_class").await.unwrap();
    assert!(cols.iter().any(|c| c.name == "oid"));
}

#[tokio::test]
#[ignore]
async fn postgres_cancel_interrupts_query() {
    let Some((profile, pw)) = pg_profile() else {
        return;
    };
    let s = std::sync::Arc::new(
        Session::open(&profile, pw.as_deref())
            .await
            .expect("connection"),
    );
    let runner = {
        let s = s.clone();
        tokio::spawn(async move { s.execute(&req("SELECT pg_sleep(30)", 0)).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    s.cancel().await.unwrap();
    let err = runner.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled, "{err}");
}

#[tokio::test]
#[ignore]
async fn oracle_types_limits_and_output() {
    let Some((profile, pw)) = ora_profile() else {
        return;
    };
    let s = Session::open(&profile, pw.as_deref())
        .await
        .expect("connection");

    let row = one_row(
        &s,
        "select 1, 1.5, 12345678901234567890.123456789, cast(null as varchar2(10)), 'texto',
                date '2024-02-29', timestamp '2024-02-29 13:45:10.25',
                hextoraw('DEADBEEF'), interval '3' day, to_clob('clob'), binary_double_infinity
         from dual;",
    )
    .await;
    let v: Vec<&str> = row.iter().map(|c| c.as_deref().unwrap_or("NULL")).collect();
    assert_eq!(v[0], "1");
    assert_eq!(v[1], "1.5");
    assert_eq!(v[2], "12345678901234567890.123456789");
    assert_eq!(v[3], "NULL");
    assert_eq!(v[4], "texto");
    assert_eq!(v[5], "2024-02-29 00:00:00");
    assert!(v[6].starts_with("2024-02-29 13:45:10.25"), "{}", v[6]);
    assert_eq!(v[7], "DEADBEEF");
    assert_eq!(v[9], "clob");
    assert_eq!(v[10], "Inf");

    let r = s
        .execute(&req("select level from dual connect by level <= 1000", 100))
        .await
        .unwrap();
    assert_eq!(r.rows.len(), 100);
    assert!(r.truncated);

    let e = s
        .execute(&req("select * from missing_table", 10))
        .await
        .unwrap_err();
    assert_eq!(e.code.as_deref(), Some("ORA-00942"));

    let r = s
        .execute(&req(
            "begin dbms_output.put_line('olá'); dbms_output.put_line('mundo'); end;\n/",
            0,
        ))
        .await
        .unwrap();
    assert_eq!(r.notices, ["olá", "mundo"]);

    let schemas = s.list_schemas().await.unwrap();
    assert!(!schemas.is_empty());
}

/// ERP tables (Protheus `SA1010` and the like) have hundreds of columns, many
/// of them blank in every row, so the server sends rows with repeated-column
/// bit vectors and responses that span many packets. Servers before 26ai do
/// not mark the end of a response, which makes the driver parse those
/// responses packet by packet.
#[tokio::test]
#[ignore]
async fn oracle_wide_rows_with_repeated_values() {
    let Some((profile, pw)) = ora_profile() else {
        return;
    };
    let s = Session::open(&profile, pw.as_deref())
        .await
        .expect("connection");
    const COLS: usize = 200;
    const ROWS: usize = 3000;
    // Column c: blank (repeated in every row), distinct per row, runs of
    // seven equal values, or a number that alternates.
    let expr = |c: usize| match c % 4 {
        0 => "rpad(' ', 10)".to_string(),
        1 => format!("'V' || level || '_{c}'"),
        2 => "to_char(trunc(level / 7))".to_string(),
        _ => format!("mod(level, 2) * {c}"),
    };
    let expected = |row: usize, c: usize| -> String {
        let level = row + 1;
        match c % 4 {
            0 => " ".repeat(10),
            1 => format!("V{level}_{c}"),
            2 => (level / 7).to_string(),
            _ => ((level % 2) * c).to_string(),
        }
    };
    for (table, lob) in [("lq_wide", false), ("lq_wide_lob", true)] {
        let _ = s
            .execute(&req(&format!("drop table {table} purge"), 0))
            .await;
        let mut ddl: Vec<String> = (0..COLS)
            .map(|c| {
                let ty = if c % 4 == 3 { "number" } else { "varchar2(40)" };
                format!("c{c} {ty}")
            })
            .collect();
        ddl.push("d_e_l_e_t_ varchar2(1)".into());
        ddl.push("r_e_c_n_o_ number primary key".into());
        if lob {
            ddl.push("memo blob".into());
        }
        s.execute(&req(
            &format!("create table {table} ({})", ddl.join(", ")),
            0,
        ))
        .await
        .unwrap();
        let cols: Vec<String> = (0..COLS).map(|c| format!("c{c}")).collect();
        let vals: Vec<String> = (0..COLS).map(expr).collect();
        s.execute(&req(
            &format!(
                "insert into {table} ({}, d_e_l_e_t_, r_e_c_n_o_)
                 select {}, ' ', level from dual connect by level <= {ROWS}",
                cols.join(", "),
                vals.join(", ")
            ),
            0,
        ))
        .await
        .unwrap();
        s.execute(&req("commit", 0)).await.unwrap();

        let sql = format!("select * from {table} order by r_e_c_n_o_");
        // Twice: the second run reuses the cached statement without a describe.
        for (limit, attempt) in [(500, 1), (500, 2), (200, 1), (0, 1)] {
            let r = s
                .execute(&req(&sql, limit))
                .await
                .unwrap_or_else(|e| panic!("{table} limit {limit} run {attempt}: {e}"));
            let n = if limit == 0 { ROWS } else { limit };
            assert_eq!(r.rows.len(), n, "{table} limit {limit}");
            for (i, row) in r.rows.iter().enumerate() {
                assert_eq!(row[COLS + 1].as_deref(), Some((i + 1).to_string().as_str()));
                for c in [0, 1, 2, 3, 98, 99, 101, 102, 199] {
                    assert_eq!(
                        row[c].as_deref(),
                        Some(expected(i, c).as_str()),
                        "{table} row {} column c{c}",
                        i + 1
                    );
                }
            }
        }
        // The session is still usable afterwards.
        assert_eq!(
            one_row(&s, &format!("select count(*) from {table}")).await[0].as_deref(),
            Some(ROWS.to_string().as_str())
        );
        s.execute(&req(&format!("drop table {table} purge"), 0))
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn oracle_connects_through_tns_alias() {
    let Some((base, pw)) = ora_profile() else {
        return;
    };
    // A Latin-1 tnsnames.ora with a domain-qualified alias, as corporate
    // files often are.
    let dir = std::env::temp_dir().join(format!("lq-tns-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut file = b"# Produ\xe7\xe3o\n".to_vec();
    file.extend_from_slice(
        format!(
            "LQTEST.WORLD =\n  (DESCRIPTION =\n    (ADDRESS = (PROTOCOL = TCP)(HOST = {})(PORT = {}))\n    (CONNECT_DATA = (SERVICE_NAME = {}))\n  )\n",
            base.host, base.port, base.database
        )
        .as_bytes(),
    );
    std::fs::write(dir.join("tnsnames.ora"), file).unwrap();
    std::fs::write(dir.join("sqlnet.ora"), "NAMES.DEFAULT_DOMAIN = WORLD\n").unwrap();

    // Folder chosen in the profile.
    let profile = ConnectionProfile {
        oracle_connect_type: OracleConnectType::Tns,
        tns_alias: "lqtest".into(),
        config_dir: Some(dir.display().to_string()),
        ..base.clone()
    };
    let s = Session::open(&profile, pw.as_deref())
        .await
        .expect("connection via folder");
    assert_eq!(
        one_row(&s, "select 1 from dual").await[0].as_deref(),
        Some("1")
    );

    // Automatic discovery through TNS_ADMIN.
    std::env::set_var("TNS_ADMIN", &dir);
    let auto = ConnectionProfile {
        config_dir: None,
        ..profile.clone()
    };
    let s = Session::open(&auto, pw.as_deref())
        .await
        .expect("connection via TNS_ADMIN");
    assert_eq!(
        one_row(&s, "select 2 from dual").await[0].as_deref(),
        Some("2")
    );

    // Unknown alias names the files that were searched.
    let missing = ConnectionProfile {
        tns_alias: "nope".into(),
        ..auto
    };
    let err = match Session::open(&missing, pw.as_deref()).await {
        Ok(_) => panic!("unknown alias connected"),
        Err(e) => e,
    };
    assert!(err.message.contains("tnsnames.ora"), "{err}");
    std::env::remove_var("TNS_ADMIN");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[ignore]
async fn postgres_result_view_sql() {
    let Some((profile, pw)) = pg_profile() else {
        return;
    };
    let s = Session::open(&profile, pw.as_deref())
        .await
        .expect("connection");
    super::view_fixtures::check(&s, "postgres").await;
}

#[tokio::test]
#[ignore]
async fn oracle_result_view_sql() {
    let Some((profile, pw)) = ora_profile() else {
        return;
    };
    let s = Session::open(&profile, pw.as_deref())
        .await
        .expect("connection");
    super::view_fixtures::check(&s, "oracle").await;
}

#[tokio::test]
#[ignore]
async fn postgres_ddl_sql() {
    let Some((profile, pw)) = pg_profile() else {
        return;
    };
    let s = Session::open(&profile, pw.as_deref())
        .await
        .expect("connection");
    super::ddl_fixtures::check(&s, "postgres", "public").await;
}

#[tokio::test]
#[ignore]
async fn oracle_ddl_sql() {
    let Some((profile, pw)) = ora_profile() else {
        return;
    };
    let s = Session::open(&profile, pw.as_deref())
        .await
        .expect("connection");
    let schema = profile.user.to_uppercase();
    super::ddl_fixtures::check(&s, "oracle", &schema).await;
}
