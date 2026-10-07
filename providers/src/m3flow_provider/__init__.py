"""Shared m3flow-provider/1 protocol runtime (docs/provider-protocol.md).

A provider is an executable `m3flow-<name>` with subcommands:
  describe [config.json]
            -> provider/task/engine descriptor JSON; with the optional
               engine-config file, the engine probe describes the engine
               that `execute` would use under that config
  validate  -> cheap request validation (no execution)
  execute   -> run one task; success or structured error JSON on stdout
  diagnose  -> describe + environment checks

All output is a single JSON document on stdout. Logs go to stderr or files.
"""

from __future__ import annotations

import inspect
import json
import math
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


# accepted unit symbols -> factor to the canonical unit (mirrors
# crates/core/src/units.rs; canonical: K, bar, fs, angstrom, g/cm3, ...)
_UNIT_FACTORS = {
    "temperature": {"K": 1.0},
    "pressure": {"bar": 1.0, "atm": 1.01325, "Pa": 1e-5, "kPa": 1e-2,
                 "MPa": 10.0, "GPa": 1e4, "psi": 0.0689476},
    "time": {"fs": 1.0, "ps": 1e3, "ns": 1e6, "us": 1e9, "s": 1e15},
    "length": {"angstrom": 1.0, "A": 1.0, "nm": 10.0},
    "density": {"g/cm3": 1.0, "kg/m3": 1e-3, "g/ml": 1.0},
    "area": {"angstrom2": 1.0, "A2": 1.0, "nm2": 100.0},
}


def quantity_value(value, dimension, name="quantity"):
    """Canonical float of a quantity given as {value, unit}, "4 nm" or a
    bare number (already canonical). None stays None; an unknown unit is an
    input error rather than a silently misread number."""
    if value is None:
        return None
    if isinstance(value, bool):
        raise ProviderFailure("input_invalid", "input_error",
                              f"{name}: expected a {dimension} quantity")
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, dict):
        number, unit = value.get("value"), value.get("unit")
    elif isinstance(value, str):
        text = value.strip()
        i = 0
        while i < len(text) and (text[i].isdigit() or text[i] in "+-.eE"):
            i += 1
        number, unit = text[:i], text[i:].strip()
    else:
        raise ProviderFailure("input_invalid", "input_error",
                              f"{name}: cannot read {value!r} as a {dimension} quantity")
    try:
        number = float(number)
    except (TypeError, ValueError):
        raise ProviderFailure("input_invalid", "input_error",
                              f"{name}: invalid number in {value!r}")
    table = _UNIT_FACTORS[dimension]
    if not unit:
        return number
    if unit not in table:
        raise ProviderFailure(
            "input_invalid", "input_error",
            f"{name}: unknown {dimension} unit '{unit}' (accepted: {', '.join(table)})")
    return number * table[unit]


def quantity(params, name, default=None):
    """Canonical {value, unit} quantity parameter -> (value, unit)."""
    q = params.get(name)
    if q is None:
        return default
    return q["value"], q["unit"]


def find_nonfinite(value, path="$"):
    """JSON path of the first NaN/Infinity float in `value`, or None.

    Standard JSON has no NaN/Infinity; the runtime would reject the whole
    response. Providers must report missing statistics explicitly instead.
    """
    if isinstance(value, float):
        return None if math.isfinite(value) else path
    if isinstance(value, dict):
        for k, v in value.items():
            hit = find_nonfinite(v, f"{path}.{k}")
            if hit:
                return hit
    elif isinstance(value, (list, tuple)):
        for i, v in enumerate(value):
            hit = find_nonfinite(v, f"{path}[{i}]")
            if hit:
                return hit
    return None


def dumps(doc, **kw):
    """Strict JSON (no NaN/Infinity tokens)."""
    return json.dumps(doc, allow_nan=False, **kw)


def _finite_or_none(value):
    """Error documents only: replace NaN/Infinity by null so a diagnostic
    can always be emitted."""
    if isinstance(value, float) and not math.isfinite(value):
        return None
    if isinstance(value, dict):
        return {k: _finite_or_none(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_finite_or_none(v) for v in value]
    return value


class Provider:
    def __init__(self, name, version, engine, tasks, checks=None,
                 scheduling_config_keys=()):
        """
        name: provider name ("autopoly")
        version: provider version string
        engine: callable([request]) -> {"name": ..., "version": ...}; every
                field joins cache keys. Callables taking one argument receive
                {"config": <engine config>} so they can probe the configured
                engine rather than a PATH default.
        tasks: {task_name: callable(request) -> result dict}
        checks: optional callable -> list of {name, ok, detail} diagnostics
        scheduling_config_keys: engine-config keys that only change *how*
                a job is parallelized (never results); the runtime leaves
                them out of cache keys.
        """
        self.name = name
        self.version = version
        self._engine = engine
        self.tasks = tasks
        self._checks = checks
        self.scheduling_config_keys = sorted(scheduling_config_keys)

    def _call_engine(self, config=None):
        try:
            takes_arg = bool(inspect.signature(self._engine).parameters)
        except (TypeError, ValueError):
            takes_arg = False
        if takes_arg:
            return self._engine({"config": config or {}})
        return self._engine()

    # ---------------------------------------------------------- subcommands

    def describe(self, config_path=None):
        config = {}
        if config_path:
            with open(config_path) as f:
                config = json.load(f) or {}
        try:
            engine = self._call_engine(config)
        except Exception as e:  # engine probe must not kill describe
            engine = {"name": "unknown", "version": f"unavailable: {e}"}
        return {
            "protocol": PROTOCOL,
            "provider": {"name": self.name, "version": self.version},
            "engine": engine,
            "scheduling_config_keys": self.scheduling_config_keys,
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
        result["engine"] = result.get("engine") or self._safe_engine(req)
        bad = find_nonfinite(result)
        if bad:
            raise ProviderFailure(
                "non_finite_value", "scientific_validation",
                f"task '{task}' produced a non-finite number at {bad}; "
                "statistics without enough data must be reported as null "
                "with an explicit status, never NaN/Infinity",
                details={"path": bad})
        return result

    def diagnose(self, request_path=None):
        out = self.describe()
        out["checks"] = self._checks() if self._checks else []
        return out

    def _safe_engine(self, req=None):
        try:
            return self._call_engine((req or {}).get("config"))
        except Exception:
            return {"name": "unknown", "version": "unknown"}

    # ---------------------------------------------------------------- entry

    def cli(self, argv=None):
        argv = list(sys.argv[1:] if argv is None else argv)
        if not argv:
            print(dumps(_finite_or_none({
                "status": "error",
                "error": {"error_type": "usage", "category": "input_error",
                          "recoverable": False,
                          "message": "usage: m3flow-<name> describe|validate|execute|diagnose [request.json]"},
            })))
            return 2
        cmd, rest = argv[0], argv[1:]
        try:
            if cmd == "describe":
                doc = self.describe(rest[0] if rest else None)
            elif cmd == "validate":
                doc = self.validate(rest[0])
            elif cmd == "execute":
                doc = self.execute(rest[0])
            elif cmd == "diagnose":
                doc = self.diagnose(rest[0] if rest else None)
            else:
                raise ProviderFailure(
                    "usage", "input_error", f"unknown subcommand '{cmd}'")
            print(dumps(doc, indent=1))
            return 0
        except ProviderFailure as e:
            print(dumps(_finite_or_none({
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
            }), indent=1))
            return 1
        except Exception as e:  # unexpected: report as engine crash
            print(dumps(_finite_or_none({
                "status": "error",
                "error": {
                    "error_type": "engine_crash",
                    "category": "provider_error",
                    "recoverable": False,
                    "provider": self.name,
                    "message": f"{type(e).__name__}: {e}",
                    "raw_log": traceback.format_exc()[-4000:],
                },
            }), indent=1))
            return 1
