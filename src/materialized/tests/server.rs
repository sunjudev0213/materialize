// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Integration tests for Materialize server.

use std::collections::HashMap;
use std::error::Error;

use reqwest::{blocking::Client, StatusCode, Url};
use tempfile::NamedTempFile;

pub mod util;

#[test]
fn test_persistence() -> Result<(), Box<dyn Error>> {
    ore::test::init_logging();

    let data_dir = tempfile::tempdir()?;
    let config = util::Config::default().data_directory(data_dir.path());

    let source_file = NamedTempFile::new()?;

    {
        let server = util::start_server(config.clone())?;
        let mut client = server.connect(postgres::NoTls)?;
        client.batch_execute(&format!(
            "CREATE SOURCE src FROM FILE '{}' FORMAT BYTES",
            source_file.path().display()
        ))?;
        client.batch_execute("CREATE VIEW constant AS SELECT 1")?;
        client.batch_execute(
            "CREATE VIEW logging_derived AS SELECT * FROM mz_catalog.mz_arrangement_sizes",
        )?;
        client.batch_execute(
            "CREATE MATERIALIZED VIEW mat AS SELECT 'a', data, 'c' AS c, data FROM src",
        )?;
        client.batch_execute("CREATE DATABASE d")?;
        client.batch_execute("CREATE SCHEMA d.s")?;
        client.batch_execute("CREATE VIEW d.s.v AS SELECT 1")?;
    }

    {
        let server = util::start_server(config.clone())?;
        let mut client = server.connect(postgres::NoTls)?;
        assert_eq!(
            client
                .query("SHOW VIEWS", &[])?
                .into_iter()
                .map(|row| row.get(0))
                .collect::<Vec<String>>(),
            &["constant", "logging_derived", "mat"]
        );
        assert_eq!(
            client
                .query("SHOW INDEXES FROM mat", &[])?
                .into_iter()
                .map(|row| (row.get("Column_name"), row.get("Seq_in_index")))
                .collect::<Vec<(String, i64)>>(),
            &[
                ("?column?".into(), 1),
                ("data".into(), 2),
                ("c".into(), 3),
                ("data".into(), 4),
            ],
        );
        assert_eq!(
            client
                .query("SHOW VIEWS FROM d.s", &[])?
                .into_iter()
                .map(|row| row.get(0))
                .collect::<Vec<String>>(),
            &["v"]
        );

        // Test that catalog recovery correctly populates `mz_catalog_names`.
        assert_eq!(
            client
                .query("SELECT global_id FROM mz_catalog_names ORDER BY 1", &[])?
                .into_iter()
                .map(|row| row.get(0))
                .collect::<Vec<String>>(),
            vec![
                "s2000", "s2001", "s2002", "s2003", "s2004", "s2005", "s2006", "s2007", "s2008",
                "s2009", "s2010", "s2011", "s2012", "s2013", "s2014", "s2015", "s2016", "s2017",
                "s2018", "s2019", "s2020", "s2021", "s2022", "s2023", "s2024", "s2025", "s2026",
                "s2027", "s3001", "s3002", "s3003", "s3004", "s3005", "s3006", "s3007", "s3008",
                "s3009", "s3010", "s3011", "s3012", "s3013", "s3014", "s3015", "s3016", "s3017",
                "s3018", "s3019", "s3020", "s3021", "s3022", "s3023", "s3024", "s3025", "s3026",
                "s3027", "s3028", "s3029", "s3030", "s3031", "s3032", "s3033", "s3034", "s3035",
                "s3036", "s3037", "s3038", "s4000", "s4001", "s4002", "s4003", "s4004", "s4005",
                "s4006", "s4007", "s4008", "s4009", "s4010", "s4011", "s4012", "s4013", "s4014",
                "s4015", "s4016", "s4017", "s4018", "s4019", "s4020", "s4021", "s4022", "s4023",
                "s4024", "u1", "u2", "u3", "u4", "u5", "u6"
            ]
        );
    }

    {
        let config = config.logging_granularity(None);
        match util::start_server(config) {
            Ok(_) => panic!("server unexpectedly booted with corrupted catalog"),
            Err(e) => assert_eq!(
                e.to_string(),
                "catalog item 'materialize.public.logging_derived' depends on system logging, \
                 but logging is disabled"
            ),
        }
    }

    Ok(())
}

// Ensures that once a node is started with `--experimental`, it requires
// `--experimental` on reboot.
#[test]
fn test_experimental_mode_reboot() -> Result<(), Box<dyn Error>> {
    let data_dir = tempfile::tempdir()?;
    let config = util::Config::default().data_directory(data_dir.path());

    {
        let _ = util::start_server(config.clone().experimental_mode())?;
    }

    {
        match util::start_server(config.clone()) {
            Ok(_) => panic!("unexpected success"),
            Err(e) => {
                if !e
                    .to_string()
                    .contains("Materialize previously started with --experimental")
                {
                    return Err(e);
                }
            }
        }
    }

    {
        let _ = util::start_server(config.experimental_mode())?;
    }

    Ok(())
}

// Ensures that only new nodes can start in experimental mode.
#[test]
fn test_experimental_mode_on_init_or_never() -> Result<(), Box<dyn Error>> {
    let data_dir = tempfile::tempdir()?;
    let config = util::Config::default().data_directory(data_dir.path());

    {
        let _ = util::start_server(config.clone())?;
    }

    {
        match util::start_server(config.experimental_mode()) {
            Ok(_) => panic!("unexpected success"),
            Err(e) => {
                if !e
                    .to_string()
                    .contains("Experimental mode is only available on new nodes")
                {
                    return Err(e);
                }
            }
        }
    }

    Ok(())
}

// Test the /sql POST endpoint of the HTTP server.
#[test]
fn test_http_sql() -> Result<(), Box<dyn Error>> {
    let server = util::start_server(util::Config::default())?;
    let url = Url::parse(&format!("http://{}/sql", server.inner.local_addr()))?;
    let mut params = HashMap::new();

    struct TestCase {
        query: &'static str,
        status: StatusCode,
        body: &'static str,
    }

    let tests = vec![
        // Regular query works.
        TestCase {
            query: "select 1+2 as col",
            status: StatusCode::OK,
            body: r#"{"rows":[[3]],"col_names":["col"]}"#,
        },
        // Only one query at a time.
        TestCase {
            query: "select 1; select 2",
            status: StatusCode::BAD_REQUEST,
            body: r#"expected exactly 1 statement"#,
        },
        // CREATEs should not work.
        TestCase {
            query: "create view v as select 1",
            status: StatusCode::BAD_REQUEST,
            body: r#"unsupported plan"#,
        },
    ];

    for tc in tests {
        params.insert("sql", tc.query);
        let res = Client::new().post(url.clone()).form(&params).send()?;
        assert_eq!(res.status(), tc.status);
        assert_eq!(res.text()?, tc.body);
    }

    Ok(())
}
