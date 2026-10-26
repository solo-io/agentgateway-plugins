# Security Policy

## Reporting a Vulnerability

To report a vulnerability, [file a private vulnerability report](https://github.com/solo-io/agentgateway-plugins/security/advisories/new).

**Please do not report security vulnerabilities through public GitHub issues.**
If you aren't sure whether an issue is a vulnerability, report it privately.

## Scope

Report issues here if they are in:

- the plugin ABI (`agentgateway_abi.h`, `rust/abi`)
- the plugin SDK (`rust/sdk`)
- a plugin in `plugins/`
- how the gateway loads or runs plugins (we'll route it to the right team)

## Support

How we handle a report depends on the plugin's status (see the
[README](./README.md#plugins)):

| Status | Handling |
| --- | --- |
| ABI and SDK | Triaged and fixed by Solo.io. |
| **Solo.io maintained** | Triaged and fixed by Solo.io. |
| **Community maintained** | Best effort, by the plugin's maintainers. |
| **Demo** | Best effort. Demos are examples, not production plugins. |

Please report issues in any plugin. Best effort still means we want to know.

## Plugins are trusted code

Plugins run inside the gateway process as native code. They are not sandboxed.
A plugin that is malicious, or that you configured to do something unsafe, can
affect the gateway, and is not a vulnerability in agentgateway or the SDK. A bug
in the SDK or in a plugin from this repository that lets a client cause harm
through normal requests, such as crashing the gateway, is.
