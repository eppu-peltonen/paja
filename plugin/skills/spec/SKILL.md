---
name: spec
description: Turn the current conversation into a spec for Paja, a GitHub issue labelled paja with a title, what to build and acceptance criteria, shown to the person to approve before it is filed. Use when the person wants Paja to build something discussed here.
---

# Spec for Paja

Use only the `paja` MCP tools to file it; don't run shell commands.

1. From the conversation, write the issue:
   - **Title**: verb first, says what changes, under about 70 characters.
   - **Body** with these sections:
     - `## What to build`: the behaviour wanted and why, in plain sentences. Name the files or parts of Paja involved when the conversation did.
     - `## Acceptance criteria`: a checklist a reviewer can verify, including the tests that should exist.
     - `## Out of scope`, when the conversation ruled something out.
   - Keep it to one concern; Paja lands one small change per issue. If the conversation holds several, propose one issue each.
   - The repository may be public: leave out secrets, private links, hostnames and personal details.
2. Show the title and body to the person exactly as they will be filed, and ask them to approve or edit. Don't file anything until they approve.
3. On approval call `work_file` with `title` and `body`. It creates the issue with the `paja` label.
4. Give the issue link and say it will be queued on Paja's next sync.
