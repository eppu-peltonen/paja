# How one piece of work flows

A chunk starts as a GitHub issue and ends as verified code on `main`. People are involved in only two places: approving
the spec, and answering escalations when the automated gates can't decide. Everything between spec approval and a
green `main` is one Mestari graph, with each step a fresh headless Claude Code run in its own Ahjo environment.

```mermaid
flowchart TB
    idea["Issue with the work label"] --> spec["Agent drafts spec"]
    spec --> approve{"Person approves spec?"}
    approve -- "no: revise" --> spec
    approve -- yes --> plan["Plan and review plan"]
    plan --> build["Build in its own environment"]
    build --> checks{"Vasara: checks pass?"}
    checks -- "no: fix loop, max 3 rounds" --> build
    checks -- yes --> review{"Reviewer agent(s) agree?"}
    review -- "changes asked" --> build
    review -- "can't decide" --> posti["Escalate to Posti"]
    build -- "fix cap reached" --> posti
    review -- yes --> land["Land on main"]
    land --> green{"main still green?"}
    green -- yes --> done["Landed and verified"]
    green -- no --> revert["Auto-revert, refile"]
    done --> learn["Learn step"]
    revert --> learn

    classDef human fill:#e8f0fe,stroke:#1a73e8,stroke-width:2px
    class approve,posti human
```

The issue is usually one with the work label. When no such item is ready, `paja run` takes the oldest open issue with
the follow-up label instead (the non-blocking findings reviewers leave behind), and it goes through the same loop,
risk tier and `Depends on` rules. Work items always come first: a work item that becomes ready is picked before any
follow-up at the next pick. `follow-ups = off` in `.paja/queue` on the default branch turns this off.

After landing, the learn step turns any revert, escalation or human fix into a proposed harness rule or check. It is
adopted when the eval suite shows it helps, and it appears in the next Posti digest.
