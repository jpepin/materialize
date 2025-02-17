#!/usr/bin/env python3

# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

from dataclasses import dataclass
from datetime import datetime
from typing import Any


@dataclass
class BuildItemOutcomeBase:
    step_key: str
    build_number: int
    created_at: datetime
    duration_in_min: float | None
    passed: bool
    retry_count: int

    def formatted_date(self) -> str:
        return self.created_at.strftime("%Y-%m-%d %H:%M:%S %z")


@dataclass
class BuildStepOutcome(BuildItemOutcomeBase):
    """Outcome of an atomic build step. For sharded jobs, more than one build step exists for a job."""

    id: str
    web_url_to_job: str
    parallel_job_index: int | None
    exit_status: int | None

    def web_url_to_build(self) -> str:
        return self.web_url_to_job[: self.web_url_to_job.index("#")]


@dataclass
class BuildJobOutcome(BuildItemOutcomeBase):
    """Outcome, which aggregates multiple build steps in case of a sharded job."""

    ids: list[str]
    web_url_to_build: str
    # number of merged shards, 1 otherwise
    count_items: int


@dataclass
class BuildStepMatcher:
    step_key: str
    # will be ignored if not specified
    parallel_job_index: int | None

    def matches(self, job_step_key: str, job_parallel_index: int | None) -> bool:
        if self.step_key != job_step_key:
            return False

        if self.parallel_job_index is None:
            # not specified
            return True

        return self.parallel_job_index == job_parallel_index


def extract_build_step_outcomes(
    builds_data: list[Any],
    selected_build_steps: list[BuildStepMatcher],
) -> list[BuildStepOutcome]:
    result = []
    for build in builds_data:
        step_infos = _extract_build_step_data_from_build(build, selected_build_steps)
        result.extend(step_infos)

    return result


def _extract_build_step_data_from_build(
    build_data: Any, selected_build_steps: list[BuildStepMatcher]
) -> list[BuildStepOutcome]:
    collected_steps = []

    for job in build_data["jobs"]:
        if not job.get("step_key"):
            continue

        if not _shall_include_build_step(job, selected_build_steps):
            continue

        if job["state"] in ["canceled", "running"]:
            continue

        id = build_data["id"]
        build_number = build_data["number"]
        created_at = datetime.fromisoformat(job["created_at"])
        build_step_key = job["step_key"]
        parallel_job_index = job.get("parallel_group_index")

        if job.get("started_at") and job.get("finished_at"):
            started_at = datetime.fromisoformat(job["started_at"])
            finished_at = datetime.fromisoformat(job["finished_at"])
            duration_in_min = (finished_at - started_at).total_seconds() / 60
        else:
            duration_in_min = None

        job_passed = job["state"] == "passed"
        exit_status = job.get("exit_status")
        retry_count = job.get("retries_count") or 0

        assert (
            not job_passed or duration_in_min is not None
        ), "Duration must be available for passed step"

        step_data = BuildStepOutcome(
            id=id,
            step_key=build_step_key,
            parallel_job_index=parallel_job_index,
            build_number=build_number,
            created_at=created_at,
            duration_in_min=duration_in_min,
            passed=job_passed,
            exit_status=exit_status,
            retry_count=retry_count,
            web_url_to_job=f"{build_data['web_url']}#{job['id']}",
        )
        collected_steps.append(step_data)

    return collected_steps


def _shall_include_build_step(
    job: Any, selected_build_steps: list[BuildStepMatcher]
) -> bool:
    if len(selected_build_steps) == 0:
        return True

    job_step_key = job["step_key"]
    job_parallel_index = job.get("parallel_group_index")

    for build_step_matcher in selected_build_steps:
        if build_step_matcher.matches(job_step_key, job_parallel_index):
            return True

    return False


def step_outcomes_to_job_outcomes(
    step_infos: list[BuildStepOutcome],
) -> list[BuildJobOutcome]:
    """This merges sharded executions of the same build and step."""
    outcomes_by_build_and_step_key: dict[str, list[BuildStepOutcome]] = dict()

    for step_info in step_infos:
        build_and_step_key = f"{step_info.build_number}.{step_info.step_key}"
        outcomes_of_same_step = (
            outcomes_by_build_and_step_key.get(build_and_step_key) or []
        )
        outcomes_of_same_step.append(step_info)
        outcomes_by_build_and_step_key[build_and_step_key] = outcomes_of_same_step

    result = []

    for _, outcomes_of_same_step in outcomes_by_build_and_step_key.items():
        result.append(_step_outcomes_to_job_outcome(outcomes_of_same_step))

    return result


def _step_outcomes_to_job_outcome(
    outcomes_of_same_step: list[BuildStepOutcome],
) -> BuildJobOutcome:
    any_execution = outcomes_of_same_step[0]

    for outcome in outcomes_of_same_step:
        assert outcome.build_number == any_execution.build_number
        assert outcome.step_key == any_execution.step_key

    ids = [s.id for s in outcomes_of_same_step]
    min_created_at = min([s.created_at for s in outcomes_of_same_step])
    durations = [
        s.duration_in_min
        for s in outcomes_of_same_step
        if s.duration_in_min is not None
    ]
    sum_duration_in_min = sum(durations) if len(durations) > 0 else None
    all_passed = len([False for s in outcomes_of_same_step if not s.passed]) == 0
    max_retry_count = max([s.retry_count for s in outcomes_of_same_step])
    count_shards = len(outcomes_of_same_step)
    web_url_without_job_id = any_execution.web_url_to_build()

    return BuildJobOutcome(
        ids=ids,
        step_key=any_execution.step_key,
        build_number=any_execution.build_number,
        created_at=min_created_at,
        duration_in_min=sum_duration_in_min,
        passed=all_passed,
        retry_count=max_retry_count,
        web_url_to_build=web_url_without_job_id,
        count_items=count_shards,
    )
