# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

ARG BUILDKITE_BUILD_NUMBER

FROM materialize/materialized:v0.1.0 AS golden010
FROM materialize/materialized:v0.6.0 AS golden060
FROM materialize/materialized:v0.7.1 AS golden071
MZFROM materialized AS edge
MZFROM testdrive AS testdrive
MZFROM ubuntu-base

RUN apt-get update && apt-get install -y curl postgresql-client wait-for-it && apt-get clean

COPY --from=golden010 /usr/local/bin/materialized /usr/local/bin/materialized-golden010
COPY --from=golden060 /usr/local/bin/materialized /usr/local/bin/materialized-golden060
COPY --from=golden071 /usr/local/bin/materialized /usr/local/bin/materialized-golden071
COPY --from=edge /usr/local/bin/materialized /usr/local/bin/materialized-edge
COPY --from=testdrive /usr/local/bin/testdrive /usr/local/bin/testdrive
COPY catcompatck /usr/local/bin/catcompatck

ENTRYPOINT ["catcompatck"]
