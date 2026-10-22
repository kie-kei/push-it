# Logging capability (wasi:logging)

The guest imports `wasi:logging/logging@0.1.0-draft` (vendored in
`wit/logging/`):

```wit
enum level { trace, debug, info, warn, error, critical }
log: func(level: level, context: string, message: string);
```

`log(level, context, message)` appends one line to the same per-app log
file that captures component stdout/stderr (`$PUSHIT_DATA/logs/<app>.log`,
surfaced by `pushit logs` and `/api/apps/<app>/logs`):

```
<ts> [INFO] [hello] hello from wasi:logging
```

Unlike stdout piping, this carries a structured severity + context tag.
No capability grant is required — logging is always available to a
deployed component.
