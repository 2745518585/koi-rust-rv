# Database migrations

Keep schema changes as ordered SQL files named `NNNN_description.sql`.

The first real migration will create groups, messages, alerts, tasks, task events,
tool calls, approvals, servers, and usage records. Migrations must remain
append-only after they are applied to a shared database.
