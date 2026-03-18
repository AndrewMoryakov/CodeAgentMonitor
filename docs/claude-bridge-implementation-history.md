# Claude CLI Bridge: Техническая документация

> История внедрения Claude CLI как альтернативного AI-бэкенда в CodeAgentMonitor.
> Период: 5 марта — 18 марта 2026. Коммиты: `bcbdd24..dfb5c51` (17 коммитов).

---

## 1. Контекст проекта

**CodeAgentMonitor** — десктопное приложение (Tauri + React), предоставляющее GUI для работы с AI-агентами. Изначально поддерживало только OpenAI Codex через JSON-RPC протокол поверх WebSocket/stdin.

**Задача:** добавить поддержку **Claude CLI** (`claude` от Anthropic) как альтернативного бэкенда, не ломая существующую Codex-интеграцию и переиспользуя весь фронтенд.

**Ключевое ограничение:** фронтенд общается по Codex JSON-RPC протоколу (30+ методов). Claude CLI выводит совершенно другой формат — `stream-json` (NDJSON). Нужен **мост (bridge)**, транслирующий между протоколами.

---

## 2. Архитектура решения

### 2.1. Принцип: Request Interceptor

Вместо создания отдельного бинарника-моста, был использован паттерн **request interceptor** — поле `request_interceptor: Option<Arc<dyn Fn(Value) -> InterceptAction>>` в структуре `WorkspaceSession`.

Все JSON-RPC сообщения от фронтенда проходят через interceptor, который:
- **`Respond(Value)`** — немедленно возвращает ответ (без записи в stdin процесса)
- **`Forward(String)`** — передаёт в stdin (используется для Codex)
- **`Drop`** — молча игнорирует сообщение

Это позволило полностью изолировать логику Claude в модуле `claude_bridge/`, не модифицируя ядро `WorkspaceSession`.

### 2.2. Модульная структура

```
src-tauri/src/claude_bridge/
├── mod.rs                  # Публичный API модуля (5 строк)
├── types.rs                # Типы Claude CLI stream-json событий (837 строк)
├── event_mapper.rs         # Маппинг Claude → Codex событий (647 строк)
├── event_mapper_tests.rs   # Тесты event_mapper (1473 строки)
├── item_tracker.rs         # Трекинг жизненного цикла инструментов (711 строк)
├── process.rs              # Управление процессом Claude CLI (1754 строки)
└── history.rs              # Загрузка истории из JSONL-файлов (695 строк)
                            # ИТОГО: 6122 строк Rust-кода
```

### 2.3. Эволюция архитектуры процесса

#### Этап 1 (Phase 1–6): Spawn-per-turn
```
turn/start → spawn claude --print → write prompt → close stdin → read stdout → result
turn/start → spawn claude --print → write prompt → close stdin → read stdout → result
```
Каждый ход пользователя — новый процесс. Просто, но с ограничениями.

#### Этап 2 (Persistent Process): Один процесс на сессию
```
session/start → spawn claude --print --input-format stream-json ...
                ├── stdin writer task (mpsc channel)
                ├── stdout reader task (NDJSON parser)
                └── interceptor (JSON-RPC ↔ Claude protocol)
```
Один долгоживущий процесс с двунаправленным NDJSON-протоколом.

---

## 3. Хронология внедрения

### Phase 0: Анализ и планирование (5 марта)

**Коммит:** `bcbdd24` — Анализ маппинга Claude CLI → Codex протокол.
**Коммит:** `2bbbd9b` — Детальный план реализации.

**Проделанная работа:**
- Изучение 30+ JSON-RPC методов Codex-протокола фронтенда
- Анализ Claude CLI `--output-format stream-json` вывода
- Маппинг событий: `system` → `codex/connected`, `content_block_*` → `item/*`, `result` → `turn/completed`
- Составление 4-фазного плана реализации

**Проблема:** Claude CLI и Codex используют принципиально разные протоколы. Codex — двунаправленный JSON-RPC через stdin/stdout. Claude CLI — однонаправленный поток NDJSON-событий (на тот момент).

**Решение:** Паттерн request interceptor — перехватываем все JSON-RPC запросы от фронтенда на уровне `WorkspaceSession`, транслируем в Claude-формат, а ответы Claude транслируем обратно в Codex JSON-RPC уведомления.

---

### Phase 1: Базовая интеграция (5 марта)

**Коммит:** `b9078bc`

**Что сделано:**
- Создан модуль `claude_bridge/` с тремя файлами: `types.rs`, `event_mapper.rs`, `process.rs`
- `ClaudeEvent` enum — десериализация всех типов событий Claude CLI (`system`, `assistant`, `content_block_*`, `message_*`, `result`)
- `map_event()` — трансляция Claude событий в Codex JSON-RPC уведомления
- `spawn_claude_session()` — spawn `claude --print`, interceptor для JSON-RPC
- Добавлен `request_interceptor` в `WorkspaceSession` (единственное изменение в `backend/app_server.rs`)
- `BackendMode::Claude` в `types.rs`

**Маппинг событий Phase 1:**
| Claude CLI | Codex JSON-RPC |
|---|---|
| `system` (init) | `codex/connected` + `thread/started` |
| `content_block_start` (text) | `item/started` (agentMessage) |
| `content_block_delta` (text) | `item/agentMessage/delta` |
| `content_block_start` (thinking) | `item/started` (reasoning) |
| `content_block_delta` (thinking) | `item/reasoning/textDelta` |
| `content_block_stop` | `item/completed` |
| `message_delta` (usage) | `thread/tokenUsage/updated` |
| `result` | `turn/completed` + `thread/name/updated` |

**Проблема:** `WorkspaceSession` требует `ChildStdin` и `Child` — но наш interceptor перехватывает все сообщения, stdin реального Claude-процесса управляется отдельно.

**Решение:** Spawn "dummy" процесс (`cmd /c exit 0` на Windows) чтобы получить `ChildStdin` и `Child` для конструктора `WorkspaceSession`. Dummy stdin никогда не используется — все записи идут через interceptor → координатор → реальный Claude stdin.

---

### Phase 2: Инструменты (Tool Execution & Item Management) (5 марта)

**Коммит:** `943a5c8`

**Что сделано:**
- Новый модуль `item_tracker.rs` — классификация и трекинг инструментов
- `ToolCategory` enum: `CommandExecution`, `FileChange`, `FileRead`, `Other`
- `classify_tool()` — маппинг имён инструментов в категории (bash, Write, Edit, Read, Glob, Grep, etc.)
- `ItemInfo` — состояние инструмента на протяжении жизненного цикла
- Обогащённые `item/started` и `item/completed` с display-полями
- Потоковый `input_json_delta` → накопление JSON → извлечение `command`, `path`

**Проблема:** Claude CLI не отправляет отдельных "tool result" событий в потоке. Результаты инструментов приходят как `content_block_start` с типом `tool_result`, а не как отдельный тип события.

**Решение:** Маппинг `ContentBlock::ToolResult` в `map_content_block_start()` — при получении `tool_result` находим соответствующий `ItemInfo` по `tool_use_id` и эмитим `outputDelta`.

**Проблема:** Параметры инструмента (например, команда bash) приходят как streaming JSON-дельты (`input_json_delta`), а не целиком.

**Решение:** Накопление `partial_json` в `ItemInfo.accumulated_input_json`, парсинг при `content_block_stop` для извлечения display-полей (`command`, `path`).

---

### Phase 3: UI для выбора режима (5 марта)

**Коммит:** `eac6f56`

**Что сделано:**
- Dropdown "Backend mode" в Settings → Server: Codex / Claude CLI / Remote
- Проверка установки Claude CLI (`claude --version`)
- Статус и кнопка "Re-check" в настройках
- Текст помощи: "Requires `claude` CLI installed and authenticated"

---

### Phase 4: Модель, стоимость, лимиты (5 марта)

**Коммит:** `758ff4f`

**Что сделано:**
- Кумулятивные счётчики в `BridgeState`: `total_input_tokens`, `total_output_tokens`, `total_cost_usd`
- `thread/tokenUsage/updated` с `last`/`total` разбивкой
- `turn/completed` с `costUsd` и `durationMs`
- `account/rateLimits/updated` — показ кумулятивной стоимости
- `model/list` — динамический список из обнаруженной модели
- `format_model_display_name()` — "claude-sonnet-4-20250514" → "Claude Sonnet 4"

---

### Phase 5: Тесты (5 марта)

**Коммит:** `1f2af29`

**Что сделано:** 75+ новых тестов по всем модулям:
- `types.rs`: десериализация всех типов событий, жизненный цикл `BridgeState`
- `event_mapper.rs`: edge cases, извлечение текста из tool_result, context window
- `process.rs`: полное покрытие interceptor для всех JSON-RPC методов

Итого: 138 тестов `claude_bridge` проходят.

---

### Phase 6: Remote daemon mode (5 марта)

**Коммит:** `12a101b`

**Что сделано:**
- Маршрутизация daemon-сессий через Claude CLI при `backend_mode = "claude"`
- `DaemonState` проверяет `use_claude` из настроек перед создданием сессии

---

### UI: Бейджи и цвета (5 марта)

**Коммит:** `edbb70b`

**Что сделано:**
- `BackendModeBadge` компонент — визуальный индикатор режима (зелёный Codex, оранжевый Claude, синий Remote)
- Бейджи моделей в строках тредов — цвета по бренду (Anthropic оранжевый, GPT фиолетовый, Gemini синий)

---

### Загрузка истории Claude CLI сессий (16 марта)

**Коммит:** `7c91480`

**Что сделано:**
- Парсинг JSONL-файлов из `~/.claude/projects/<encoded-path>/`
- `ClaudeSession` — структура сессии (session_id, name, last_active_ms)
- `read_claude_sessions()` — сканирование директории, сортировка по дате
- `read_session_items()` — парсинг JSONL в Codex-совместимые items для отображения в чате
- `encode_workspace_path()` — кодирование пути в формат Claude CLI (замена `\`, `/`, `:`, ` ` на `-`)
- `thread/list` возвращает историю сессий
- `thread/resume` загружает items из JSONL-файла сессии

**Проблема:** Claude CLI кодирует пути проектов специфичным образом: `D:\Projects\MyApp` → `D--Projects-MyApp`. Пробелы тоже заменяются дефисами.

**Решение:** Функция `encode_workspace_path()` реплицирует логику кодирования CLI.

**Проблема:** Фронтенд при переключении режима или фокусе окна вызывал `thread/list` с batched-запросом по всем workspace. В Claude-режиме каждый workspace должен иметь свою изолированную историю.

**Решение:** Отключение session sharing в Claude-режиме. Per-workspace listing в `thread/list`, `thread/resume`, и при переключении режима. Очистка active thread при смене backend mode.

---

### Динамическое обнаружение моделей (16 марта)

**Коммит:** `ff9370c`

**Что сделано:**
- `discover_models()` — сканирование всех JSONL-сессий на предмет использованных моделей
- Удалён захардкоженный список `KNOWN_CLAUDE_MODELS`
- Raw model IDs без трансформации (поддержка любых моделей, включая custom/llama)

**Проблема:** Первоначально был захардкожен список Claude-моделей. Это не работало с пользовательскими моделями и моделями от сторонних провайдеров.

**Решение:** Сканирование поля `model` во всех существующих JSONL-сессиях. Каждая уникальная модель попадает в `model/list`. Если сессий нет — fallback на `claude-sonnet-4-6`.

**Коммит:** `a994910` — исправления после рефакторинга (сигнатуры тестов, fallback ID).

---

### Совместимость с CLI 2.1.77 и потоковые события (17 марта)

**Коммит:** `4098d83`

**Что сделано:**
- Флаги `--verbose` и `--include-partial-messages` (стали обязательными в новых версиях CLI)
- `StreamEvent` wrapper — CLI стал оборачивать события в `{"type": "stream_event", "event": {...}}`
- `RateLimitEvent` — новый тип события (игнорируется)
- Tool calls в истории чата (двухпроходный парсинг JSONL)
- Pass-through модели из UI dropdown в `claude --model`
- Crash recovery: синтетический `turn/completed` при неожиданном завершении процесса

**Проблема:** Claude CLI 2.1.77 изменил формат вывода при `--include-partial-messages`. Вместо прямых `message_start`, `content_block_delta` и т.д. события стали приходить обёрнутыми в `stream_event`:

```json
// Было (старые версии):
{"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}}

// Стало (2.1.77+):
{"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}}}
```

**Решение:** Добавлен `StreamEventWrapper` в `ClaudeEvent` и `map_stream_event()` в event_mapper — разворачивает внутреннее событие и рекурсивно вызывает `map_event()`.

**Проблема:** Дублирование `codex/connected` при каждом ходе. Event mapper эмитил `codex/connected` при получении `system` init, но это происходило при каждом spawn-per-turn, вызывая `reconnectLive()` во фронтенде и сброс состояния тредов.

**Решение:** `codex/connected` эмитится однократно в `spawn_claude_session()`. Из `map_system()` убрана эмиссия connected.

**Проблема:** При крэше процесса Claude (OOM, сетевая ошибка, kill) фронтенд зависал в состоянии "processing" навсегда.

**Решение:** Флаг `turn_completed` в координаторе. Если `turn_started == true` но `turn_completed == false` при завершении процесса — эмитится синтетический `turn/completed` со статусом `"error"`.

**Проблема:** Двухпроходный парсинг JSONL для отображения tool calls в истории. В JSONL-файлах Claude tool_use и tool_result находятся в разных строках (messages). Нужно сначала собрать все tool_result, затем при обработке tool_use найти соответствующий результат.

**Решение:** `read_session_items()` делает два прохода: (1) собирает map `tool_use_id → result_text`, (2) при обходе content blocks для tool_use находит результат и строит `commandExecution`/`fileChange` items.

---

### Persistent Process: Permission Prompts и AskUserQuestion (18 марта)

**Коммит:** `f0ded82` — Fix тестов item_tracker.
**Коммит:** `dfb5c51` — Основная реализация.

**Что сделано:**

Полная замена архитектуры spawn-per-turn на persistent process с двунаправленным NDJSON-протоколом.

#### Новые типы (`types.rs`)
- `ClaudeEvent::ControlRequest` — десериализация `control_request` от CLI
- `ControlRequestData` — `request_id` + `request` payload
- `PendingControlRequest` — хранение ожидающих одобрения (claude_request_id, tool_name, request)
- `BridgeState` расширен: `pending_control_requests: HashMap<u64, PendingControlRequest>`, `approval_id_counter` (начинается с 100 000), `claude_session_id`

#### Маппинг control_request (`event_mapper.rs`)
- `map_control_request()` для `subtype == "can_use_tool"`:
  - `AskUserQuestion` → `item/tool/requestUserInput` (модальное окно)
  - Любой другой инструмент → `codex/requestApproval` (тост одобрения)
- Генерация уникальных numeric ID для корреляции (100000, 100001, ...)
- Сохранение `PendingControlRequest` в `BridgeState` для обратной корреляции

#### Новая архитектура процесса (`process.rs`)

**Удалено:** `TurnRequest`, `SessionArg`, `run_claude_turn()`.

**Добавлено:**

```
spawn_claude_session()
  └─ claude --print --input-format stream-json --output-format stream-json --verbose --include-partial-messages
       ├─ stdin ← stdin_writer_task (mpsc channel)
       ├─ stdout → stdout_reader_task (NDJSON parser → event_mapper → event_sink)
       └─ interceptor (JSON-RPC ↔ Claude NDJSON protocol)
```

- **`StdinMessage` enum:** `UserMessage { text, uuid }`, `ControlResponse(String)`, `Interrupt`
- **`stdin_writer_task()`** — tokio task, единственный владелец `ChildStdin`, получает сообщения через `mpsc::unbounded_channel`
- **`stdout_reader_task()`** — tokio task, читает NDJSON из stdout, парсит `ClaudeEvent`, вызывает `event_mapper::map_event()`, эмитит Codex-события
- **Interceptor** обрабатывает:
  - `turn/start` / `turn/steer` → отправляет `StdinMessage::UserMessage` через канал
  - `turn/interrupt` → отправляет `StdinMessage::Interrupt`
  - Ответы на approval (result без method, с numeric ID) → находит `PendingControlRequest`, строит `control_response` NDJSON, отправляет через канал
  - Все остальные методы — как раньше

#### NDJSON-хелперы

- `build_user_message(text, uuid)` — формат: `{"type":"user","message":{"role":"user","content":"..."},"uuid":"..."}`
- `build_interrupt_request()` — формат: `{"type":"control_request","request_id":"...","request":{"subtype":"interrupt"}}`
- `build_control_response(pending, result)` — dispatches to:
  - `build_tool_approval_response()` — `{"type":"control_response","response":{"behavior":"allow"|"deny",...}}`
  - `build_ask_user_response()` — `{"type":"control_response","response":{"behavior":"allow","updatedInput":{"questions":...,"answers":...}}}`

**Проблема:** Reverse engineering двунаправленного протокола Claude CLI. Официальная документация описывает только однонаправленный `--output-format stream-json`. Протокол `--input-format stream-json` нигде не документирован.

**Решение:** Экспериментальное исследование исходного кода CLI (bundled `cli.js` в npm-пакете `@anthropic-ai/claude-code@2.1.77`). Результат задокументирован в `docs/claude-cli-stream-json-protocol.md` — полная спецификация: user messages, control_request/control_response, interrupt, keep_alive, update_environment_variables.

**Проблема:** Interceptor вызывается синхронно (из `write_message` в `app_server.rs`), но запись в stdin — асинхронная операция. Interceptor не может напрямую писать в `ChildStdin`.

**Решение:** `mpsc::unbounded_channel<StdinMessage>` — interceptor отправляет сообщения в канал (синхронная операция), отдельный `stdin_writer_task` (tokio task) читает из канала и пишет в stdin (асинхронно). Разделение владения: interceptor держит `Sender`, stdin_writer_task — `Receiver` + `ChildStdin`.

**Проблема:** Корреляция approval-ответов от фронтенда с Claude CLI request_id. Фронтенд отправляет ответ с numeric `id` (u64), а Claude CLI ожидает string `request_id`.

**Решение:** Двухуровневая корреляция. При получении `control_request` от CLI:
1. Генерируем numeric `approval_id` (из `approval_id_counter`, начиная с 100 000 чтобы не конфликтовать с обычными JSON-RPC ID)
2. Сохраняем `PendingControlRequest { claude_request_id, tool_name, request }` в `pending_control_requests[approval_id]`
3. Эмитим Codex-событие с `"id": approval_id`

При получении ответа от фронтенда (value с `result` и без `method`):
1. Извлекаем `id` как `u64`
2. Находим `PendingControlRequest` в `pending_control_requests`
3. Строим `control_response` NDJSON с `request_id` из pending
4. Отправляем через канал в stdin

---

## 4. Маппинг протоколов: Полная таблица

| Claude CLI событие | Codex JSON-RPC | Примечания |
|---|---|---|
| `system` (init) | `thread/started` | `codex/connected` эмитится отдельно, один раз |
| `stream_event` → `message_start` | `turn/started` | Только первый раз за turn |
| `stream_event` → `content_block_start` (text) | `item/started` (agentMessage) | |
| `stream_event` → `content_block_delta` (text_delta) | `item/agentMessage/delta` | Стриминг текста |
| `stream_event` → `content_block_start` (thinking) | `item/started` (reasoning) | |
| `stream_event` → `content_block_delta` (thinking_delta) | `item/reasoning/textDelta` | Стриминг мыслей |
| `stream_event` → `content_block_start` (tool_use) | `item/started` (commandExecution/fileChange) | Категория по `classify_tool()` |
| `stream_event` → `content_block_delta` (input_json_delta) | `item/commandExecution/outputDelta` | Накопление input JSON |
| `stream_event` → `content_block_start` (tool_result) | `item/commandExecution/outputDelta` | Результат инструмента |
| `stream_event` → `content_block_stop` | `item/completed` | Обогащённый item с command/changes |
| `stream_event` → `message_delta` (usage) | `thread/tokenUsage/updated` | |
| `assistant` (snapshot) | *(ignored, model extracted)* | С `--include-partial-messages` — дубликат |
| `control_request` (can_use_tool) | `codex/requestApproval` | Permission prompt |
| `control_request` (AskUserQuestion) | `item/tool/requestUserInput` | Интерактивный вопрос |
| `result` (success) | `turn/completed` + `thread/name/updated` + `account/rateLimits/updated` | Кумулятивная стоимость |
| `result` (error) | `error` + `turn/completed` (status: error) | |
| `rate_limit_event` | *(ignored)* | |

---

## 5. Фронтенд-интеграция

Фронтенд **не требовал модификации** для поддержки новых возможностей persistent process (permission prompts, AskUserQuestion) — эти компоненты уже существовали в кодовой базе Codex GUI.

### Существующие компоненты (переиспользованы)
- **ApprovalToasts** (`ApprovalToasts.tsx`) — toast-уведомления для `codex/requestApproval`
- **RequestUserInputMessage** (`RequestUserInputMessage.tsx`) — модальное окно для `item/tool/requestUserInput`
- **Обработчики** в `useThreads.ts`: `handleApprovalDecision`, `handleUserInputSubmit`

### Добавленные компоненты (Phase 1–6)
- **BackendModeBadge** — визуальный бейдж режима (Codex/Claude/Remote)
- **SettingsServerSection** — dropdown выбора backend mode + проверка Claude CLI
- **Model brand colors** — цветовая маркировка моделей в строках тредов

### Изменённые файлы фронтенда
23 файла, 534 добавлений, 42 удалений. Основные:
- `App.tsx` — wiring approval/userInput handlers
- `SettingsServerSection.tsx` — Claude CLI backend mode option
- `useWorkspaceRestore.ts` — per-workspace session restoration
- `useWorkspaceRefreshOnFocus.ts` — session isolation
- `useThreadMessaging.ts` — backend mode parameter passing
- `sidebar.css` — стили для backend mode badges

---

## 6. Тестирование

### Покрытие тестами
| Модуль | Тестов | Покрытие |
|---|---|---|
| `types.rs` | 22 | Десериализация всех типов событий, BridgeState lifecycle |
| `event_mapper.rs` + `event_mapper_tests.rs` | 50+ | Все event mappings, edge cases, control_request, stale result detection |
| `item_tracker.rs` | 25+ | Классификация, build_item_*, output deltas, heuristic classification |
| `process.rs` | 35+ | Interceptor, NDJSON builders, extract_user_text, group_items_into_turns |
| `history.rs` | 35+ | Session loading, path encoding, JSONL parsing, discover_models, format_model_name |
| **Итого** | **~170+** | Все проходят (393 всего в проекте) |

### Что тестируется
- Десериализация каждого типа Claude CLI события (system, assistant, content_block_*, message_*, result, control_request)
- Маппинг событий в Codex JSON-RPC (корректные method, params, threadId, turnId)
- Полный жизненный цикл инструментов (started → delta → completed)
- Interceptor: все 30+ JSON-RPC методов (initialize, turn/start, thread/list, model/list, etc.)
- NDJSON-билдеры: user_message, interrupt, control_response (allow/deny), AskUserQuestion
- Накопление кумулятивных метрик (tokens, cost)
- Edge cases: пустые сообщения, отсутствующие поля, невалидный JSON

---

## 7. Принцип Open-Closed

Все изменения за исключением минимальных точек интеграции остались **внутри** `claude_bridge/`:

| Файл вне claude_bridge | Изменений | Назначение |
|---|---|---|
| `backend/app_server.rs` | +35 строк | Добавление `request_interceptor` в `WorkspaceSession` |
| `codex/mod.rs` | +19 строк | Маршрутизация на `spawn_claude_session()` при `BackendMode::Claude` |
| `types.rs` | +4 строки | `BackendMode::Claude` вариант |
| `shared/workspaces_core/connect.rs` | +27 строк | Session isolation в Claude-режиме |

Итого: **85 строк** изменений вне модуля vs **5154 строки** внутри.

---

## 8. Известные ограничения

1. **Нет `--resume` / `--continue`** в persistent process mode — каждая сессия начинается с чистого контекста. История загружается из JSONL для отображения, но процесс не возобновляет старую сессию.

2. **Нет смены модели mid-session** — модель фиксируется при запуске процесса. Для смены нужен рестарт.

3. **Нет `--session-id`** — persistent process не привязывается к конкретной сессии. Claude CLI сам создаёт и управляет session ID.

4. ~~**Windows-специфичный dummy process**~~ — **Исправлено** в `ef30fb3`: `#[cfg(windows)]` cmd /c exit 0, `#[cfg(not(windows))]` true.

---

## Аудит кодовой базы (18 марта 2026)

Систематический trace-based аудит всех модулей claude_bridge. Методология: чтение кода → трассировка путей выполнения → выявление багов → исправление с тестами → коммит.

### Event Mapper (`ec67ccb`)
- Stale result detection: `result` событие от предыдущего turn'а обрабатывалось как текущее
- `content_block_stop` для tool_result блоков генерировало лишнее `item/completed`
- 8 новых тестов, тесты вынесены в `event_mapper_tests.rs`

### History / Thread Resume (`b0748fe`, `6950a22`)
- `thread/list` возвращал устаревшие сессии — теперь обновляет при каждом запросе
- O(n²) `collect_assistant_text` → O(n) через HashMap группировку
- `read_session_name_from_jsonl` падала при I/O ошибке одной строки
- `thread/resume` возвращал все items одним turn'ом → `group_items_into_turns`
- `discover_models` мог сканировать неограниченное число файлов — добавлены лимиты

### Item Tracker (`bb38e1d`, `858288f`)
- Поле `command` было пустым для инструментов кроме Bash — добавлены описания для Read, Grep, Glob, WebFetch, WebSearch
- Эвристическая классификация для MCP/unknown инструментов: `infer_command_from_input`, `infer_category_from_input`

### Process Lifecycle (`ef30fb3`)
- Дубликат `format_model_display_name` → удалён, используется `format_model_name` из history.rs
- При смерти процесса между turns не было уведомления → добавлен `codex/disconnected`
- Dummy child `cmd /c exit 0` не работал на Unix → cross-platform через `#[cfg]`
- Лишняя аллокация String в `stdin_writer_task` → `into_bytes()` + `push(b'\n')`

---

## 9. Файловая структура проекта (Claude-related)

```
docs/
├── claude-cli-integration-analysis.md      # Начальный анализ (Phase 0)
├── claude-cli-stream-json-protocol.md      # Полная спецификация протокола
└── claude-bridge-implementation-history.md # Этот документ

src-tauri/src/
├── claude_bridge/
│   ├── mod.rs                              # Публичный API
│   ├── types.rs                            # ClaudeEvent, BridgeState, ControlRequestData
│   ├── event_mapper.rs                     # Claude → Codex event mapping
│   ├── event_mapper_tests.rs               # Тесты event_mapper
│   ├── item_tracker.rs                     # Tool classification & lifecycle
│   ├── process.rs                          # Process management, interceptor, NDJSON
│   └── history.rs                          # JSONL session loading, model discovery
├── backend/app_server.rs                   # +request_interceptor в WorkspaceSession
├── codex/mod.rs                            # Маршрутизация на Claude bridge
└── types.rs                                # BackendMode::Claude

src/
├── features/settings/.../SettingsServerSection.tsx  # Claude CLI UI
├── features/app/components/BackendModeBadge.tsx     # Mode badges
└── ... (23 файлов, 534 строк изменений)
```
