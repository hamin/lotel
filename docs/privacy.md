# Privacy

Coding agents can send the text of your prompts, tool inputs and outputs, and command lines. lotel's default is to hide that content in the UI and to forget it at the retention line. This page explains what lotel stores, what it shows, and how to change that.

## Stored vs shown

**Stored: everything that arrives.** lotel writes each log record, span and metric point as it was sent. It does not drop or rewrite any field on the way in. If an agent sends prompt text, that text is in the store.

**Shown: masked by default.** The pages that show stored text pass it through one masking step before it renders. That covers log bodies and attributes, span names, status messages and attributes, the session timelines, the Spend and Tools tables, the Fields panel, and the results of the Explore SQL console.

**Forgotten: on schedule.** Stored content ages out with everything else: logs after 7 days and traces after 3 days by default. See [Retention](retention.md).

## What the mask hides

In JSON log bodies and attribute bags, the value of any of these keys shows as `[REDACTED]`, at any depth and in any letter case:

| Kind | Keys |
| --- | --- |
| Prompts and responses | `prompt`, `prompt_text`, `user_prompt`, `response`, `response.model_output` |
| Tool input and output | `tool_input`, `tool_parameters`, `tool_output`, `content`, `output`, `diff`, `new_context` |
| System prompt | `system_prompt_preview`, `user_system_prompt`, `system_reminders` |
| Commands | `full_command`, `bash_command`, `command` |

The key stays visible, so you can see that a field was sent. The whole value is hidden, even when it is a nested object. Other fields, such as `prompt_length`, `tool_name` or `success`, stay readable.

Two more rules apply:

- **opencode's start-up line.** opencode logs a body that starts with `cli starting` followed by its full command line, prompt included. lotel keeps the `cli starting` message and replaces the rest with `[REDACTED]`.
- **Emails and home paths in any text.** An email address shows as `[redacted-email@example.invalid]`. A home directory path such as `/Users/<name>/...` or `/home/<name>/...` shows as `[redacted-path]/...`: the part that names the person is hidden, the file name after it stays.

Text that is not JSON is shown as it was sent, apart from the email and path masks.

## What the setup blocks turn on

The blocks on the Setup page, also shown in [Quickstart](quickstart.md), turn on each agent's telemetry and nothing more. They do not enable any agent option that exports prompt or tool text. The Codex blocks set `log_user_prompt = false`.

Some content arrives anyway. opencode's start-up line is one example. That is why lotel masks at render time instead of relying on the agent's settings.

## Show content on your machine

To see the stored content, start lotel with:

```sh
LOTEL_AGENT_SHOW_CONTENT=1 lotel
```

`1`, `true`, `on` and `yes` (any case) turn masking off. Unset, empty, `0` or any other value keeps it on.

With it on:

- The pages above show stored text exactly as it was sent, including emails and home paths.
- The startup banner prints `lotel: agent content: shown`.
- The Sessions, Spend, Tools and Setup pages show a `content: shown` badge and say that content is shown.

The setting changes display only. Turning it on or off does not change what is in the store.

## What masking does not do

Masking is a display step. Keep these limits in mind:

- **The files are not masked.** The data directory holds the raw text in Parquet files. Anyone who can read the directory can read the content. Treat it as sensitive.
- **Filters match stored values.** A filter such as `prompt~deploy` on the Logs page matches against the raw value, even though the result shows `[REDACTED]`. Someone with UI access can probe for text this way.
- **The UI has no login.** Anyone who can reach the UI port can browse it. Both ports bind to `127.0.0.1` by default. See [Configuration](configuration.md) before you bind them to another address.

To keep content out of lotel entirely, don't have your agents send it. To shorten how long it stays, lower the retention for logs and traces. See [Retention](retention.md).
