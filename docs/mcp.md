# AI agents

An AI agent connects to Rudder over MCP (the Model Context Protocol) and calls the same actions as the panel. You decide what that agent is allowed to do when you approve it.

## Connect an agent

1. In the agent, add a remote MCP server. The address is the panel's public address with `/mcp` on the end, for example `https://panel.example.com/mcp`.
2. The agent opens a browser page on the panel. Sign in if you are not already.
3. Choose a permission template, or pick permissions one by one, then approve.
4. The agent receives a short-lived access token and a refresh token. It refreshes on its own until you revoke it or the approval reaches 90 days.

The address Rudder gives agents is the panel domain saved under **Certificates → Panel domain**. Without one it falls back to `RUDDER_PUBLIC_URL`, then to the address the request came in on. An agent that is not on the same machine (Claude, for example) needs an HTTPS domain: save one and press **Check SSL** on the same page to confirm the certificate is trusted, covers the domain, and is not about to expire.

**Admin → AI agents** lists every approval, the template, and the last time it was used. **Revoke** cuts off the access token and the refresh token immediately.

## Permission templates

| Template | What the agent can do |
| --- | --- |
| **Full control** | Read and change every setting and all data, including users, tokens, and updates. |
| **Read everything** | Read every setting and all data. Cannot change anything. |
| **Operate** | Deploy and manage apps, domains, databases, stacks, caches, logs, monitors, Rudder Access, and security checks. Can read the Cloudflare Zero Trust settings but not change them. Cannot change users, tokens, backups, or panel updates. |
| **Watch** | Read health, logs, metrics, cache stats, and security checks. |
| **Custom** | Only the boxes you tick. A write permission includes the matching read permission. |

Only an administrator can approve an agent, and only from a signed-in browser. An agent cannot approve a wider grant for itself.

## How refresh works

- The access token lasts 15 minutes.
- The refresh token lasts 30 days and is replaced on every refresh. The previous refresh token stops working.
- Presenting a refresh token that was already replaced revokes that approval. The agent must be approved again.
- An approval that is older than 90 days cannot be refreshed. Approve the agent again.

## When you add a feature

Every panel feature is an MCP tool. A new HTTP route becomes a tool automatically, under the permission scope of its OpenAPI tag. Full control includes that tool. See the MCP parity rule in `AGENTS.md`.

## Search the documentation

Every panel carries its own copy of these guides, so an agent reads the documentation of the exact version you run, from the address it is already connected to. Three tools cover it (they need the `system:read` permission, which every template includes):

| Tool | HTTP | Use |
| --- | --- | --- |
| `get_v1_docs_search` | `GET /api/v1/docs/search?q=…&limit=5` | Ranked matches: guide, section, and a snippet. `limit` is at most 20 |
| `get_v1_docs__name_` | `GET /api/v1/docs/{name}` | One whole guide as Markdown, for example `cli` |
| `get_v1_docs` | `GET /api/v1/docs` | The guide names and titles |

Pass `q` as a query argument: `{"query": {"q": "deploy an image"}}`. A custom approval needs the **System** read permission ticked to use them.

## Teach agents about Rudder (skills)

`rudder skills install` puts a Rudder skill where AI coding agents look for skills. It explains what Rudder is, how to use the CLI and MCP tools, which actions need your confirmation, and how to search these docs. See [Skills for AI agents](cli.md#7-skills-for-ai-agents) in the CLI guide.
