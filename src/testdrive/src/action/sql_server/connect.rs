// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use anyhow::Context;
use tiberius::{Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use crate::action::{ControlFlow, State};
use crate::parser::BuiltinCommand;

pub async fn run_connect(
    mut cmd: BuiltinCommand,
    state: &mut State,
) -> Result<ControlFlow, anyhow::Error> {
    let name = cmd.args.string("name")?;
    cmd.args.done()?;

    let ado_string = cmd.input.join("\n");

    let config = Config::from_ado_string(&ado_string).context("parsing ADO string: {}")?;

    let tcp = TcpStream::connect(config.get_addr())
        .await
        .context("connecting to SQL Server: {}")?;

    tcp.set_nodelay(true)
        .context("setting nodelay socket option")?;

    // To be able to use Tokio's tcp, we're using the `compat_write` from
    // the `TokioAsyncWriteCompatExt` to get a stream compatible with the
    // traits from the `futures` crate.
    let client = Client::connect(config.clone(), tcp.compat_write())
        .await
        .context("connecting to SQL Server")?;

    state.sql_server_clients.insert(name.clone(), client);
    Ok(ControlFlow::Continue)
}
