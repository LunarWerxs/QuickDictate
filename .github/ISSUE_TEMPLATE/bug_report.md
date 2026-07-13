---
name: Bug report
about: Report a problem with QuickDictate
title: ""
labels: bug
assignees: ""
---

> **NEVER paste API keys (or full log lines that might contain them) into this issue.** Redact anything from your `settings.json` key arrays before pasting. Issues are public.

### Describe the bug

A clear, concise description of what's wrong.

### Provider & settings

- `stt_provider`: <!-- e.g. elevenlabs / deepgram / openai / assemblyai / dashscope / google -->
- `stt_model`: <!-- e.g. null / custom model string -->
- `dashscope_intl` (if using dashscope): <!-- true / false -->
- `mode`: <!-- toggle / hold -->
- Any other non-default settings you changed:

**Do not paste your key arrays.** If a key is involved (e.g. "invalid key" error), just say which provider and whether the key is new/old — do not paste the key value itself.

### Steps to reproduce

1.
2.
3.

### Expected vs actual

**Expected:**

**Actual:**

### quickdictate.log excerpt

Set `"enable_logging": true` in `settings.json`, reproduce the issue, then paste the relevant excerpt from `quickdictate.log` (found next to the exe) below.

**Before pasting, check the excerpt for API keys or other secrets and redact them.**

```
paste log excerpt here
```

### Windows version

<!-- e.g. Windows 11 23H2, from Settings > System > About -->

### QuickDictate version

<!-- e.g. v0.4.0, or the commit hash you built from -->
