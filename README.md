# Elphie

<img src="crates/elphie-acp/assets/elphie.webp" width="96" height="96" alt="Elphie, a black and white cat">

Elphie is a text chat agent written in Rust. The terminal opens a conversation with an Elphie server over standard input and output. A desktop client can start the same server and use the same ACP v2 messages.

## Start a conversation

Build with Rust 1.90 or later. For ChatGPT, sign in once with an eligible account and grant Elphie permission to use the plan.

```sh
cargo install --locked --path .
elphie login
elphie
```

Sign-in opens the system browser. Elphie saves the registration in `~/.elphie` and refreshes its token when needed. Set `ELPHIE_CONFIG_DIR` to the absolute path of a different private directory. To keep using a registration from an earlier version, set this variable to its existing directory. Run `elphie logout` to sign out of the active registration. [ChatGPT plan authorization](https://developers.openai.com/siwc/token-sharing-open-source)

The default model is `gpt-6.1-sol`. Use `elphie --model <model>` to select another model available to the account. For Ollama, start the local service. Substitute an installed model name from `ollama list` in this example.

```sh
elphie --provider ollama --model llama3.2
```

Type `/model` to open the model picker immediately. Press `/` in the picker to search by model name or ID. Use ↑ or ↓ to highlight a match, then press Enter to select it. Esc clears the search and Esc again closes the picker. The next message uses the selected model with the same conversation history.

Press Shift+Enter to add a line. If your terminal sends the same key code for Enter and Shift+Enter, use Ctrl+J or Alt+Enter. Press Enter to send the whole message. Esc cancels the current turn, Page Up and Page Down scroll the conversation, and Ctrl+C quits. Each response appears when the provider finishes. Conversation history stays in the server process until it exits.

With an empty draft, press ↑ to recall a prompt from this session or `?` to show keyboard shortcuts. Press `?` again to start a message with a question mark. You can edit a recalled prompt before sending it. In a multiline draft, ↑ and ↓ move between lines.

## Give Elphie read access to a directory

```sh
elphie --tools .
```

Ask “Read README.md and summarize the install steps.” Elphie can call `list_directory` and `read_file`. The terminal shows the read-only directory and the status of each call. **The selected model provider receives directory listings and file contents.** Choose a directory on a trusted filesystem whose contents you can share. Git-ignored files are readable if they meet the same access rules. For Ollama, select a model that supports tools.

File tools are off by default. The directory passed to `--tools` sets the read boundary for every session on that server. A model request or a client's working directory cannot expand it. File tools accept relative paths, skip dotfiles and reject symbolic links, multiply linked files, and special files. They cannot run commands, change files, or make network requests. This scope follows the principle of enforcing access limits outside the model. [Inside the Codex sandbox](https://openai.com/index/inside-the-codex-sandbox/)

A prompt can make up to eight tool calls. Each call returns up to 32 KiB. File reads require UTF-8 text. Directory listings return up to 256 visible entries and stop with an error if the directory exceeds the scan limit of 1,024 entries. Tool access requires macOS with `O_NOFOLLOW_ANY` or Linux with `openat2`. Linux rejects kernel filesystems such as `/proc` and `/sys` as tool directories.

## Connect a desktop client

Start `elphie serve` as a child process. Use the same provider and model flags as the terminal. Exchange one JSON-RPC message per line on its standard streams.

1. Send `initialize` with `protocolVersion: 2`, client information, and client capabilities.
2. Send `session/new` with an absolute working directory. Keep the returned `sessionId`.
3. Send `session/prompt` with that ID and text content. The response identifies the accepted user message.
4. Apply `session/update` notifications to the conversation in receive order. Match each message by role and ID. A `state_update` with state `idle` tells the UI that foreground work has ended.
5. Send `session/cancel` to stop current work. Send `session/close` when the view closes.

To show Elphie’s icon, read `info._meta["io.github.thiskevinwang.elphie/icon"]` during initialization. The agent and terminal use this extension to send an ACP `Icon` with a 192 × 192 WebP data URI in `src`. Clients can display it offline. [ACP metadata extensions](https://agentclientprotocol.com/protocol/v2/extensibility)

To enable `/model`, advertise `"elicitation": {"form": {}}` in the client capabilities. When the user sends the command, display the model choices from `elicitation/create`. Return the selected model ID in `content.model` with the `accept` action, or return `cancel` to keep the current model. [ACP form elicitation](https://agentclientprotocol.com/protocol/v2/elicitation)

Start `elphie serve --tools ./project` to grant the same read access for a desktop client. Apply each `tool_call_update` to its `toolCallId`. A later update changes the same transcript row. The server sends the path, status, and result size or error. File contents go to the model. [ACP tool calls](https://agentclientprotocol.com/protocol/v2/tool-calls)

The server also supports listing sessions, resuming them with transcript replay, and retrying submissions with an idempotency key. Resource links enter the prompt as a name and URI. ACP v2 is a draft. The project pins its Rust SDK version. [ACP v2](https://agentclientprotocol.com/protocol/v2/overview) · [Rust SDK](https://agentclientprotocol.com/libraries/rust)

## Code map

| Component | Responsibility |
| --- | --- |
| `src/terminal.rs` | Translate terminal input into user intents and send the resulting actions to the client. |
| `src/terminal/view.rs` | Draw the transcript, composer, picker, and shortcuts for the available screen size. |
| `elphie-view-model` | Apply user intents and agent events to the draft, prompt history, transcript, and picker. |
| `src/client.rs` | Send actions over ACP and translate replies into view-model events. Own pending form replies. |
| `elphie-core` | Define provider-independent messages and the provider interface. |
| `elphie-acp` | Own session history, tool execution order, cancellation, and replay. |
| `elphie-tools` | Define the workspace tools and enforce read limits through the operating system. |
| `elphie-providers` | Send conversation history to ChatGPT or Ollama. |
| `elphie-signin` | Register Elphie with ChatGPT and manage its account credentials. |
| `elphie-oauth` | Authorize a host to access an MCP resource with the official Rust MCP SDK. |

To add a model provider, implement `elphie_core::Provider` and select the implementation in `src/main.rs`. The terminal and ACP session code use the same interface for both providers.

For example, when the user presses Enter in the picker, the terminal sends a submit intent to the view model. The view model returns the selected model ID and form ID. The client answers the ACP request. The displayed model changes when the server confirms the selection. A desktop host can drive the same Rust view model through its public intents and events.

For MCP authorization, supply the URL of a hosted Client ID Metadata Document (CIMD), use dynamic registration, or use an existing public client registration. The host supplies the browser and callback listener. The library returns an authorized rmcp manager. [MCP sign-in example](crates/elphie-oauth/README.md)

## Verify

```sh
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked
python3 tests/terminal.py
```

The last check uses a pseudo-terminal and a local Ollama fixture. It covers model selection, multiline input, cancellation, and tool calls. It checks that a permitted file reaches the provider and that a symbolic link to another directory fails.

To run the upstream ACP suite with the same local fixture, install the tested revision of the kit and run the following commands. The report lists exercised and skipped requirements. [ACP compatibility kit](https://github.com/agentclientprotocol/acp-tck/tree/7c71c4d1af0994e73bdc100c9f7f6b98f2319cda)

```sh
uv tool install git+https://github.com/agentclientprotocol/acp-tck@7c71c4d1af0994e73bdc100c9f7f6b98f2319cda
python3 tests/conformance.py
```
