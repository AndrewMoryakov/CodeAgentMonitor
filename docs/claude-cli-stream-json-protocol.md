# Claude CLI Stream-JSON Bidirectional Protocol

> Reverse-engineered from `@anthropic-ai/claude-code@2.1.77` (2026-03-16).
> Verified experimentally on Windows 11.

## Зачем этот документ

Текущая архитектура CodeAgentMonitor использует `claude --print` с закрытием stdin после каждого хода (spawn-per-turn). Это делает невозможным:
- Запросы разрешений (permission prompts) — CLI не может спросить "разрешить ли Bash?"
- AskUserQuestion — интерактивные вопросы от ИИ
- Прерывание (interrupt) во время выполнения
- Контекст между ходами (каждый ход — новый процесс)

Claude CLI поддерживает полноценный двунаправленный протокол через stdin/stdout. Этот документ описывает его полностью.

---

## 1. Запуск процесса

```bash
claude --print \
  --input-format stream-json \
  --output-format stream-json \
  --verbose \
  --include-partial-messages \
  --no-session-persistence \
  --permission-mode default
```

| Флаг | Обязательный | Описание |
|------|:---:|-----------|
| `--print` | ✓ | Неинтерактивный режим (без TUI) |
| `--input-format stream-json` | ✓ | Stdin принимает NDJSON-сообщения |
| `--output-format stream-json` | ✓ | Stdout выдаёт NDJSON-события |
| `--verbose` | ✓ | Обязателен для stream-json вывода |
| `--include-partial-messages` | | Стриминг токен-по-токену (stream_event) |
| `--replay-user-messages` | | CLI эхом возвращает user-сообщения на stdout |
| `--no-session-persistence` | | Не сохранять сессию на диск |
| `--permission-mode <mode>` | | `default`, `acceptEdits`, `bypassPermissions`, `plan`, `auto`, `dontAsk` |
| `--model <model>` | | Модель: `sonnet`, `opus`, `claude-sonnet-4-6`, etc. |
| `--session-id <uuid>` | | Привязка к конкретной сессии |
| `--continue` | | Продолжить последнюю сессию в текущей директории |
| `--resume <id>` | | Возобновить сессию по ID |
| `--max-turns <n>` | | Лимит ходов (auto-stop) |
| `--max-budget-usd <n>` | | Лимит бюджета |
| `--sdk-url <url>` | | WebSocket мост (НЕ нужен для stdin/stdout) |

### Важно

- `--input-format stream-json` **требует** `--output-format stream-json`
- `--sdk-url` **не нужен** — полный протокол работает через stdin/stdout
- Процесс **не завершается** после первого ответа — ждёт новые сообщения на stdin
- Процесс завершается при EOF на stdin или получении `interrupt`

---

## 2. Транспорт: NDJSON (Newline-Delimited JSON)

Каждое сообщение — одна строка JSON, завершённая `\n`:

```
{"type":"user","message":{"role":"user","content":"Hello"},"uuid":"...","session_id":""}\n
```

Парсинг: `JSON.parse(line)` для каждой строки (разделитель — `\n`).
Запись: `serde_json::to_string(&msg) + "\n"` на stdin.

---

## 3. Входящие сообщения (наше приложение → Claude CLI stdin)

### 3.1. Пользовательское сообщение

```json
{
  "type": "user",
  "message": {
    "role": "user",
    "content": "Текст сообщения пользователя"
  },
  "uuid": "<uuidv4>",
  "timestamp": "2026-03-17T12:00:00.000Z",
  "parent_tool_use_id": null,
  "session_id": ""
}
```

| Поле | Тип | Описание |
|------|-----|----------|
| `type` | `"user"` | Тип сообщения |
| `message.role` | `"user"` | Всегда `"user"` |
| `message.content` | `string` | Текст промпта |
| `uuid` | `string` | Уникальный ID сообщения |
| `timestamp` | `string` | ISO 8601 |
| `parent_tool_use_id` | `null` | Для tool_result — ID tool_use |
| `session_id` | `string` | Можно оставить пустым |

Дополнительные поля (опциональные):
- `isSynthetic: bool` — системное/мета сообщение
- `isReplay: bool` — повтор из истории
- `priority: "now" | "next" | "later"` — приоритет обработки

### 3.2. Ответ на запрос разрешения (control_response)

Когда CLI просит разрешение на инструмент (см. 4.6), отвечаем:

**Разрешить:**
```json
{
  "type": "control_response",
  "response": {
    "subtype": "success",
    "request_id": "<request_id из control_request>",
    "response": {
      "behavior": "allow",
      "updatedInput": null
    }
  }
}
```

**Запретить:**
```json
{
  "type": "control_response",
  "response": {
    "subtype": "success",
    "request_id": "<request_id>",
    "response": {
      "behavior": "deny",
      "message": "Пользователь запретил это действие"
    }
  }
}
```

**Ошибка:**
```json
{
  "type": "control_response",
  "response": {
    "subtype": "error",
    "request_id": "<request_id>",
    "error": "Описание ошибки"
  }
}
```

Поле `updatedInput` позволяет модифицировать входные данные инструмента перед выполнением (например, исправить команду).

### 3.3. Ответ на AskUserQuestion

AskUserQuestion — это обычный `control_request` с `tool_name: "AskUserQuestion"`. Ответ:

```json
{
  "type": "control_response",
  "response": {
    "subtype": "success",
    "request_id": "<request_id>",
    "response": {
      "behavior": "allow",
      "updatedInput": {
        "questions": [
          {
            "question": "Какую библиотеку использовать?",
            "header": "Library",
            "options": [
              { "label": "axios", "description": "HTTP client" },
              { "label": "fetch", "description": "Built-in" }
            ]
          }
        ],
        "answers": {
          "Какую библиотеку использовать?": "axios"
        }
      }
    }
  }
}
```

Формат `answers`: ключ — полный текст `question`, значение — `label` выбранного варианта.
Для `multiSelect: true` — через `", "` (запятая + пробел): `"axios, fetch"`.
Для свободного ввода (пользователь выбрал "Other") — произвольный текст.

### 3.4. Управляющие запросы (control_request → CLI)

**Прервать текущий ход:**
```json
{
  "type": "control_request",
  "request_id": "<uuidv4>",
  "request": {
    "subtype": "interrupt"
  }
}
```

**Сменить модель:**
```json
{
  "type": "control_request",
  "request_id": "<uuidv4>",
  "request": {
    "subtype": "set_model",
    "model": "claude-sonnet-4-6"
  }
}
```

**Сменить permission mode:**
```json
{
  "type": "control_request",
  "request_id": "<uuidv4>",
  "request": {
    "subtype": "set_permission_mode",
    "mode": "acceptEdits"
  }
}
```

Другие subtypes: `set_max_thinking_tokens`, `mcp_status`, `mcp_message`, `mcp_reconnect`, `mcp_toggle`, `mcp_set_servers`, `initialize`.

### 3.5. Keep-alive

```json
{"type": "keep_alive"}
```

### 3.6. Обновление переменных окружения

```json
{
  "type": "update_environment_variables",
  "variables": {
    "KEY": "value"
  }
}
```

---

## 4. Исходящие сообщения (Claude CLI stdout → наше приложение)

### 4.1. system (init)

Первое сообщение после запуска:

```json
{
  "type": "system",
  "subtype": "init",
  "cwd": "D:\\Projects\\CodeAgentMonitor",
  "session_id": "19dad926-b55a-4fda-b3d5-c3acb08fb609",
  "tools": ["Bash", "Read", "Write", "Edit", "Grep", "Glob", "WebFetch", "AskUserQuestion", "..."],
  "mcp_servers": [
    {"name": "claude.ai Notion", "status": "connected"},
    {"name": "claude.ai Gmail", "status": "needs-auth"}
  ],
  "model": "claude-opus-4-6[1m]",
  "permissionMode": "acceptEdits",
  "slash_commands": ["commit", "review", "..."],
  "apiKeySource": "none",
  "claude_code_version": "2.1.77",
  "agents": ["general-purpose", "Explore", "Plan", "..."],
  "skills": ["commit", "review", "..."],
  "uuid": "<uuid>"
}
```

### 4.2. assistant (полное сообщение)

Без `--include-partial-messages` — одно событие на весь ответ:

```json
{
  "type": "assistant",
  "message": {
    "model": "claude-opus-4-6",
    "id": "msg_01...",
    "role": "assistant",
    "content": [
      {"type": "text", "text": "Ответ ИИ"},
      {"type": "tool_use", "id": "toolu_01...", "name": "Bash", "input": {"command": "ls -la"}}
    ],
    "stop_reason": "end_turn",
    "usage": {
      "input_tokens": 100,
      "output_tokens": 50,
      "cache_read_input_tokens": 15000
    }
  },
  "session_id": "...",
  "uuid": "..."
}
```

`content` — массив блоков:
- `{"type": "text", "text": "..."}` — текст
- `{"type": "thinking", "thinking": "..."}` — размышления
- `{"type": "tool_use", "id": "toolu_...", "name": "Bash", "input": {...}}` — вызов инструмента

### 4.3. stream_event (потоковые события)

С `--include-partial-messages` — токен-по-токену:

```json
{"type": "stream_event", "event": {"type": "message_start", "message": {...}}}
{"type": "stream_event", "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}}
{"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Привет"}}}
{"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": ", мир!"}}}
{"type": "stream_event", "event": {"type": "content_block_stop", "index": 0}}
{"type": "stream_event", "event": {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 10}}}
{"type": "stream_event", "event": {"type": "message_stop"}}
```

Внутренний event следует стандарту Anthropic Messages API streaming.

### 4.4. user (результат инструмента)

После выполнения tool_use CLI сам запускает инструмент и отправляет результат:

```json
{
  "type": "user",
  "message": {
    "role": "user",
    "content": [
      {
        "tool_use_id": "toolu_01...",
        "type": "tool_result",
        "content": "total 8\ndrwxr-xr-x 2 user user 4096 ..."
      }
    ]
  },
  "session_id": "...",
  "uuid": "..."
}
```

**Важно:** CLI сам выполняет инструменты. Мы НЕ выполняем их — мы только одобряем/запрещаем (если permission mode требует).

### 4.5. result (завершение хода)

```json
{
  "type": "result",
  "subtype": "success",
  "is_error": false,
  "duration_ms": 5138,
  "duration_api_ms": 5131,
  "num_turns": 1,
  "result": "Текстовый результат",
  "stop_reason": "end_turn",
  "session_id": "...",
  "total_cost_usd": 0.0080675,
  "usage": {
    "input_tokens": 3,
    "cache_read_input_tokens": 15855,
    "output_tokens": 5,
    "service_tier": "standard"
  },
  "modelUsage": {
    "claude-opus-4-6[1m]": {
      "inputTokens": 3,
      "outputTokens": 5,
      "cacheReadInputTokens": 15855,
      "costUSD": 0.0080675,
      "contextWindow": 1000000,
      "maxOutputTokens": 64000
    }
  },
  "permission_denials": [],
  "uuid": "..."
}
```

Subtypes: `success`, `error_max_turns`, `error_during_execution`, `error_max_budget_usd`, `error_max_structured_output_retries`.

### 4.6. control_request (запрос разрешения)

Когда CLI хочет использовать инструмент, требующий одобрения:

```json
{
  "type": "control_request",
  "request_id": "a1b2c3d4-...",
  "request": {
    "subtype": "can_use_tool",
    "tool_name": "Bash",
    "input": {
      "command": "rm -rf /tmp/test",
      "description": "Delete test directory"
    },
    "tool_use_id": "toolu_01...",
    "description": "Run bash command: rm -rf /tmp/test",
    "permission_suggestions": [],
    "blocked_path": null
  }
}
```

| Поле | Описание |
|------|----------|
| `request_id` | UUID для корреляции ответа |
| `tool_name` | Имя инструмента: `Bash`, `Write`, `Edit`, `Read`, `Grep`, `WebFetch`, `AskUserQuestion`, etc. |
| `input` | Параметры инструмента (зависят от tool_name) |
| `tool_use_id` | ID блока tool_use из assistant-сообщения |
| `description` | Человекочитаемое описание действия |
| `permission_suggestions` | Подсказки для правил разрешений |
| `blocked_path` | Путь, на который нет доступа |

**CLI блокируется до получения `control_response`** с matching `request_id`.

### 4.7. rate_limit_event

```json
{
  "type": "rate_limit_event",
  "rate_limit_info": {
    "status": "allowed",
    "resetsAt": 1773766800,
    "rateLimitType": "five_hour",
    "overageStatus": "allowed",
    "overageResetsAt": 1775001600,
    "isUsingOverage": false
  },
  "uuid": "...",
  "session_id": "..."
}
```

### 4.8. system (другие subtypes)

- `compact_boundary` — компакция контекста
- `hook_started`, `hook_progress` — хуки
- `status` — смена permission mode

---

## 5. Полный жизненный цикл сессии

```
Приложение                         Claude CLI
    │                                  │
    │  ──── spawn process ────────────>│
    │                                  │
    │  <──── system (init) ────────────│   // tools, model, session_id
    │                                  │
    │  ──── user message ────────────> │   // {"type":"user","message":{...}}
    │                                  │
    │  <──── stream_event (deltas) ────│   // токен-по-токену (optional)
    │  <──── assistant (snapshot) ─────│   // полное сообщение с tool_use
    │                                  │
    │  <──── control_request ──────────│   // "можно ли выполнить Bash: rm -rf?"
    │  ──── control_response ────────> │   // "allow" / "deny"
    │                                  │
    │  <──── user (tool_result) ───────│   // результат выполнения
    │  <──── assistant (next msg) ─────│   // продолжение ответа
    │  <──── rate_limit_event ─────────│
    │  <──── result (success) ─────────│   // ход завершён
    │                                  │
    │  ──── user message ────────────> │   // следующий ход (тот же процесс!)
    │  ...                             │
    │                                  │
    │  ──── EOF (close stdin) ────────>│   // завершение сессии
    │  <──── process exit ─────────────│
```

---

## 6. Маппинг на Codex JSON-RPC (фронтенд)

Наш фронтенд ожидает Codex-совместимые события. Маппинг:

| Claude CLI событие | Codex событие | Примечания |
|---|---|---|
| `system` (init) | `codex/connected` + `thread/started` | Одноразово при запуске |
| `stream_event` (content_block_start, text) | `item/started` + `turn/started` | Начало текстового блока |
| `stream_event` (content_block_delta, text) | `item/agentMessage/delta` | Стриминг текста |
| `stream_event` (content_block_start, thinking) | `item/started` (reasoning) | Начало размышлений |
| `stream_event` (content_block_delta, thinking) | `item/reasoning/textDelta` | Стриминг мыслей |
| `stream_event` (content_block_start, tool_use) | `item/started` (commandExecution/fileChange) | Начало tool_use |
| `stream_event` (content_block_stop) | `item/completed` | Завершение блока |
| `stream_event` (message_stop) | — | Конец assistant-сообщения |
| `assistant` (snapshot) | Извлечь модель, usage | Не дублировать content |
| `user` (tool_result) | `item/commandExecution/outputDelta` | Результат инструмента |
| `control_request` (can_use_tool) | `*requestApproval` | Показать диалог разрешения |
| `control_request` (AskUserQuestion) | `item/tool/requestUserInput` | Показать диалог вопроса |
| `result` (success) | `turn/completed` | Ход завершён |
| `result` (error) | `turn/completed` + `error` | Ошибка |
| `rate_limit_event` | `account/rateLimits/updated` | Rate limit данные |

---

## 7. Архитектурная миграция: spawn-per-turn → persistent process

### Текущая архитектура (process.rs)

```
turn/start → spawn claude --print → write prompt → close stdin → read stdout → result
turn/start → spawn claude --print → write prompt → close stdin → read stdout → result
```

Каждый ход — новый процесс. Нет контекста между ходами. Нет обратной связи.

### Целевая архитектура

```
session/start → spawn claude --print --input-format stream-json ...
                │
                ├── stdin (мы пишем): user messages, control_responses
                └── stdout (мы читаем): events, control_requests
                │
                ├── turn 1: write user msg → read events → result
                ├── turn 2: write user msg → read events → control_request → write response → result
                ├── turn 3: ...
                │
session/end → close stdin → process exits
```

### Ключевые изменения в process.rs

1. **Один процесс на сессию** — `spawn_claude_session()` создаёт долгоживущий процесс
2. **stdin остаётся открытым** — `TurnRequest` записывается как JSON-строка в stdin
3. **Coordinator** маршрутизирует события от stdout:
   - Обычные события → `event_mapper` → фронтенд
   - `control_request` → UI показывает диалог → `control_response` → stdin
4. **BridgeState** живёт на всю сессию (не per-turn)
5. **Прерывание** — отправить `control_request` с `subtype: "interrupt"` на stdin

### Новая структура TurnRequest

```rust
// Вместо prompt + close stdin:
struct TurnMessage {
    content: String,
    uuid: String,
}

// Отправка на stdin:
fn send_user_message(stdin: &mut ChildStdin, msg: &TurnMessage) {
    let json = json!({
        "type": "user",
        "message": {"role": "user", "content": msg.content},
        "uuid": msg.uuid,
        "parent_tool_use_id": null,
        "session_id": ""
    });
    stdin.write_all(format!("{}\n", json).as_bytes());
}
```

### Новая обработка permission prompt

```rust
match event_type.as_str() {
    "control_request" => {
        let request_id = msg["request_id"].as_str();
        let tool_name = msg["request"]["tool_name"].as_str();

        // Emit approval request to frontend
        event_sink.emit(AppServerEvent {
            message: json!({
                "id": request_id,
                "method": format!("{}/requestApproval", tool_category),
                "params": { /* tool details */ }
            })
        });

        // Frontend responds via interceptor → we write control_response to stdin
    }
}
```

---

## 8. Полезные флаги для отладки

```bash
# Подробные логи в файл
claude --print --input-format stream-json --output-format stream-json \
  --verbose --debug --debug-file /tmp/claude-debug.log

# Пропуск всех разрешений (для тестирования)
claude --print --input-format stream-json --output-format stream-json \
  --verbose --dangerously-skip-permissions

# Только одобрение правок (без вопросов о Bash)
claude --print --input-format stream-json --output-format stream-json \
  --verbose --permission-mode acceptEdits

# Ограничение бюджета
claude --print --input-format stream-json --output-format stream-json \
  --verbose --max-budget-usd 1.00 --max-turns 10
```

---

## 9. Различия с текущим --print режимом

| Аспект | `--print` (текущий) | `--input-format stream-json` (целевой) |
|--------|---|---|
| Жизнь процесса | Один ход | Вся сессия |
| stdin | Текст промпта → EOF | NDJSON-поток сообщений |
| Контекст между ходами | Нет (--continue) | Автоматически |
| Permission prompts | Невозможно | `control_request`/`control_response` |
| AskUserQuestion | Невозможно | Через тот же механизм |
| Прерывание | kill процесс | `control_request` (interrupt) |
| Смена модели | Новый процесс | `control_request` (set_model) |
| Стоимость | Холодный старт на каждый ход | Один тёплый процесс |

---

## 10. Ссылки

- [Claude Code CLI Reference](https://code.claude.com/docs/en/cli-reference)
- [Claude Code SDK](https://docs.anthropic.com/en/docs/claude-code/sdk)
- SDK пакет: `@anthropic-ai/claude-code` (npm), переименован в `@anthropic-ai/claude-agent-sdk`
- Исходники: `cli.js` в npm-пакете (bundled, minified)
- Текущая реализация: `src-tauri/src/claude_bridge/process.rs`
- Маппинг событий: `src-tauri/src/claude_bridge/event_mapper.rs`
- Типы bridge state: `src-tauri/src/claude_bridge/types.rs`
