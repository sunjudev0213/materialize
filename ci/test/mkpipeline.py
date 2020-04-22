# Copyright Materialize, Inc. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

from collections import OrderedDict
from materialize import mzbuild
from materialize import spawn
from pathlib import Path
from tempfile import TemporaryFile
from typing import Any, List, Set, Sequence
import os
import subprocess
import sys
import yaml


def main() -> int:
    with open(Path(__file__).parent / "pipeline.template.yml") as f:
        pipeline = yaml.safe_load(f)

    if os.environ["BUILDKITE_BRANCH"] != "master" and not os.environ["BUILDKITE_TAG"]:
        print("--- Trimming unchanged steps from pipeline")
        trim_pipeline(pipeline)

    f = TemporaryFile()
    yaml.dump(pipeline, f, encoding="utf-8")  # type: ignore
    f.seek(0)
    spawn.runv(["buildkite-agent", "pipeline", "upload"], stdin=f)

    return 0


class PipelineStep:
    def __init__(self, id: str):
        self.id = id
        self.manual_inputs: Set[str] = set()
        self.image_dependencies: Set[mzbuild.ResolvedImage] = set()
        self.step_dependencies: Set[str] = set()

    def inputs(self) -> Set[str]:
        inputs = set()
        inputs.update(self.manual_inputs)
        for image in self.image_dependencies:
            inputs.update(image.inputs(transitive=True))
        return inputs


def trim_pipeline(pipeline: Any) -> None:
    """Trim pipeline steps whose inputs have not changed in this branch.

    Steps are assigned inputs in two ways:

      1. An explicit glob in the `inputs` key.
      2. An implicit dependency on any number of mzbuild images via the
         mzcompose plugin. Any steps which use the mzcompose plugin will
         have inputs autodiscovered based on the images used in that
         mzcompose configuration.

    A step is trimmed if a) none of its inputs have changed, and b) there are
    no other untrimmed steps that depend on it.
    """
    repo = mzbuild.Repository(Path("."))
    images = repo.resolve_dependencies(image for image in repo)

    steps = OrderedDict()
    for config in pipeline["steps"]:
        step = PipelineStep(config["id"])
        if "inputs" in config:
            for inp in config["inputs"]:
                if inp.startswith("#"):
                    step.image_dependencies.add(images[inp[1:]])
                else:
                    step.manual_inputs.add(inp)
        if "depends_on" in config:
            d = config["depends_on"]
            if isinstance(d, str):
                step.step_dependencies.add(d)
            elif isinstance(d, list):
                step.step_dependencies.update(d)
            else:
                raise ValueError(f"unexpected non-str non-list for depends_on: {d}")
        if "plugins" in config:
            for plugin in config["plugins"]:
                for name, plugin_config in plugin.items():
                    if name == "./ci/plugins/mzcompose":
                        step.image_dependencies.update(
                            find_compose_images(images, plugin_config["config"])
                        )
        steps[step.id] = step

    # Make sure we have an up to date view of master.
    spawn.runv(["git", "fetch", "origin", "master"])

    # Print out a summary of all changes.
    os.environ["GIT_PAGER"] = ""
    spawn.runv(["git", "diff", "--stat", "origin/master..."])

    # Find all the steps whose inputs have changed with respect to master.
    # We delegate this hard work to Git.
    changed = set()
    for step in steps.values():
        inputs = step.inputs()
        if not inputs:
            # No inputs means there is no way this step can be considered
            # changed, but `git diff` with no pathspecs means "diff everything",
            # not "diff nothing", so explicitly skip.
            continue
        diff = subprocess.run(
            ["git", "diff", "--no-patch", "--quiet", "origin/master...", "--", *inputs,]
        )
        if diff.returncode != 0:
            changed.add(step.id)

    # Then collect all changed steps, and all the steps that those changed steps
    # depend on.
    needed = set()

    def visit(step: PipelineStep) -> None:
        if step.id not in needed:
            needed.add(step.id)
            for d in step.step_dependencies:
                visit(steps[d])

    for step_id in changed:
        visit(steps[step_id])

    # Print decisions, for debugging.
    for step in steps.values():
        print(f'{"✓" if step.id in needed else "✗"} {step.id}')
        if step.step_dependencies:
            print("    wait:", " ".join(step.step_dependencies))
        if step.manual_inputs:
            print("    globs:", " ".join(step.manual_inputs))
        if step.image_dependencies:
            print(
                "    images:", " ".join(image.name for image in step.image_dependencies)
            )

    # Restrict the pipeline to the needed steps.
    pipeline["steps"] = [step for step in pipeline["steps"] if step["id"] in needed]


def find_compose_images(
    images: mzbuild.DependencySet, path: Path
) -> Set[mzbuild.ResolvedImage]:
    """Extract the images that an mzcompose.yml configuration depends upon."""
    out = set()
    with open(path) as f:
        compose = yaml.safe_load(f)
    for config in compose["services"].values():
        if "mzbuild" in config:
            image_name = config["mzbuild"]
            out.add(images[image_name])
    return out


if __name__ == "__main__":
    sys.exit(main())
