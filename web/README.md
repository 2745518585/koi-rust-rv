# Web frontend

The Web UI will be a React + TypeScript + Vite application. It communicates with
the Rust backend through `/api/v1` JSON endpoints and a server-sent event stream.

Frontend scaffolding is intentionally deferred until the backend API DTOs are
defined, so generated TypeScript types remain aligned with the Rust API.
