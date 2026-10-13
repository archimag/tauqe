# OpenRouter Chat Completions Protocol

This document defines the OpenRouter interaction protocol for:

- ordinary text responses;
- Structured Output;
- streaming and non-streaming operation;
- configurable reasoning effort in both modes;
- usage and reasoning metadata.

The application should expose one provider-independent model interface and keep OpenRouter-specific transport details inside the OpenRouter adapter.

---

## 1. Endpoint

Use the OpenAI-compatible Chat Completions endpoint:

```text
POST https://openrouter.ai/api/v1/chat/completions
```

Required headers:

```http
Authorization: Bearer <OPENROUTER_API_KEY>
Content-Type: application/json
```

Optional application-identification headers:

```http
HTTP-Referer: <application-url>
X-Title: <application-name>
```

Basic request:

```json
{
  "model": "~anthropic/claude-haiku-latest",
  "messages": [
    {
      "role": "system",
      "content": "System instructions."
    },
    {
      "role": "user",
      "content": "User request."
    }
  ]
}
```

---

## 2. Reasoning Effort

Reasoning effort is independent of the output mode.

It can be used with:

```text
ordinary text
ordinary text + streaming
structured output
structured output + streaming
```

Preferred OpenRouter request shape:

```json
{
  "reasoning": {
    "effort": "medium"
  }
}
```

Possible effort values include:

```text
none
minimal
low
medium
high
xhigh
```

Support is model-dependent. Do not assume every model supports every effort level.

The application should represent effort internally as a provider-independent enum and translate it into OpenRouter's `reasoning.effort`.

Do not normally send both:

```json
"reasoning": {
  "effort": "high"
}
```

and:

```json
"reasoning_effort": "high"
```

`reasoning_effort` is an OpenAI-style compatibility parameter supported by some models/endpoints. For new OpenRouter-native code, prefer:

```json
"reasoning": {
  "effort": "high"
}
```

If the selected model does not support reasoning or the requested effort level, either omit the parameter or reject/fallback according to application policy.

Reasoning effort affects inference behavior, typically including:

```text
quality/depth
latency
reasoning-token usage
cost
```

It does not change the transport protocol.

---

## 3. Model Capability Detection

Do not hard-code the assumption that all models support:

```text
reasoning
structured output
tools
specific reasoning effort levels
```

OpenRouter model metadata exposes supported parameters.

The adapter should distinguish at least:

```text
supports_reasoning
supports_structured_output
supports_tools
```

If a request requires a feature, use provider routing that requires the requested parameters:

```json
{
  "provider": {
    "require_parameters": true
  }
}
```

This is particularly important for Structured Output because different provider endpoints serving the same model may support different parameters.

---

# Ordinary Text Mode

## 4. Non-Streaming Text

Request:

```json
{
  "model": "<model>",
  "messages": [...],
  "reasoning": {
    "effort": "low"
  },
  "stream": false
}
```

Typical response:

```json
{
  "id": "...",
  "model": "...",
  "choices": [
    {
      "index": 0,
      "finish_reason": "stop",
      "message": {
        "role": "assistant",
        "content": "Complete model response"
      }
    }
  ],
  "usage": {
    "prompt_tokens": 100,
    "completion_tokens": 50,
    "total_tokens": 150
  }
}
```

Primary result:

```text
choices[0].message.content
```

Reasoning-capable models may additionally return reasoning-related fields such as:

```text
message.reasoning
message.reasoning_details
```

Treat reasoning metadata separately from final answer content.

Do not concatenate reasoning into `content`.

---

## 5. Streaming Text

Request:

```json
{
  "model": "<model>",
  "messages": [...],
  "reasoning": {
    "effort": "medium"
  },
  "stream": true
}
```

The HTTP response is an SSE stream.

Critical rule:

```text
HTTP chunks != SSE events
```

A TCP/HTTP read may contain:

```text
part of one SSE event
one complete event
multiple SSE events
```

Therefore first parse SSE framing.

Typical SSE record:

```text
data: {"choices":[{"index":0,"delta":{"content":"Hello"}}]}
```

Stream termination:

```text
data: [DONE]
```

Text is normally streamed through:

```text
choices[*].delta.content
```

Example:

```text
delta.content = "Hel"
delta.content = "lo "
delta.content = "world"
```

Accumulate as:

```text
Hello world
```

Do not assume every event contains `content`.

Chunks may contain:

```text
reasoning
reasoning_details
usage
finish_reason
tool calls
metadata
empty deltas
```

Reasoning-related streaming data must remain separate from final-answer text.

A useful internal event model is:

```text
ReasoningDelta
TextDelta
Usage
Finish
Error
```

`finish_reason` describes completion of a choice. It is not a substitute for correctly processing the SSE stream lifecycle.

---

# Structured Output Mode

## 6. Structured Output

Structured Output constrains the final assistant content to a JSON Schema.

Request:

```json
{
  "model": "<model>",
  "messages": [...],

  "reasoning": {
    "effort": "high"
  },

  "response_format": {
    "type": "json_schema",
    "json_schema": {
      "name": "result",
      "strict": true,
      "schema": {
        "type": "object",
        "properties": {
          "summary": {
            "type": "string"
          },
          "success": {
            "type": "boolean"
          }
        },
        "required": [
          "summary",
          "success"
        ],
        "additionalProperties": false
      }
    }
  },

  "provider": {
    "require_parameters": true
  }
}
```

Reasoning effort works exactly as in ordinary mode:

```json
"reasoning": {
  "effort": "high"
}
```

`response_format` changes the output contract; it does not replace or disable reasoning controls.

Use:

```json
"strict": true
```

when strict schema adherence is desired.

Still validate the returned object in application code.

Schema-valid output does not imply semantic correctness.

---

## 7. Non-Streaming Structured Output

Request:

```json
{
  "model": "<model>",
  "messages": [...],

  "reasoning": {
    "effort": "medium"
  },

  "response_format": {
    "type": "json_schema",
    "json_schema": {
      "name": "result",
      "strict": true,
      "schema": {
        "...": "..."
      }
    }
  },

  "provider": {
    "require_parameters": true
  },

  "stream": false
}
```

The assistant's `content` contains the JSON document.

Conceptually:

```json
{
  "summary": "Operation completed",
  "success": true
}
```

At the Chat Completions level, obtain it from:

```text
choices[0].message.content
```

Then:

```text
content
→ JSON parse
→ schema validation
→ application semantic validation
→ typed result
```

Do not treat raw content as trusted application state before validation.

Reasoning metadata, when available, is separate from the structured result.

The JSON Schema applies to the assistant's answer, not to reasoning metadata.

---

## 8. Streaming Structured Output

Structured Output can be combined with:

```json
"stream": true
```

and reasoning effort:

```json
"reasoning": {
  "effort": "high"
}
```

Example request:

```json
{
  "model": "<model>",
  "messages": [...],

  "reasoning": {
    "effort": "high"
  },

  "response_format": {
    "type": "json_schema",
    "json_schema": {
      "name": "result",
      "strict": true,
      "schema": {
        "type": "object",
        "properties": {
          "summary": {
            "type": "string"
          },
          "success": {
            "type": "boolean"
          }
        },
        "required": [
          "summary",
          "success"
        ],
        "additionalProperties": false
      }
    }
  },

  "provider": {
    "require_parameters": true
  },

  "stream": true
}
```

The transport is still SSE.

Structured content is streamed through:

```text
choices[*].delta.content
```

The deltas are fragments of one JSON document.

Example:

```text
delta.content = "{\"summary\":"
delta.content = "\"Operation "
delta.content = "completed\","
delta.content = "\"success\":true}"
```

Accumulated:

```json
{
  "summary": "Operation completed",
  "success": true
}
```

Critical rule:

```text
individual delta.content fragments are not independent JSON documents
```

Do not call a normal JSON parser on every fragment.

Two valid strategies exist:

```text
A. Buffer everything
   → parse once when complete

B. Incremental JSON parser
   → expose partial structured state
   → validate complete object at end
```

Even with incremental parsing, irreversible side effects must not be triggered from partial JSON.

Final lifecycle:

```text
SSE
→ content deltas
→ accumulate / incremental parse
→ stream completes
→ parse complete JSON
→ schema validation
→ semantic validation
→ accept typed result
```

If the stream terminates unexpectedly before a complete valid JSON document exists:

```text
discard structured result
```

Partial output may remain visible for presentation/debugging, but it is not authoritative.

Reasoning deltas must not be appended to the structured JSON buffer.

Maintain separate streams:

```text
reasoning stream
structured content stream
usage/metadata stream
```

---

# Reasoning Output

## 9. Reasoning Metadata

Reasoning effort controls how much reasoning the model performs.

Whether reasoning is exposed to the caller is a separate concern.

OpenRouter may expose reasoning through fields such as:

```text
reasoning
reasoning_details
```

For models supporting reasoning continuation, preserve returned `reasoning_details` when replaying the assistant message into a continuation request.

Do not modify, summarize, or reconstruct provider reasoning metadata before sending it back as continuation state.

Internally distinguish:

```text
ReasoningConfig
    effort

ReasoningOutput
    provider reasoning metadata
```

The configured effort and returned reasoning are not the same thing.

A model may perform reasoning without exposing a human-readable reasoning stream.

Do not require visible reasoning in order to support effort.

---

# Usage and Cost

## 10. Usage

To explicitly request usage metadata:

```json
{
  "usage": {
    "include": true
  }
}
```

Usage may include:

```json
{
  "prompt_tokens": 1200,
  "completion_tokens": 300,
  "total_tokens": 1500
}
```

Reasoning-capable models may expose additional token details, including reasoning tokens.

In streaming mode, usage may appear only near the end of the stream.

Do not expect it on every chunk.

OpenRouter may also provide calculated request cost in usage metadata.

Prefer OpenRouter's reported cost over reproducing model/provider pricing manually.

Internally normalize to something like:

```text
Usage {
    input_tokens,
    output_tokens,
    reasoning_tokens?,
    cached_tokens?,
    cost_usd?
}
```

All fields except the basic token counts should be treated as optional because provider/model support varies.

---

# SSE Parsing

## 11. SSE Decoder

Keep SSE framing completely separate from OpenRouter JSON decoding.

Pipeline:

```text
HTTP byte stream
      ↓
SSE decoder
      ↓
SSE data payload
      ↓
if payload == "[DONE]"
    finish
else
    JSON decode
      ↓
ChatCompletionChunk
```

Do not assume:

```text
one HTTP chunk = one SSE event
one SSE event = one token
every SSE event has choices
every choice delta has content
every response exposes reasoning
usage accompanies text
```

Ignore SSE comments / keepalive events that contain no model payload.

---

# Errors

## 12. Failure Classes

Keep these failures distinct:

```text
HTTP failure
authentication failure
rate limit
provider failure
SSE framing failure
malformed JSON event
interrupted stream
unsupported model parameter
structured JSON parse failure
schema validation failure
semantic validation failure
```

A valid HTTP response does not imply a valid model result.

A valid JSON response does not imply a valid Structured Output result.

A schema-valid result does not imply semantic correctness.

---

# Recommended Internal Interface

## 13. Provider-Independent Request

The application should expose something conceptually similar to:

```text
ModelRequest {
    model,
    messages,
    effort?,
    output_mode,
    stream
}
```

Where:

```text
effort =
    none
    minimal
    low
    medium
    high
    xhigh
```

And:

```text
output_mode =
    Text
    Structured(schema)
```

OpenRouter adapter mapping:

```text
effort
→ reasoning.effort

Structured(schema)
→ response_format.json_schema

stream
→ stream: true/false
```

This keeps effort orthogonal to output mode.

The application must support all four combinations:

```text
Text + non-streaming
Text + streaming
Structured + non-streaming
Structured + streaming
```

with the same effort configuration.

---

## 14. Provider-Independent Stream

Recommended normalized events:

```text
ModelEvent
├── ReasoningDelta
├── TextDelta
├── Usage
├── Finish
└── Error
```

For Structured Output, `TextDelta` contains JSON fragments.

A higher layer may consume them through:

```text
StructuredOutputAssembler
```

Result:

```text
OpenRouter HTTP/SSE
        ↓
OpenRouter decoder
        ↓
normalized ModelEvent
        ↓
Text presentation
or
StructuredOutputAssembler
        ↓
typed validated result
```

Raw OpenRouter response types should not leak into core application logic.

---

# Summary

Ordinary streaming with effort:

```text
reasoning.effort
      ↓
OpenRouter
      ↓
SSE
      ↓
ReasoningDelta / TextDelta
```

Structured streaming with effort:

```text
reasoning.effort
+ JSON Schema
      ↓
OpenRouter
      ↓
SSE
      ↓
reasoning deltas      JSON content deltas
      │                       │
      │                 assemble JSON
      │                       ↓
      │                 parse + validate
      │                       ↓
      └────────────────── typed result
```

Core rules:

```text
effort is independent of output mode

use reasoning.effort as the canonical OpenRouter control

do not mix reasoning with final content

HTTP chunks are not SSE events

Structured Output streaming produces partial JSON

never execute irreversible side effects from partial structured output

validate complete Structured Output in application code

use provider.require_parameters when required features must be honored
```
