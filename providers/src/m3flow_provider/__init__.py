"""Shared m3flow-provider/1 protocol runtime (docs/provider-protocol.md).

A provider is an executable `m3flow-<name>` with subcommands:
  describe  -> provider/task/engine descriptor JSON
  validate  -> cheap request validation (no execution)
  execute   -> run one task; success or structured error JSON on stdout
  diagnose  -> describe + environment checks

All output is a single JSON document on stdout. Logs go to stderr or files.
"""

from __future__ import annotations

import json
import os
import sys
import traceback
from pathlib import Path

PROTOCOL = "m3flow-provider/1"


class ProviderFailure(Exception):
    """Structured, protocol-level failure."""

    def __init__(self, error_type, category, message, recoverable=False,
                 details=None, raw_log=None):
        super().__init__(message)
        self.error_type = error_type
        self.category = category
        self.message = message
        self.recoverable = recoverable
        self.details = details
        self.raw_log = raw_log


def artifact(artifact_type, files, metadata=None, data=None):
    """A StagedArtifact: files are workdir-relative paths."""
    return {
        "type": artifact_type,
        "files": files,
        "metadata": metadata or {},
        "data": data,
    }


def verdict(name, passed, detail=None):
    return {"name": name, "passed": bool(passed), "detail": detail}


def read_request(path):
    with open(path) as f:
        req = json.load(f)
    if req.get("protocol") != PROTOCOL:
        raise ProviderFailure(
            "protocol_error", "protocol_error",
            f"request protocol {req.get('protocol')!r} != {PROTOCOL!r}")
    return req


def input_files(req, name):
    """Absolute file paths of an input artifact (or list for many-inputs)."""
    inp = req["inputs"].get(name)
    if inp is None:
        raise ProviderFailure(
            "input_invalid", "input_error", f"missing input '{name}'")
    return inp


def quantity(params, name, default=None):
    """Canonical {value, unit} quantity parameter -> (value, unit)."""
    q = params.get(name)
    if q is None:
        return default
    return q["value"], q["unit"]


class Provider:
    def __init__(self, name, version, engine, tasks, checks=None):
        """
        name: provider name ("autopoly")
        version: provider version string
        engine: callable -> {"name": ..., "version": ...} (for cache keys)
        tasks: {task_name: callable(request) -> result dict}
        checks: optional callable -> list of {name, ok, detail} diagnostics
        """
        self.name = name
        self.version = version
        self._engine = engine
        self.tasks = tasks
        self._checks = checks

    # ---------------------------------------------------------- subcommands

    def describe(self):
        try:
            engine = self._engine()
        except Exception as e:  # engine probe must not kill describe
            engine = {"name": "unknown", "version": f"unavailable: {e}"}
        return {
            "protocol": PROTOCOL,
            "provider": {"name": self.name, "version": self.version},
            "engine": engine,
            "tasks": sorted(self.tasks.keys()),
        }

    def validate(self, request_path):
        req = read_request(request_path)
        task = req["task"]["name"]
        if task not in self.tasks:
            raise ProviderFailure(
                "input_invalid", "input_error",
                f"provider '{self.name}' does not implement task '{task}'",
                details={"implemented": sorted(self.tasks)})
        return {"valid": True, "task": task}

    def execute(self, request_path):
        req = read_request(request_path)
        task = req["task"]["name"]
        handler = self.tasks.get(task)
        if handler is None:
            raise ProviderFailure(
                "input_invalid", "input_error",
                f"provider '{self.name}' does not implement task '{task}'")
        workdir = Path(req["workdir"])
        workdir.mkdir(parents=True, exist_ok=True)
        os.chdir(workdir)
        result = handler(req)
        result.setdefault("status", "success")
        result.setdefault("outputs", {})
        result.setdefault("validation", [])
        result["engine"] = result.get("engine") or self._safe_engine()
        return result

    def diagnose(self, request_path=None):
        out = self.describe()
        out["checks"] = self._checks() if self._checks else []
        return out

    def _safe_engine(self):
        try:
            return self._engine()
        except Exception:
            return {"name": "unknown", "version": "unknown"}

    # ---------------------------------------------------------------- entry

    def cli(self, argv=None):
        argv = list(sys.argv[1:] if argv is None else argv)
        if not argv:
            print(json.dumps({
                "status": "error",
                "error": {"error_type": "usage", "category": "input_error",
                          "recoverable": False,
                          "message": "usage: m3flow-<name> describe|validate|execute|diagnose [request.json]"},
            }))
            return 2
        cmd, rest = argv[0], argv[1:]
        try:
            if cmd == "describe":
                doc = self.describe()
            elif cmd == "validate":
                doc = self.validate(rest[0])
            elif cmd == "execute":
                doc = self.execute(rest[0])
            elif cmd == "diagnose":
                doc = self.diagnose(rest[0] if rest else None)
            else:
                raise ProviderFailure(
                    "usage", "input_error", f"unknown subcommand '{cmd}'")
            print(json.dumps(doc, indent=1))
            return 0
        except ProviderFailure as e:
            print(json.dumps({
                "status": "error",
                "error": {
                    "error_type": e.error_type,
                    "category": e.category,
                    "recoverable": e.recoverable,
                    "provider": self.name,
                    "message": e.message,
                    "details": e.details,
                    "raw_log": e.raw_log,
                },
            }, indent=1))
            return 1
        except Exception as e:  # unexpected: report as engine crash
            print(json.dumps({
                "status": "error",
                "error": {
                    "error_type": "engine_crash",
                    "category": "provider_error",
                    "recoverable": False,
                    "provider": self.name,
                    "message": f"{type(e).__name__}: {e}",
                    "raw_log": traceback.format_exc()[-4000:],
                },
            }, indent=1))
            return 1
