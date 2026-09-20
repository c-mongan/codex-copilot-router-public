<h1 align="center">Codex Copilot Router</h1>

<p align="center">
  <strong>Native OpenAI and GitHub Copilot.<br>One Codex workspace.</strong>
</p>

<p align="center"><strong>Experimental</strong> · Unofficial · macOS</p>

<p align="center">
  <a href="docs/SETUP.md#build-and-prepare-a-fresh-installation"><strong>Get started →</strong></a>
  &nbsp;·&nbsp;
  <a href="docs/SETUP.md#context-defaults-and-optional-long-context">Context settings</a>
  &nbsp;·&nbsp;
  <a href="LICENSE">MIT licence</a>
</p>

<p align="center">
  <img src="docs/images/codex-model-picker.png" width="439" alt="Codex model picker showing native GPT-6 Astra alongside Copilot GPT-6 Astra">
</p>

A small local Rust router that adds Copilot models to the **real Codex App and CLI**. Codex keeps its tools, approvals, sandbox and conversation management.

- **Choose either provider.** Native models and `copilot/` models live side by side.
- **Keep requests intact.** Only the Copilot model alias changes; response streams pass through.
- **Separate access.** Model requests require a local key. Provider credentials stay separate, with no silent fallback.

<details>
<summary><strong>Reasoning controls stay in Codex</strong></summary>

<p align="center">
  <img src="docs/images/codex-reasoning-effort.png" width="441" alt="Codex reasoning-effort control set to High for Copilot GPT-5.6 Sol">
</p>

Reasoning effort and context size are separate settings. Available models and effort levels depend on your account and catalogue.

</details>

## Get started

**You’ll need:** macOS, Rust/Cargo, Python 3.11+, a compatible Codex installation and GitHub Copilot access. Keep your ChatGPT-backed Codex login for native models.

```sh
git clone https://github.com/c-mongan/codex-copilot-router-public.git
cd codex-copilot-router-public
install -d -m 700 "$HOME/.local/share/codex-code-router"
cargo install --path . --bins --locked --root "$HOME/.local/share/codex-code-router"
python3 scripts/prepare_setup.py --codex "$(command -v codex)"
```

The helper prepares private files. **It does not log in, start services or overwrite Codex settings.**

**[Finish setup in the guide →](docs/SETUP.md#build-and-prepare-a-fresh-installation)** Merge the private config fragment, sign in, then verify and enable your chosen models.

## Know the boundaries

**Experimental and unofficial.** Tested with Codex `0.154.0-alpha.6.2` on macOS—not guaranteed across every client, model or update. Copilot quotas and policies still apply.

- Codex-managed compaction completed in offline replay; provider-native Copilot compact/lite endpoints are not implemented.
- Keep it on a trusted machine. Tokens are private files, not Keychain entries; these controls do not protect against same-user malware.
- Never share tokens, generated config fragments or raw conversation logs.

[Compatibility and compaction](docs/SETUP.md#desktop-native-and-copilot-models-side-by-side) · [Security details](docs/SETUP.md#local-security-changes) · [Troubleshooting](docs/SETUP.md#verification-and-troubleshooting)

## Credit

An experimental fork of [Derek Pearson’s codex-code-router 1.0.0](https://github.com/dpearson2699/codex-code-router/tree/081fb248a7f7279829f195ddbd23fb9d899e5639), released under the [MIT licence](LICENSE). Not affiliated with OpenAI or GitHub.
