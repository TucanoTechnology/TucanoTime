---
load-when: always — check before carrying out a request
applies-to: every repository
applies-to-models: deepseek
title: Peak-hour warnings
---

# Peak-hour warnings

Requests are billed and rate-limited at a premium during the organisation's
peak windows. When a request is carried out inside one, surface a short warning
before doing the work so the operator can decide whether to defer it.

- **Peak windows (UTC):** `01:00–04:00` and `06:00–10:00` UTC.
- **When to warn:** at the start of a task, resolve the current UTC time and
  compare it to the windows above. If the time falls inside one, open the
  response with a one-line warning naming the current UTC time and the window,
  then carry on with the request unless it was refused.
- **When not to warn:** outside the windows, or when the request is purely
  read-only. State the current UTC time once so the check is visible.
- **Model scope:** this rule is triggered by the DeepSeek API. Agents backed by
  other models may ignore it.

Use `date -u '+%Y-%m-%d %H:%M UTC'` to resolve the time; do not infer it.
