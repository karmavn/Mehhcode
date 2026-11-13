<p align="center">
  <a href="https://mehh-code.dev">
    <img alt="Mehh Code logo" src="https://mehh-code.dev/logo-auto.svg" width="128">
  </a>
</p>
<p align="center">
  <a href="https://discord.com/invite/3cU7Bz4UPx"><img alt="Discord" src="https://img.shields.io/badge/discord-community-5865F2?style=flat-square&logo=discord&logoColor=white" /></a>
  <a href="https://www.npmjs.com/package/@mehh-code/coding-agent"><img alt="npm" src="https://img.shields.io/npm/v/@mehh-code/coding-agent?style=flat-square&logo=npm&logoColor=white" /></a>
</p>

> New issues and PRs from new contributors are closed automatically. Maintainers review closed submissions daily. See [CONTRIBUTING.md](https://github.com/mehh-code/mehh-code/blob/main/CONTRIBUTING.md).

# Mehh Code

Mehh Code is a minimal, extensible agent harness that you can make your own.

Adapt mehh to your workflows, not the other way around. Customize mehh with [skills](docs/skills.md), [prompt templates](docs/prompt-templates.md), and [themes](docs/themes.md), and bundle them as [mehh packages](docs/packages.md) to share via npm or git. Built-in integrations (MCP, codemode, tool search, llama.cpp) are toggled in the `extensions` setting or with `mehh config`. Executable extensions are application code: pass them to the [TypeScript SDK](docs/sdk.md) as inline factory functions; the CLI does not load extension files. See [extensions](docs/extensions.md) for how extensions work.

Mehh ships with powerful defaults but skips features like sub-agents and plan mode. Ask mehh to build what you want, or install a package that does it your way.

Use mehh [interactively](docs/usage.md), automate it in [print or JSON mode](docs/cli.md), control it over [RPC](docs/rpc.md), or build apps with the [mehh TypeScript SDK](docs/sdk.md). See [OpenClaw](https://github.com/OpenClaw/OpenClaw) for a real-world integration.

## Getting started

Install the command-line interface:

```bash
curl -fsSL https://mehh-code.dev/install.sh | sh
```

On Windows:

```shell
powershell -c "irm https://mehh-code.dev/install.ps1 | iex"
```

The installer pins all dependencies and updates mehh with `mehh update`. Alternatively, install directly with npm, which does not pin transitive dependencies:

```bash
npm install -g --ignore-scripts @mehh-code/coding-agent
```

Mehh requires Node.js 22.19 or newer. The macOS, Linux, and Windows installers can install it if needed. Mehh does not require dependency lifecycle scripts for a normal npm installation.

On macOS and Linux, Nix users can install the latest release with `nix profile add github:mehh-code/mehh-code/stable`. See the [quickstart](docs/quickstart.md#1-install-mehh) for updating and pinning releases.

Start mehh in the directory where you want it to work:

```bash
cd /path/to/project
mehh
```

For a built-in AI provider, run `/login` inside mehh to connect a subscription or API key. Then give mehh a task.

See the [documentation](docs/index.md) for full setup and usage instructions.

## Development

Clone the repository, install its dependencies, and run mehh from source:

```bash
git clone https://github.com/mehh-code/mehh-code
cd mehh-code
npm install --ignore-scripts
./mehh-test.sh
```

`mehh-test.sh` can be called from any directory and preserves the caller's working directory.

Before submitting changes, run:

```bash
npm run check
./test.sh
```

Read [CONTRIBUTING.md](https://github.com/mehh-code/mehh-code/blob/main/CONTRIBUTING.md) before opening an issue or pull request. It defines the contribution gate, issue quality bar, and required checks. Read [AGENTS.md](https://github.com/mehh-code/mehh-code/blob/main/AGENTS.md) for repository-specific implementation, testing, dependency, and release rules.

## License

MIT
