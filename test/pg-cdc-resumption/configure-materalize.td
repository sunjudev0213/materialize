# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

> DROP SECRET IF EXISTS pgpass CASCADE;
> DROP CONNECTION IF EXISTS pg CASCADE;

> CREATE SECRET pgpass AS 'materialize'
> CREATE CONNECTION pg TO POSTGRES (
    HOST toxiproxy,
    DATABASE postgres,
    USER materialize,
    PASSWORD SECRET pgpass
  );

> DROP SOURCE IF EXISTS mz_source CASCADE;

> CREATE SOURCE mz_source
  FROM POSTGRES
  CONNECTION pg
  (PUBLICATION 'mz_source')
  FOR ALL TABLES;
