```text
██╗  ██╗██████╗ ███████╗███╗   ██╗ ██████╗██╗  ██╗
██║  ██║██╔══██╗██╔════╝████╗  ██║██╔════╝██║  ██║
███████║██████╔╝█████╗  ██╔██╗ ██║██║     ███████║
██╔══██║██╔══██╗██╔══╝  ██║╚██╗██║██║     ██╔══██║
██║  ██║██████╔╝███████╗██║ ╚████║╚██████╗██║  ██║
╚═╝  ╚═╝╚═════╝ ╚══════╝╚═╝  ╚═══╝ ╚═════╝╚═╝  ╚═╝
```

# hbench

hbench is the benchmark engine behind [Harness Playground](https://nodeq.tech), by node-q.

Run coding agents (Claude Code, Codex, Gemini CLI and more) on benchmark tasks, and see how many they solve.

- [Installation](#installation)
- [Quick start](#quick-start)
- [Docs](#docs)
- [Usage](#usage)
- [Agents](#agents)
- [License](#license)
- [Contributors and contact](#contributors-and-contact)

## Installation

You need Docker, on Linux or macOS.

```bash
curl -fsSL https://nodeq.tech/install.sh | sh
```

Later, `hbench update` installs the newest release.

Or build from source:

```bash
cargo install --path .
```

## Quick start

```bash
hbench run     # pick a dataset, tasks, agent and model
hbench view    # see the results in your browser
```

To run Claude Code, set your Anthropic key first, then pick `claude-code`:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
hbench run
```

Other agents need their own key (see [Agents](#agents)). Every run is saved in `jobs/`.

## Docs

The full docs are at [nodeq.tech/docs](https://nodeq.tech/docs). Good places to start:

- [Run your first arena on your machine](https://nodeq.tech/docs/get-started/first-local-run)
- [Read your result](https://nodeq.tech/docs/get-started/read-your-result)
- [Use a local model server](https://nodeq.tech/docs/how-to/local-model-server)
- [Agents](https://nodeq.tech/docs/reference/agents) and the [hbench command line](https://nodeq.tech/docs/reference/hbench-cli)
- [Troubleshooting](https://nodeq.tech/docs/troubleshooting)

## Usage

```bash
# Run
hbench run                                                # interactive
hbench run -p examples/tasks/hello-world                  # a local task
hbench run -d terminal-bench@2.0.1 \
  -a claude-code -m anthropic/claude-opus-5-5             # a dataset, with Claude Code
hbench run -c job.yaml                                    # from a job config (YAML/JSON)

# Results
hbench view                                               # web viewer
hbench jobs summary -p jobs/<job>
hbench jobs resume -p jobs/<job>

# Tasks and datasets
hbench init <org/name>                                    # a new task
hbench tasks check -p <tasks>                             # validate tasks: config, files, tests
hbench check <tasks>                                      # review tasks against a rubric, with claude-code
hbench datasets list|download

# Other
hbench agents list                                        # every agent
hbench agents schema <agent>                              # an agent's options
hbench --help
```

Useful options: `-l 5` runs only 5 tasks, `-k 3` runs each task 3 times, `-n 4` runs 4 at a time.

## Agents

| Agent | What it runs | Example model (`-m`) | Key |
|---|---|---|---|
| `claude-code` | Claude Code | `anthropic/claude-opus-5-5` | `ANTHROPIC_API_KEY` |
| `codex` | OpenAI Codex CLI | `openai/gpt-6.1-sol` | `OPENAI_API_KEY` |
| `gemini-cli` | Google Gemini CLI | `google/gemini-3-pro` | `GEMINI_API_KEY` |
| `deepseek-harness` | DeepSeek's harness ([dsh](https://github.com/deepseek-ai/deepseek-harness)) | `deepseek/deepseek-v4-flash` | `DEEPSEEK_API_KEY` |
| `cursor-cli` | Cursor CLI | `<provider>/<model>` | `CURSOR_API_KEY` |
| `copilot-cli` | GitHub Copilot CLI | `<provider>/<model>` | `GITHUB_TOKEN` |
| `opencode` | opencode | `<provider>/<model>` | the provider's key |
| `aider` | Aider | `<provider>/<model>` | the provider's key |
| `openhands` | OpenHands | `<provider>/<model>` | the provider's key |
| `oracle` | Each task's reference solution, to check the tasks | | |
| … and a lot more | `hbench agents list` | | |

## License

Apache-2.0. See [LICENSE](LICENSE).

## Contributors and contact

hbench is built and maintained by [node-q](https://nodeq.tech). See the
[contributors graph](https://github.com/node-q/hbench/graphs/contributors) for everyone who has contributed.

- Questions and help: [#support on Discord](https://discord.com/channels/1562896153710690495/1563027997995442176)
- New releases: [#announcements on Discord](https://discord.com/channels/1562896153710690495/1563071156527833210)
- Bugs and feature requests: [GitHub issues](https://github.com/node-q/hbench/issues)
- The full reference: [docs/REFERENCE.md](docs/REFERENCE.md)
- Adding an agent: [docs/ADDING_AN_AGENT.md](docs/ADDING_AN_AGENT.md)
