# The agent-facing JSON contract

M3Flow is designed to be driven programmatically by coding agents. Every CLI
command accepts `--json`; stdout then carries exactly one JSON document.
Progress and diagnostics go to stderr.

## Success

Command-specific documents, stable keys:

- `workflow run --json` → the WorkflowRun record: `{id, name, version,
  status, inputs, params, outputs, error, created_at, started_at, ended_at,
  spec_hash, workdir, git}`. `outputs` maps output names to artifact ids;
  outputs of SKIPPED branches are absent. Exit code: 0 COMPLETED,
  2 FAILED, 3 CANCELLED.
- `run inspect --json` → `{run: {...}, tasks: [TaskRunRecord…]}` where each
  task record has `{id, node_id, task_name, task_version, provider, status,
  attempts, params, error, validation, engine, started_at, ended_at,
  cache_key}`.
- `run graph --json` → `{nodes: [{id, task, label, status}], edges: [{from,
  to}]}`.
- `artifact inspect --json` → `{id, type, schema_version, content_hash,
  files: {name: {path, store_relpath}}, metadata, data, producer}`.
- `artifact lineage --json` → recursive `{id, type, producer: {task_run…,
  inputs: [{input, artifact: …}]}}`.
- `artifact compatible --json` → `{compatible: bool, matching_inputs: [{input,
  declared_type}]}`.
- `task inspect/list/search`, `workflow list/inspect`, `schema list/show`,
  `provider list/diagnose`, `cache stats` emit their spec/status documents.

## Errors

Single document with `error_type` (snake_case variant) plus:

```json
{
  "error_type": "schema | type | workflow | task | provider | execution |
                 scientific_validation | artifact_compatibility | not_found |
                 io | internal",
  "category": "input_error | protocol_error | task_error | provider_error |
               execution_error | scientific_validation | compatibility_error |
               not_found | environment_error | internal",
  "recoverable": false,
  "message": "human-readable",
  "...": "variant-specific fields (details[], expected, received, step, ...)"
}
```

Branch on `category` (retry policy) and `error_type` (semantics). Provider
task failures add `raw_log` (engine log tail) for diagnosis.

## Provider protocol

`m3flow-provider/1` — see `docs/provider-protocol.md` for the full
request/response contract (`describe`, `validate`, `execute`, `diagnose`).

## Ids

`art_…` artifact · `tr_…` task run · `wr_…` workflow run. Any id-taking
command accepts the full id; run ids also accept unique prefixes nowhere —
always full ids (they are 8 hex chars).
