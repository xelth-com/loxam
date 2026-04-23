# Runtime State

**This is a CLI tool.** There are no persistent background services, ports, or daemons.

- **Server:** N/A — runs as a one-shot process per invocation
- **Services:** N/A — no background services
- **Ports:** N/A — no network listeners
- **Environment:** Requires a Rust toolchain (`rustup`) for build; release binary is self-contained
- **State:** Stateless — reads input file, writes output file, exits
