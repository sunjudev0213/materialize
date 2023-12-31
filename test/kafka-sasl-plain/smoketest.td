# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.



$ set schema={
    "name": "row",
    "type": "record",
    "fields": [
      {"name": "a", "type": "long"}
    ]
  }

$ kafka-create-topic topic=data

$ kafka-ingest format=avro topic=data schema=${schema} timestamp=1
{"a": 1}

> CREATE SECRET sasl_password AS 'sekurity'

# Ensure that connectors work with SSL basic_auth
> CREATE CONNECTION kafka_sasl TO KAFKA (
    BROKER 'kafka:9092',
    SASL MECHANISMS = 'PLAIN',
    SASL USERNAME = 'materialize',
    SASL PASSWORD = SECRET sasl_password,
    SSL CERTIFICATE AUTHORITY = '${arg.ca}'
  );

> CREATE CONNECTION csr_sasl
  FOR CONFLUENT SCHEMA REGISTRY
    URL '${testdrive.schema-registry-url}',
    SSL CERTIFICATE AUTHORITY = '${arg.ca}'

> CREATE SOURCE data
  FROM KAFKA CONNECTION kafka_sasl (TOPIC 'testdrive-data-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_sasl

> SELECT * FROM data
a
---
1

$ kafka-ingest format=avro topic=data schema=${schema} timestamp=2
{"a": 2}

> SELECT * FROM data
a
---
1
2

> CREATE SINK data_snk
  FROM data
  INTO KAFKA CONNECTION kafka_sasl (TOPIC 'testdrive-sink-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_sasl
  ENVELOPE DEBEZIUM

$ kafka-verify-data format=avro sink=materialize.public.data_snk sort-messages=true
{"before": null, "after": {"row": {"a": 1}}}
{"before": null, "after": {"row": {"a": 2}}}

# Ensure that connectors do not require the certificate authority
# This ensures that the error is not that the CA was required, but simply that
# not providing it prohibits connecting.
! CREATE CONNECTION kafka_sasl_no_ca TO KAFKA (
    BROKER 'kafka:9092',
    SASL MECHANISMS = 'PLAIN',
    SASL USERNAME = 'materialize',
    SASL PASSWORD = SECRET sasl_password
  );
contains:Meta data fetch error: BrokerTransportFailure (Local: Broker transport failure)
