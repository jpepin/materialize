#!/usr/bin/env python3

# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

import argparse

import pandas as pd

from materialize.buildkite_insights.buildkite_api.buildkite_config import MZ_PIPELINES
from materialize.buildkite_insights.buildkite_api.buildkite_constants import (
    BUILDKITE_BUILD_STATES,
)
from materialize.buildkite_insights.cache import builds_cache
from materialize.buildkite_insights.cache.cache_constants import (
    FETCH_MODE_CHOICES,
    FetchMode,
)
from materialize.buildkite_insights.step_durations.build_step import (
    BuildJobOutcome,
    BuildStepMatcher,
    extract_build_step_outcomes,
    step_outcomes_to_job_outcomes,
)

OUTPUT_TYPE_TXT = "txt"
OUTPUT_TYPE_TXT_SHORT = "txt-short"
OUTPUT_TYPE_CSV = "csv"


def print_data(
    job_outcomes: list[BuildJobOutcome],
    build_steps: list[BuildStepMatcher],
    output_type: str,
) -> None:
    if output_type == OUTPUT_TYPE_CSV:
        print("step_key,build_number,created_at,duration_in_min,passed,retry_count")

    for entry in job_outcomes:
        if output_type in [OUTPUT_TYPE_TXT, OUTPUT_TYPE_TXT_SHORT]:
            formatted_duration = (
                f"{entry.duration_in_min:.2f}"
                if entry.duration_in_min is not None
                else "None"
            )
            url = (
                ""
                if output_type == OUTPUT_TYPE_TXT_SHORT
                else f"{entry.web_url_to_build}, "
            )
            print(
                f"{entry.step_key}, #{entry.build_number}, {entry.formatted_date()}, {formatted_duration} min, {url}{'SUCCESS' if entry.passed else 'FAIL'}{' (RETRY)' if entry.retry_count > 0 else ''}"
            )
        elif output_type == OUTPUT_TYPE_CSV:
            print(
                f"{entry.step_key},{entry.build_number},{entry.created_at.isoformat()},{entry.duration_in_min},{1 if entry.passed else 0},{entry.retry_count}"
            )

    if output_type in [OUTPUT_TYPE_TXT, OUTPUT_TYPE_TXT_SHORT]:
        print_stats(job_outcomes, build_steps)


def print_stats(
    job_outcomes: list[BuildJobOutcome],
    build_matchers: list[BuildStepMatcher],
) -> None:
    job_filter_desc = f"jobs matching {build_matchers}"
    if len(job_outcomes) == 0:
        print(f"No data for {job_filter_desc}!")
        return

    dfs = pd.DataFrame(job_outcomes)
    dfs_with_success = dfs.loc[dfs["passed"]]

    number_of_builds = len(job_outcomes)
    number_of_builds_with_successful_step = len(dfs_with_success.index)
    success_prop = number_of_builds_with_successful_step / number_of_builds

    print()
    print(f"Statistics for {job_filter_desc}:")
    print(f"Number of builds: {number_of_builds}")
    print(
        f"Number of builds with job success: {number_of_builds_with_successful_step} ({100 * success_prop:.1f}%)"
    )
    print(
        f"Min duration with success: {dfs_with_success['duration_in_min'].min():.2f} min"
    )
    print(
        f"Max duration with success: {dfs_with_success['duration_in_min'].max():.2f} min"
    )
    print(
        f"Mean duration with success: {dfs_with_success['duration_in_min'].mean():.2f} min"
    )
    print(
        f"Median duration with success: {dfs_with_success['duration_in_min'].median():.2f} min"
    )


def main(
    pipeline_slug: str,
    build_steps: list[BuildStepMatcher],
    fetch_mode: FetchMode,
    max_fetches: int,
    branch: str | None,
    build_state: str | None,
    output_type: str,
) -> None:
    build_states = [build_state] if build_state is not None else None
    builds_data = builds_cache.get_or_query_builds(
        pipeline_slug, fetch_mode, max_fetches, branch, build_states
    )
    step_outcomes = extract_build_step_outcomes(
        builds_data=builds_data,
        selected_build_steps=build_steps,
    )
    job_outcomes = step_outcomes_to_job_outcomes(step_outcomes)
    print_data(job_outcomes, build_steps, output_type)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        prog="buildkite-step-durations",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )

    parser.add_argument("--pipeline", choices=MZ_PIPELINES, default="tests", type=str)
    parser.add_argument("--build-step-key", action="append", default=[], type=str)
    parser.add_argument(
        "--fetch",
        type=lambda mode: FetchMode[mode],
        choices=FETCH_MODE_CHOICES,
        default=FetchMode.AUTO,
        help="Whether to fetch fresh builds from Buildkite.",
    )
    parser.add_argument("--max-fetches", default=3, type=int)
    parser.add_argument(
        "--branch", default="main", type=str, help="Use '*' for all branches"
    )
    parser.add_argument(
        "--build-state",
        default=None,
        type=str,
        choices=BUILDKITE_BUILD_STATES,
    )
    parser.add_argument(
        "--output-type",
        choices=[OUTPUT_TYPE_TXT, OUTPUT_TYPE_TXT_SHORT, OUTPUT_TYPE_CSV],
        default=OUTPUT_TYPE_TXT,
        type=str,
    )
    args = parser.parse_args()

    main(
        args.pipeline,
        [
            BuildStepMatcher(build_step_key, None)
            for build_step_key in args.build_step_key
        ],
        args.fetch,
        args.max_fetches,
        args.branch if args.branch != "*" else None,
        args.build_state,
        args.output_type,
    )
