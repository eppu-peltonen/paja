---
name: answer
description: Answer a question Paja escalated. Lists the escalated items with their questions, takes the person's answer, posts it on the issue and queues the item again. Use when the person wants to reply to Paja or unblock an item.
---

# Answer Paja

Use only the `paja` MCP tools; don't run shell commands.

1. Call `work_list` and keep the items whose `status` is `escalated`. If there are none, say that nothing is waiting and stop.
2. If the person named an item (for example `/paja:answer 31`), use it. Otherwise show each escalated item's number, title, link and question (its `note`), and ask which one they want to answer.
3. Ask for the answer if they haven't given it. Help them make it a clear decision Paja can act on, but keep their meaning; never answer for them.
4. Show the exact text that will be posted on the issue and wait for the person to confirm.
5. Call `work_answer` with the number and the text. It posts the answer as a comment on the issue and sets the item back to queued, and fails without changing anything if the item isn't escalated.
6. Say that the answer is posted and the item is queued, with the issue link. The next `paja run` picks it up.
