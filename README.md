```text
██╗  ██╗██████╗ ███████╗███╗   ██╗ ██████╗██╗  ██╗
██║  ██║██╔══██╗██╔════╝████╗  ██║██╔════╝██║  ██║
███████║██████╔╝█████╗  ██╔██╗ ██║██║     ███████║
██╔══██║██╔══██╗██╔══╝  ██║╚██╗██║██║     ██╔══██║
██║  ██║██████╔╝███████╗██║ ╚████║╚██████╗██║  ██║
╚═╝  ╚═╝╚═════╝ ╚══════╝╚═╝  ╚═══╝ ╚═════╝╚═╝  ╚═╝
```

# hbench

hbench is the open source benchmark engine behind [Harness Playground](https://nodeq.tech), by node-q.

As AI agents learn to improve themselves, progress needs proof. hbench runs a harness (Claude Code, Codex,
Gemini CLI and 40+ more) on fixed tasks in Docker, with its version and model pinned, so you can tell whether a
change really made it better.

- **Compare versions.** `--ak version=X` pins the harness, on the same model and tasks.
- **Check the result.** `hbench jobs verify` fails if a job's files have changed.
- **Use your own tasks.** `hbench init` starts one, and [hbench-action](https://github.com/node-q/hbench-action)
  runs them in CI. For a private site for your company, [talk to us](https://nodeq.tech/contact#enterprise).
- **Publish it.** Upload a job to [Your bench](https://nodeq.tech/bench) to see it on a board.

Contents:

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
curl -fsSL https://github.com/node-q/hbench/releases/latest/download/install.sh | sh
```

Later, `hbench update` installs the newest release. To check the installer before you run it, see
[SECURITY.md](SECURITY.md#checking-the-installer).

Or build from source:

```bash
git clone https://github.com/node-q/hbench
cd hbench
cargo install --path .
```

## Quick start

Try it on a small task. This needs no key: with no agent named, hbench runs the task's reference solution.

```bash
hbench init demo/hello --task    # a new task, in hello/
hbench run -p hello              # run it
hbench view                      # see the results in your browser
```

To run a real agent, set its key (see [Agents](#agents)), then pick a dataset, tasks, agent and model:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
hbench run
```

Every run is saved in `jobs/`. To see a job on a board, upload its folder to
[Your bench](https://nodeq.tech/bench). It stays private until you publish it.

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
hbench run -p <task>                                      # a local task, or a dir of tasks
hbench run -d terminal-bench@2.0.1 \
  -a claude-code -m anthropic/claude-opus-5-5             # a dataset, with Claude Code
hbench run -d terminal-bench@2.0.1 \
  -a claude-code -m anthropic/claude-opus-5-5 \
  --ak version=<version>                                  # the same, at one version of the harness
hbench run -c job.yaml                                    # from a job config (YAML/JSON)

# Results
hbench view                                               # web viewer
hbench jobs summary -p jobs/<job>
hbench jobs resume -p jobs/<job>
hbench jobs verify -p jobs/<job>                          # fail if the job's files have changed

# Tasks and datasets
hbench init <org/name> --task                             # a new task
hbench tasks check -p <tasks>                             # validate tasks: config, files, tests
hbench check <tasks>                                      # review tasks against a rubric, with claude-code
hbench datasets list|download

# Other
hbench agents list                                        # every agent
hbench agents schema <agent>                              # an agent's options
hbench --help
```

Useful options: `-l 5` runs only 5 tasks, `-k 3` runs each task 3 times, `-n 4` runs 4 at a time.

Datasets include Terminal-Bench, SWE-bench Verified, Lite and Multilingual, and HumanEval+. `hbench datasets list`
shows them all.

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
| … 40+ in all | `hbench agents list` | | |

## License

Apache-2.0. See [LICENSE](LICENSE).

## Contributors and contact

hbench is built and maintained by [node-q](https://nodeq.tech). See the
[contributors graph](https://github.com/node-q/hbench/graphs/contributors) for everyone who has contributed.

- Questions and help: [#support on Discord](https://discord.gg/5b6wCn5sEK)
- New releases: [#announcements on Discord](https://discord.gg/dEvy6DCMTn)
- Bugs and feature requests: [GitHub issues](https://github.com/node-q/hbench/issues)
- The full reference: [docs/REFERENCE.md](docs/REFERENCE.md)
- Adding an agent: [docs/ADDING_AN_AGENT.md](docs/ADDING_AN_AGENT.md)
