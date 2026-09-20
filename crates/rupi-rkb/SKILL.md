# RKB knowledge retrieval

RKB is optional and lazy. It is not connected merely because this skill is present;
retrieve only after the configured server has been explicitly enabled (`/mcp enable
rkb`, or the configured RKB server name). If it is not active, do not emit an RKB tool
call—continue without RKB or ask the operator to enable it.

Use RKB when a question needs durable project knowledge, design decisions, or
source-backed evidence. Search with `get_agent_context` before answering and
retain each returned `record_id` and citation. Prefer concise snippets in the
active context; references may be rehydrated later with `search_chunks` using
the exact record id. Preserve source URL, source document, and page when
reporting evidence. Do not present an RKB citation as native model reasoning.
