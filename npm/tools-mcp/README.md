# tools-mcp

Install the platform-native local worker:

```sh
npm install --global tools-mcp
```

Create the device key and certificate during the one-time enrollment, then save
the relay settings with `tools-mcp setup`. On macOS, `setup --device-key`
selects a user-only PEM key and avoids Keychain authorization prompts. Without
that option the launcher uses the non-exportable Keychain key; Windows uses
CNG. Daily startup is intentionally a bare command from the project directory:

```sh
cd path/to/project
tools-mcp
```

The process fixes that launch directory as its local workspace. Stop it with
Ctrl-C to return new ChatGPT calls to the VPS fallback.
