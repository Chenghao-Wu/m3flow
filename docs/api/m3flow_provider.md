# `m3flow_provider`

Shared protocol runtime implementing
[`m3flow-provider/1`](../provider-protocol.md). Every provider process is an
executable `m3flow-<name>` with `describe` / `validate` / `execute` /
`diagnose` subcommands emitting a single JSON document on stdout.

::: m3flow_provider
