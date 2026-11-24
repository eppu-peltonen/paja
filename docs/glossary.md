# Glossary

Paja's parts have Finnish names from the smithy. Use them in code, docs and commit messages.

| Name | Finnish meaning | What it is |
| --- | --- | --- |
| Paja | workshop, forge | The whole factory and its headless core |
| Jono | queue | Work queue and scheduler, fed by GitHub issues |
| Kaava | pattern, formula | Specs, held-out scenarios, harness evals and the harness agents work from |
| Posti | mail | Inbox for agent questions and escalations |
| Mestari | master craftsman | Orchestrator that runs workflow graphs |
| Ahjo | forge hearth | Isolated environments where the work runs |
| Vasara | hammer | Verifier |
| Paja Desk | workshop desk | App for watching and steering the fleet |

Other terms:

- **Chunk**: one unit of work, sized so one agent can hold it in one session. Usually one GitHub issue.
- **Harness**: everything an agent works from besides the code: `CLAUDE.md`, skills, hooks, commands and rules.
- **Event log**: the append-only record of everything Paja does, and the core's only source of truth.
