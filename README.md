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

Write a config file (details below). Assuming you called it `limmat.toml`, and
the branch you're working on is based on `origin/master`, run this from the root
of your repository:

```
limmat watch --config limmat.toml origin/master
```

Limmat will start testing every commit in the range `origin/master..HEAD`.
Meanwhile, it watches your repository for commits being added to or removed from
that range and spawns new tests or cancels them as needed to get you your
feedback as soon as possible.

Check out the
[json-schema](https://json-schema.app/view/%23?url=https%3A%2F%2Fraw.githubusercontent.com%2Fbjackman%2Flimmat%2Frefs%2Fheads%2Fmaster%2Flimmat.schema.json)??