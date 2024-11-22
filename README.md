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

Configuration is in TOML. Let's start with an example, here's how you might
configure a Rust project (it's a reduced version of [this repository's own
config](limmat.toml)):

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
code. It's run from the root directory of a copy of the repository, with the
commit to be tested already checked out.

> [!WARNING]
> Limmat doesn't clean the source tree for you, it just does `git checkout`. If
> your test command can't be trusted to work in a dirty worktree (for example,
> if you have janky Makefiles) it should start with something like `git clean
> -fdx`.

When the test is no longer needed (usually because the commit is no longer in
the range being watched), the test comamnd will receive `SIGINT`. It should try
to shut down promptly so that the worktree can be reused for another test.

> [!NOTE]
> Tests configured with `command` are currently hard-coded to use Bash as the
> shell. There's no good reason for this.

### Caching

Results are stored in a database, and by default Limmat won't run the same test
for the same commit twice. 

You can disable that behaviour for a test by setting `cache = "no_caching"`;
then when Limmat restarts it will re-run all instances of that test.

Alternatively, you can crank the caching _up_ by setting `cache = "by_tree"`.
That means Limmat won't re-run tests if the code hasn't changed, for example if
you just change the commit message.

Test results also aren't cached if they result in an _error_.

Check out the
[json-schema](https://json-schema.app/view/%23?url=https%3A%2F%2Fraw.githubusercontent.com%2Fbjackman%2Flimmat%2Frefs%2Fheads%2Fmaster%2Flimmat.schema.json)??