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

Type `/model` to open the model picker immediately. You can also type `/` and press Tab. Use ↑ or ↓ to highlight a model from the current provider. Press Enter to select it or Esc to keep the current model. The next message uses that model with the same conversation history. Each session has its own selection.

Press Shift+Enter to add a line. If your terminal sends the same key code for Enter and Shift+Enter, use Ctrl+J or Alt+Enter. Press Enter to send the whole message. Esc cancels the current turn, Page Up and Page Down scroll the conversation, and Ctrl+C quits. Each response appears when the provider finishes. Conversation history stays in the server process until it exits.

## Connect a desktop client

Start `elphie serve` as a child process. Use the same provider and model flags as the terminal. Exchange one JSON-RPC message per line on its standard streams.

1. Send `initialize` with `protocolVersion: 2`, client information, and client capabilities.
2. Send `session/new` with an absolute working directory. Keep the returned `sessionId`.
3. Send `session/prompt` with that ID and text content. The response identifies the accepted user message.
4. Apply `session/update` notifications to the conversation in receive order. Match each message by role and ID. A `state_update` with state `idle` tells the UI that foreground work has ended.
5. Send `session/cancel` to stop current work. Send `session/close` when the view closes.

To show Elphie’s icon, read `info._meta["io.github.thiskevinwang.elphie/icon"]` during initialization. The agent and terminal use this extension to send an ACP `Icon` with a 192 × 192 WebP data URI in `src`. Clients can display it offline. [ACP metadata extensions](https://agentclientprotocol.com/protocol/v2/extensibility)

To enable `/model`, advertise `"elicitation": {"form": {}}` in the client capabilities. When the user sends the command, display the model choices from `elicitation/create`. Return the selected model ID in `content.model` with the `accept` action, or return `cancel` to keep the current model. [ACP form elicitation](https://agentclientprotocol.com/protocol/v2/elicitation)

The server also supports listing sessions, resuming them with transcript replay, and retrying submissions with an idempotency key. Resource links enter the prompt as a name and URI. This release supports text conversations and requests no filesystem, shell, or MCP tool permissions. ACP v2 is a draft. The project pins its Rust SDK version. [ACP v2](https://agentclientprotocol.com/protocol/v2/overview) · [Rust SDK](https://agentclientprotocol.com/libraries/rust)

## Code map

| Component | Responsibility |
| --- | --- |
| `src/terminal.rs` | Draw the terminal and capture keyboard input. |
| `src/view_model.rs` | Manage the draft, message edits, and turn status independently of the terminal. |
| `src/client.rs` | Convert UI actions to ACP requests and ACP updates to view-model events. |
| `elphie-core` | Define provider-independent messages and the provider interface. |
| `elphie-acp` | Own session history, prompt ordering, cancellation, and replay. |
| `elphie-providers` | Send conversation history to ChatGPT or Ollama. |
| `elphie-signin` | Register Elphie with ChatGPT and manage its account credentials. |
| `elphie-oauth` | Authorize a host to access an MCP resource with the official Rust MCP SDK. |

To add a model provider, implement `elphie_core::Provider` and select the implementation in `src/main.rs`. The terminal and ACP session code use the same interface for both providers.

For MCP authorization, supply the URL of a hosted Client ID Metadata Document (CIMD), use dynamic registration, or use an existing public client registration. The host supplies the browser and callback listener. The library returns an authorized rmcp manager. [MCP sign-in example](crates/elphie-oauth/README.md)

## Verify

```sh
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked
python3 tests/terminal.py
```

The last check needs a Unix pseudo-terminal. It opens the picker by typing `/model`, sends a multiline message to the selected model, checks the conversation history, and cancels both a selection and a prompt against a local HTTP fixture.

To run the upstream ACP suite with the same local fixture, install the tested revision of the kit and run the following commands. The report lists exercised and skipped requirements. [ACP compatibility kit](https://github.com/agentclientprotocol/acp-tck/tree/7c71c4d1af0994e73bdc100c9f7f6b98f2319cda)

```sh
uv tool install git+https://github.com/agentclientprotocol/acp-tck@7c71c4d1af0994e73bdc100c9f7f6b98f2319cda
python3 tests/conformance.py
```
