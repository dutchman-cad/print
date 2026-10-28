#!/usr/bin/env python3
"""Validate release YAML syntax and the draft-only safeguards (requires PyYAML)."""

from pathlib import Path
import re
import subprocess
import yaml


def check() -> None:
    # BaseLoader keeps GitHub's `on` key as text rather than a YAML 1.1 boolean.
    workflow = yaml.load(Path(".github/workflows/release.yml").read_text(), Loader=yaml.BaseLoader)
    assert set(workflow["on"]) == {"push", "workflow_dispatch"}
    assert workflow["on"]["push"]["tags"] == ["v*"]
    assert workflow["permissions"] == {"contents": "read"}
    jobs = workflow["jobs"]
    writes = []
    creates = []
    for name, job in jobs.items():
        if job.get("permissions", {}).get("contents") == "write":
            writes.append(name)
        for step in job["steps"]:
            if "uses" in step:
                assert re.fullmatch(r"[^@]+@[a-f0-9]{40}", step["uses"]), step["uses"]
            run = step.get("run", "")
            if run:
                subprocess.run(["bash", "-n"], input=run, text=True, check=True)
            assert "gh release edit" not in run
            if "gh release create" in run:
                creates.append(name)
                assert all(flag in run for flag in ("--draft", "--latest=false", "--verify-tag"))
    assert writes == creates == ["draft-release"]
    draft = jobs["draft-release"]
    assert set(draft["needs"]) == {"metadata", "build", "macos"}
    assert draft["if"] == "startsWith(github.ref, 'refs/tags/v')"
    print("Release YAML and draft safeguards passed.")


if __name__ == "__main__":
    check()
