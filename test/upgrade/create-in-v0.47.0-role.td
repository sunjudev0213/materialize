# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ postgres-execute connection=postgres://mz_system@${testdrive.materialize-internal-sql-addr}
CREATE ROLE superuser_login;
CREATE ROLE "space role";
CREATE ROLE joe;
CREATE ROLE group;
GRANT group TO joe;
