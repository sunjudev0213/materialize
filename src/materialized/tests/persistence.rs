// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

use std::error::Error;

pub mod util;

#[test]
fn test_persistence() -> Result<(), Box<dyn Error>> {
    ore::log::init();

    let data_directory = tempfile::tempdir()?;
    let config = util::Config::default()
        .data_directory(data_directory.path().to_owned())
        .bootstrap_sql(
            "CREATE VIEW bootstrap1 AS SELECT 1;
             CREATE VIEW bootstrap2 AS SELECT * FROM bootstrap1;",
        );

    {
        let (_server, mut client) = util::start_server(config.clone())?;
        // TODO(benesch): when file sources land, use them here. Creating a
        // populated Kafka source here is too annoying.
        client.batch_execute("CREATE VIEW constant AS SELECT 1")?;
        client.batch_execute(
            "CREATE VIEW logging_derived AS SELECT * FROM mz_catalog.mz_arrangement_sizes",
        )?;
        client.batch_execute("CREATE DATABASE d")?;
        client.batch_execute("CREATE SCHEMA d.s")?;
        client.batch_execute("CREATE VIEW d.s.v AS SELECT 1")?;
    }

    {
        let (_server, mut client) = util::start_server(config)?;
        assert_eq!(
            client
                .query("SHOW VIEWS", &[])?
                .into_iter()
                .map(|row| row.get(0))
                .collect::<Vec<String>>(),
            &["bootstrap1", "bootstrap2", "constant", "logging_derived"]
        );
        assert_eq!(
            client
                .query("SHOW VIEWS FROM d.s", &[])?
                .into_iter()
                .map(|row| row.get(0))
                .collect::<Vec<String>>(),
            &["v"]
        );
    }

    Ok(())
}
