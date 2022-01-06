# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

from materialize.mzcompose import Composition
from materialize.mzcompose.services import (
    Kafka,
    Materialized,
    SchemaRegistry,
    Testdrive,
    Zookeeper,
)

CONFLUENT_PLATFORM_VERSIONS = [
    "4.0.0",
    "5.0.0",
    "6.0.2",
    "6.1.2",
    "6.2.0",
    "latest",
]

SERVICES = [
    Materialized(),
    Testdrive(),
    Zookeeper(),
    Kafka(),
    SchemaRegistry(),
]


def workflow_kafka_matrix(c: Composition) -> None:
    for version in CONFLUENT_PLATFORM_VERSIONS:
        print(f"==> Testing Confluent Platform {version}")
        confluent_platform_SERVICES = [
            Zookeeper(tag=version),
            Kafka(tag=version),
            SchemaRegistry(tag=version),
        ]
        with c.with_services(confluent_platform_services):
            c.start_and_wait_for_tcp(
                services=["zookeeper", "kafka", "schema-registry", "materialized"]
            )
            c.wait_for_mz()
            c.run_service(
                service="testdrive-svc",
                command="kafka-matrix.td",
            )
            c.remove_services(
                services=["zookeeper", "kafka", "schema-registry", "materialized"],
                destroy_volumes=True,
            )
