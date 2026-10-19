#!/usr/bin/env python3
# Modified by the hbench authors.
"""Run a NeMo Agent Toolkit workflow headless and print its answer as plain text.

`nat run` renders through Rich, which drops text when stdout is not a TTY, so the
workflow API is driven directly. With `--trajectory-output PATH` the workflow's
intermediate steps are also saved as an ATIF trajectory.

    nemo_agent_run_wrapper.py CONFIG INSTRUCTION... [--trajectory-output PATH]
"""

import asyncio
import json
import logging
import os
import sys
import traceback
from uuid import uuid4

LEVEL = os.environ.get("NVIDIA_NAT_LOG_LEVEL", "WARNING").upper()
logging.basicConfig(level=getattr(logging, LEVEL, logging.WARNING))
os.environ.setdefault("NAT_LOG_LEVEL", LEVEL)


def warn(what: str) -> None:
    print(f"[nemo-agent-wrapper] {what}:", file=sys.stderr)
    traceback.print_exc(file=sys.stderr)


def save_atif(raw_steps: list[dict], path: str) -> None:
    from nat.plugins.eval.utils.intermediate_step_adapter import IntermediateStepAdapter
    from nat.utils.atif_converter import IntermediateStepToATIFConverter

    steps = IntermediateStepAdapter().validate_intermediate_steps(raw_steps)
    doc = IntermediateStepToATIFConverter().convert(steps, session_id=str(uuid4()), agent_name="nemo-agent")
    with open(path, "w", encoding="utf-8") as out:
        json.dump(doc.to_json_dict(exclude_none=True), out, indent=2, ensure_ascii=False)


async def run(config_file: str, prompt: str, atif_path: str | None) -> None:
    from nat.builder.workflow_builder import WorkflowBuilder
    from nat.data_models.config import Config
    from nat.runtime.loader import PluginTypes, discover_and_register_plugins
    from nat.runtime.session import SessionManager
    from nat.utils.io.yaml_tools import yaml_load

    discover_and_register_plugins(PluginTypes.COMPONENT)
    config = Config(**yaml_load(config_file))

    async with WorkflowBuilder.from_config(config) as builder:
        sessions = await SessionManager.create(config=config, shared_builder=builder)
        async with sessions.session(user_id="hbench") as session, session.run(prompt) as runner:
            steps = None
            if atif_path:
                # Must subscribe before the run produces events, hence a task started up front.
                try:
                    from nat.builder.runtime_event_subscriber import pull_intermediate

                    steps = asyncio.ensure_future(pull_intermediate())
                except Exception:
                    warn("Failed to start trajectory collection")

            print(str(await runner.result()))

            if steps is not None:
                try:
                    save_atif(await steps, atif_path)
                except Exception:
                    warn("Failed to write trajectory")
        await sessions.shutdown()


def parse(argv: list[str]) -> tuple[str, str, str | None]:
    atif_path = None
    if "--trajectory-output" in argv:
        i = argv.index("--trajectory-output")
        if i + 1 >= len(argv):
            sys.exit("--trajectory-output requires a path argument")
        atif_path = argv[i + 1]
        argv = argv[:i] + argv[i + 2 :]
    if len(argv) < 2:
        sys.exit(f"Usage: {sys.argv[0]} <config_file> <instruction> [--trajectory-output <path>]")
    return argv[0], " ".join(argv[1:]), atif_path


if __name__ == "__main__":
    asyncio.run(run(*parse(sys.argv[1:])))
