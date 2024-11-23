# Limmat: Local Immediate Automated Testing

Limmat watches a Git branch for changes, and runs tests on every commit, in
parallel. It's a bit like having good CI, but it's all local so you don't need
to figure out infrastructure, and you get feedback faster.

It gives you a live web (and terminal) UI to show you which commits are passing
or failing each test:

![screenshot of UI](docs/assets/screenshot.png)

Clicking on the test results ([including in the
terminal](https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda))
will take you to the logs.

## Installation

[Install
Cargo](https://doc.rust-lang.org/cargo/getting-started/installation.html) then:

```sh
cargo install limmat
```

## Usage

Write a config file (details [below](#config)). Assuming you called it `limmat.toml`, and
the branch you're working on is based on `origin/master`, run this from the root
of your repository:

```sh
limmat watch --config limmat.toml origin/master
```

Limmat will start testing every commit in the range `origin/master..HEAD`.
Meanwhile, it watches your repository for commits being added to or removed from
that range and spawns new tests or cancels them as needed to get you your
feedback as soon as possible.

## Configuration {#config}

Configuration is in [TOML](https://toml.io/en/). Let's start with an example,
here's how you might configure a Rust project (it's a reduced version of [this
repository's own config](limmat.toml)):

```toml
# Check out 8 copies of the repository to run tests in.
num_worktrees = 8

# Check that the formatting is correct
[[tests]]
name = "fmt"
command = "cargo fmt --check"

# Check that the tests pass
[[tests]]
name = "test"
command = "cargo test"
```
 
Each test is just a shell command. If you want to skip the shell, use `args` instead of `command`:

```toml
args = ["cargo", "test"]
```

### Writing the test command {#test-commands}

The test command's job is to produce a zero (sucess) or nonzero (failure) status
code. By defualt, it's run from the root directory of a copy of the repository,
with the commit to be tested already checked out. 

> [!WARNING]
> Limmat doesn't clean the source tree for you, it just does `git checkout`. If
> your test command can't be trusted to work in a dirty worktree (for example,
> if you have janky Makefiles) it should start with something like `git clean
> -fdx`.

If your test command doesn't actually need to access the codebase (for example,
if it only cares about the commit message), you can set `needs_worktree =
false`. In that case it will run in your main worktree, and the commit it needs
to test will be passed in the [environment](#env) as `$LIMMAT_COMMIT`.

> [!NOTE]
> Tests configured with `command` are currently hard-coded to use Bash as the
> shell. There's no good reason for this it's just a silly limitation of the
> implementation.

When the test is no longer needed (usually because the commit is no longer in
the range being watched), the test comamnd's process group will receive
`SIGTERM`. It should try to shut down promptly so that the worktree can be
reused for another test. If it doesn't shut down after a timeout then it will
receive `SIGKILL` instead. You can configure the timeout by setting
`shutdown_grace_period_s` in seconds (default 60).

### Caching

Results are stored in a database, and by default Limmat won't run a test again
if there's a result in the database for that commit.

You can disable that behaviour for a test by setting `cache = "no_caching"`;
then when Limmat restarts it will re-run all instances of that test.

Alternatively, you can crank the caching _up_ by setting `cache = "by_tree"`.
That means Limmat won't re-run tests unless the actual repository contents
change - for example changes to the commit message won't invalidate cache
results.

If the test is terminated by a signal, it isn't considered to have produced a
result: instead of "sucess" or "failure" it's an "error". Errors aren't cached.

> [!TIP]
> You can use this as a hack to prevent environmental failures from
> being stored as test failures. For example, in my own scripts I use `kill
> -SIGUSR1 $$` if no devices are available in my company's test lab. In a later
> version I'd like to formalize this hack as a feature, using designated exit
> codes instead of signal-termination.

The configuration for each test and its dependencies are hashed, and if this
hash changes then the database entry is invalidated.

> [!WARNING]
> If your test script uses config files that aren't checked into your repository,
> Limmat doesn't know about that and can't hash those files. It's up to you
> to determine if your scripts are "hermetic" - if they aren't you probably just want 
> to set `cache = "no_caching`.

### Resources

You probably have a lot of tests to run, otherwise you wouldn't find Limmat
useful. So the system needs a way to throttle the parallelism to avoid gobbling
resources. The most obious source of throttling is the worktrees - if your tests
need one (i.e. if you haven't set `needs_worktee = false`) then those tests can
only be parallelised up to `num_worktrees`. But there's also more flexible
throttling available.

To use this, define `resources` globally (separately from `tests`) in your
config file, for example:

```toml
[[resources]]
name = "pokemon"
tokens = ["moltres", "articuno", "zapdos"]
```

Now a can refer to this resource, and it won't be run until Limmat can
allocate a Pokemon for it:

```toml
[[tests]]
name = "test_with_pokemon"
resources = ["pokemon"]
command = "./test_with_pokemon.sh --pokemon=$LIMMAT_RESOURCE_pokemon
```

As you can see, resource values are passed in the [environment](#env) to the
test command.

Resources don't need to have values, they can also just be anonymous tokens for
limiting parallelism:

```toml
resources = [
    "singular_resource", # If you don't specify a count, it defaults to 1.
    { "name" = "threeple_resource", count = 3 },
]
```

### Reference

#### Config file

The JSON Schema is [available in the
repo](https://github.com/bjackman/limmat/blob/master/limmat.schema.json). (The
configuration is is TOML, but TOML and JSON are equivalent for our purposes
here. Limmat might accept JSON directly in a later version, and maybe other
formats like YAML). There are online viewers for reading JSON Schemata more
easily, try [viewing it in Atlassian's tool
here](https://json-schema.app/view/%23?url=https%3A%2F%2Fraw.githubusercontent.com%2Fbjackman%2Flimmat%2Frefs%2Fheads%2Fmaster%2Flimmat.schema.json).
[Contributions are
welcome](https://github.com/bjackman/limmat/commit/3181929c0f9031dbe9b13ad07a52b66f2f3439a4)
for static HTML documentation.

#### Job environment (#env)

These environment variables are passed to your job (remember - if you configure
your job with `args` instead of `command` it isn't interpreted by the shell):

| Name                              | Value                                                 |
| --------------------------------- | ----------------------------------------------------- |
| `LIMMAT_ORIGIN`                   | Path of the main repository worktree (i.e. `--repo`). |
| `LIMMAT_COMMIT`                   | Hash of the commit to be tested.                      |
| `LIMMAT_RESOURCE_<resource_name>` | Values for [resources](#resources) used by the test.  |