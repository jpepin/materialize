# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

from pathlib import Path

from materialize import ci_util
from materialize.mzcompose.composition import Composition, WorkflowArgumentParser
from materialize.mzcompose.services.kafka import Kafka
from materialize.mzcompose.services.localstack import Localstack
from materialize.mzcompose.services.materialized import Materialized
from materialize.mzcompose.services.redpanda import Redpanda
from materialize.mzcompose.services.schema_registry import SchemaRegistry
from materialize.mzcompose.services.testdrive import Testdrive
from materialize.mzcompose.services.zookeeper import Zookeeper

SERVICES = [
    Zookeeper(),
    Kafka(),
    SchemaRegistry(),
    Redpanda(),
    Localstack(),
    Materialized(),
    Testdrive(),
]


def workflow_default(c: Composition, parser: WorkflowArgumentParser) -> None:
    """Run testdrive."""
    parser.add_argument(
        "--redpanda",
        action="store_true",
        help="run against Redpanda instead of the Confluent Platform",
    )
    parser.add_argument(
        "--aws-region",
        help="run against the specified AWS region instead of localstack",
    )
    parser.add_argument(
        "--kafka-default-partitions",
        type=int,
        metavar="N",
        help="set the default number of kafka partitions per topic",
    )
    parser.add_argument(
        "--default-size",
        type=int,
        default=Materialized.Size.DEFAULT_SIZE,
        help="Use SIZE 'N-N' for replicas and SIZE 'N' for sources",
    )

    parser.add_argument("--replicas", type=int, default=1, help="use multiple replicas")

    parser.add_argument(
        "--default-timeout",
        type=str,
        help="set the default timeout for Testdrive",
    )

    parser.add_argument(
        "files",
        nargs="*",
        default=["*.td"],
        help="run against the specified files",
    )
    (args, passthrough_args) = parser.parse_known_args()

    dependencies = ["materialized"]
    if args.redpanda:
        dependencies += ["redpanda"]
    else:
        dependencies += ["zookeeper", "kafka", "schema-registry"]

    if args.aws_region is None:
        dependencies += ["localstack"]

    testdrive = Testdrive(
        forward_buildkite_shard=True,
        kafka_default_partitions=args.kafka_default_partitions,
        aws_region=args.aws_region,
        validate_catalog_store=True,
        default_timeout=args.default_timeout,
        volumes_extra=["mzdata:/mzdata"],
    )

    materialized = Materialized(
        default_size=args.default_size,
    )

    with c.override(testdrive, materialized):
        c.up(*dependencies)

        c.sql(
            "ALTER SYSTEM SET max_clusters = 50;",
            port=6877,
            user="mz_system",
        )

        non_default_testdrive_vars = []

        if args.replicas > 1:
            c.sql("DROP CLUSTER quickstart CASCADE", user="mz_system", port=6877)
            # Make sure a replica named 'r1' always exists
            replica_names = [
                "r1" if replica_id == 0 else f"replica{replica_id}"
                for replica_id in range(0, args.replicas)
            ]
            replica_string = ",".join(
                f"{replica_name} (SIZE '{materialized.default_replica_size}')"
                for replica_name in replica_names
            )
            c.sql(
                f"CREATE CLUSTER quickstart REPLICAS ({replica_string})",
                user="mz_system",
                port=6877,
            )

            # Note that any command that outputs SHOW CLUSTERS will have output
            # that depends on the number of replicas testdrive has. This means
            # it might be easier to skip certain tests if the number of replicas
            # is > 1.
            c.sql(
                f"""
                CREATE CLUSTER testdrive_single_replica_cluster SIZE = '{materialized.default_replica_size}';
                GRANT ALL PRIVILEGES ON CLUSTER testdrive_single_replica_cluster TO materialize;
                """,
                user="mz_system",
                port=6877,
            )

            non_default_testdrive_vars.append(f"--var=replicas={args.replicas}")
            non_default_testdrive_vars.append(
                "--var=single-replica-cluster=testdrive_single_replica_cluster"
            )

        if args.default_size != 1:
            non_default_testdrive_vars.append(
                f"--var=default-replica-size={materialized.default_replica_size}"
            )
            non_default_testdrive_vars.append(
                f"--var=default-storage-size={materialized.default_storage_size}"
            )

        junit_report = ci_util.junit_report_filename(c.name)

        try:
            junit_report = ci_util.junit_report_filename(c.name)
            print(f"Passing through arguments to testdrive {passthrough_args}\n")
            # do not set default args, they should be set in the td file using set-arg-default to easen the execution
            # without mzcompose
            for file in args.files:
                c.run_testdrive_files(
                    f"--junit-report={junit_report}",
                    *non_default_testdrive_vars,
                    *passthrough_args,
                    file,
                )
                c.sanity_restart_mz()
        finally:
            ci_util.upload_junit_report(
                "testdrive", Path(__file__).parent / junit_report
            )
