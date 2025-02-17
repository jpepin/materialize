#!/usr/bin/env python3

# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

BUILDKITE_BUILD_STATES = [
    "running",
    "scheduled",
    "passed",
    "failing",
    "failed",
    "blocked",
    "canceled",
    "canceling",
    "skipped",
    "not_run",
    "finished",
]

BUILDKITE_FAILED_BUILD_STATES = [
    "failing",
    "failed",
]

BUILDKITE_COMPLETED_BUILD_STATES = [
    "passed",
    "failed",
    "canceled",
    "skipped",
    "not_run",
    "finished",
]
