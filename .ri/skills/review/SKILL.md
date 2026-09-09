---
name: review
description: Review changes in this Rust harness for correctness, regressions, and missing tests.
---

Review the user's requested changes, or the current uncommitted Git diff if no scope is given.

- Read the repository instructions and inspect the diff before drawing conclusions.
- Trace affected conversation, provider, persistence, and tool-execution paths.
- Prioritize concrete bugs, security issues, and regressions over style preferences.
- Check error recovery, cancellation, output limits, and preservation of tool ordering where relevant.
- Run focused tests through `nix develop -c cargo test` when useful. Do not use live provider credentials without the user's request.
- Report actionable findings with file paths, line numbers, and an explanation of the impact. State explicitly if you found no issues.
- Do not modify files unless the user asks for fixes.
