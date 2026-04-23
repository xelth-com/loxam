---
name: eck-fetch
description: Fetches specific source code files from an external repository using glob patterns.
whenToUse: Use this after running eck-scout when you need to see the exact implementation of specific files.
arguments:
  - name: path
    description: Path to the external repository.
  - name: glob
    description: Glob pattern matching the files (e.g., "**/api.ts").
disable-model-invocation: false
---
# Fetch Protocol
To fetch files, I will execute:
```bash
cd ${path} && eck-snapshot fetch "${glob}"
```
