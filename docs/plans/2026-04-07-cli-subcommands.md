# Add CLI subcommands

## Context

The CLI is currently flat — all args go to the VM. Adding subcommands
(`go`, `project info/list/remove`, `help`) requires restructuring the
arg parsing and splitting logic into command modules.

## Arg parsing strategy

Parse `--` manually BEFORE clap. Split argv into:
- `ez_args`: everything before `--` (or all args if no `--`)
- `extra_args`: everything after `--`

Feed `ez_args` to clap for subcommand routing. Pass `extra_args` to
the `go` command. Error if extra_args given with non-go commands.
Default to `go` when no subcommand given.

## Commands

| Command | Description |
|---------|-------------|
| `ez` / `ez go` | Start VM (current behavior). Accepts `-- <args>` |
| `ez project info` | Show project config, cache dir, image |
| `ez project list` (`ls`) | List all projects in ~/.ezpez/projects/ |
| `ez project remove` (`rm`) | Remove a project (fail if running) |
| `ez help` | Print help (clap built-in) |

## File structure

```
cli/src/
  main.rs              # entrypoint: split --, dispatch
  cli.rs               # clap definitions, global settings, LogLevel
  cli/
    cmd_go.rs          # go command: current main.rs logic
    cmd_project_info.rs
    cmd_project_list.rs
    cmd_project_remove.rs
```

## Detailed changes

### main.rs

```rust
fn main() {
    // 1. Split argv at "--"
    let raw_args: Vec<String> = std::env::args().collect();
    let (ez_args, extra_args) = split_at_separator(&raw_args);
    
    // 2. Parse with clap
    let cli = Cli::parse_from(&ez_args);
    
    // 3. Dispatch
    match cli.command.unwrap_or(Command::Go) {
        Command::Go => cmd_go::run(cli.global, extra_args),
        Command::Project(sub) => {
            if !extra_args.is_empty() {
                error("-- args not supported for this command");
            }
            match sub {
                ProjectCommand::Info => cmd_project_info::run(cli.global),
                ProjectCommand::List => cmd_project_list::run(cli.global),
                ProjectCommand::Remove => cmd_project_remove::run(cli.global),
            }
        }
    }
}
```

### cli.rs — clap definitions

```rust
#[derive(Parser)]
#[command(name = "ez", version, about)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,
    
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Args)]
struct GlobalArgs {
    #[arg(short, long)]
    quiet: bool,
    #[arg(long, env = "EZ_LOG_LEVEL", default_value = "warn")]
    log_level: LogLevel,
}

#[derive(Subcommand)]
enum Command {
    /// Start the VM (default when no command given)
    Go,
    /// Manage projects
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// Show project info for current directory
    Info,
    /// List all projects
    #[command(alias = "ls")]
    List,
    /// Remove a project
    #[command(alias = "rm")]
    Remove,
}
```

### cmd_go.rs

Move the current `main.rs` async logic here:
- `pub fn run(global: GlobalArgs, extra_args: Vec<String>)`
- Loads config, locks project, prepares bundle, starts VM
- `extra_args` replaces the old `CliArgs.args`

### cmd_project_info.rs

- Load config from cwd
- Compute project hash
- Show: image, presets, mounts, cache config, cache dir path, overlay dir

### cmd_project_list.rs

- Iterate `~/.ezpez/projects/*/`
- For each: check lock file (running?), read image_digest, show cwd
- Need to store the original cwd somewhere — currently only the hash
  is stored. Add a `cwd` file to the project dir on lock.

### cmd_project_remove.rs

- Compute project hash from cwd (or accept path arg)
- Check lock file — fail if running
- Remove project dir

## Key considerations

- `cli::initialize()` currently sets up signal handlers and returns
  `CliArgs`. Refactor to return `Cli` struct instead, move signal
  setup into `cmd_go` (only needed for VM).
- The `CliArgs` type is used in `rpc/supervisor.rs` for `log_filter()`.
  Change to pass `GlobalArgs` or just the filter string directly.
- For `project list`: need to store cwd→hash mapping. Write a `cwd`
  file in the project dir during `project::lock()`.

## Verification

1. `mise run lint` passes
2. `cargo test -p ezpez-cli` — all 68 tests pass
3. `ez` (no args) → starts VM as before
4. `ez go -- ls /` → starts VM with `ls /`
5. `ez project list` → lists projects
6. `ez project info` → shows current project
7. `ez help` → shows subcommands
