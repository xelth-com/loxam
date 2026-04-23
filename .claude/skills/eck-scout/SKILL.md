---
name: eck-scout
description: Explores external repositories and generates directory trees for context.
whenToUse: Use this when you need to understand the architecture of a linked or external project.
arguments:
  - name: path
    description: Absolute or relative path to the external repository.
  - name: depth
    description: Depth level (0-9). 0 is tree-only, 5 is skeleton, 9 is full source. Default is 0.
    required: false
disable-model-invocation: false
---
# Scout Protocol
Execute cross-repository scans.
To run a scout, I will execute:
```bash
cd ${path} && eck-snapshot scout ${depth}
```
