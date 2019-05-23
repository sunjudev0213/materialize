#!/usr/bin/env bash

# Copyright 2019 Materialize, Inc. All rights reserved.
#
# This file is part of Materialize. Materialize may not be used or
# distributed without the express permission of Materialize, Inc.
#
# slt.sh — runs sqllogictest in CI.

# TODO(jamii) We can't currently run test/select4.test and test/select5.test
# because of https://github.com/MaterializeInc/materialize/issues/43.
cargo run --release --bin=sqllogictest -- \
    sqllogictest/test/select1.test \
    sqllogictest/test/select2.test \
    sqllogictest/test/select3.test \
    sqllogictest/test/evidence \
    sqllogictest/test/index \
    sqllogictest/test/random \
    -vv \
