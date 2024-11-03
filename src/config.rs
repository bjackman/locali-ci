use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    ffi::OsString,
    hash::{DefaultHasher, Hash as _, Hasher as _},
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context as _};
#[allow(unused_imports)]
use log::debug;
use serde::Deserialize;

use crate::{
    dag::{Dag, GraphNode},
    resource::ResourceKey,
    test::{self, CachePolicy, TestName},
};

#[derive(Deserialize, Debug, Hash, Clone)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub enum Resource {
    /// Shorthand for describing a singular resource, equivalent to setting count=1.
    Bare(String),
    /// Specify resources where you don't care about the value of the token.
    Counted { name: String, count: usize },
    /// Specify resources with explicitly set token values. These will be passed
    /// into the job environment via LCI_RESOURCE_<name>_<n> where n is 0-indexed.
    // TODO: If there's only one, we should also export it without the _<n>
    Explicit { name: String, tokens: Vec<String> },
}

impl Resource {
    pub fn name(&self) -> &str {
        match self {
            Self::Bare(n) => n,
            Self::Counted { name: n, count: _ } => n,
            Self::Explicit { name: n, tokens: _ } => n,
        }
    }

    pub fn count(&self) -> usize {
        match self {
            Self::Bare(_) => 1,
            Self::Counted { name: _, count: c } => *c,
            Self::Explicit { name: _, tokens: t } => t.len(),
        }
    }
}

#[derive(Deserialize, Debug, Hash, Clone)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub enum Command {
    Shell(String),
    Raw(Vec<String>),
}

impl Command {
    pub fn program(&self) -> OsString {
        match self {
            Self::Shell(_) => "bash".into(), // TODO: Figure out the user's configured shell.
            Self::Raw(args) => args[0].clone().into(),
        }
    }

    pub fn args(&self) -> Vec<OsString> {
        match self {
            Self::Shell(cmd) => vec!["-c".into(), cmd.into()],
            Self::Raw(args) => args[1..].iter().map(|s| s.into()).collect(),
        }
    }
}

#[derive(Deserialize, Debug, Hash, Clone)]
#[serde(deny_unknown_fields)]
pub struct Test {
    name: String,
    command: Command,
    #[serde(default = "default_requires_worktree")]
    requires_worktree: bool,
    resources: Option<Vec<Resource>>,
    #[serde(default = "default_shutdown_grace_period")]
    /// When a job is no longer needed it's SIGTERMed. If it doesn't respond (by
    /// dying) after this duration it will then be SIGKILLed. This also affects
    /// the overall shutdown of local-ci so do not set this to longer than you are
    /// willing to wait when you terminate this program.
    shutdown_grace_period_s: u64,
    #[serde(default = "default_cache_policy")]
    cache: CachePolicy,
    #[serde(default = "default_depends_on")]
    depends_on: Vec<String>,
}

fn default_depends_on() -> Vec<String> {
    vec![]
}

fn default_requires_worktree() -> bool {
    true
}

// This implementation is only valid for Tests among those registered for a single Manager.
impl GraphNode<String> for Test {
    fn id(&self) -> impl Borrow<String> {
        &self.name
    }

    fn child_ids(&self) -> Vec<impl Borrow<String>> {
        self.depends_on.iter().collect()
    }
}

impl Test {
    // Convert to the "real" object. other_tests is the set of other tests that
    // have already been parsed, which must include all of these test's
    // transitive dependencies (or this will panic).
    pub fn parse(&self, other_tests: &HashMap<TestName, test::Test>) -> anyhow::Result<test::Test> {
        let mut seen_resources = HashSet::new();
        for resource in self.resources.as_ref().unwrap_or(&vec![]) {
            if seen_resources.contains(&resource.name()) {
                // TODO: Need better error messages.
                bail!("duplicate resource reference {:?}", resource.name());
            }
            seen_resources.insert(resource.name());
        }
        let mut needs_resources: HashMap<ResourceKey, usize> = self
            .resources
            .as_ref()
            .unwrap_or(&vec![])
            .iter()
            .map(|r| (ResourceKey::UserToken(r.name().to_owned()), r.count()))
            .collect();
        if self.requires_worktree {
            needs_resources.insert(ResourceKey::Worktree, 1);
        }

        // Hash the config, also taking into account the hashes of the
        // dependency test configs.
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        for dep_name in &self.depends_on {
            other_tests[&TestName::new(dep_name)]
                .config_hash
                .hash(&mut hasher);
        }
        let config_hash = hasher.finish();

        Ok(test::Test {
            name: TestName::new(self.name.clone()),
            program: self.command.program(),
            args: self.command.args(),
            needs_resources,
            shutdown_grace_period: Duration::from_secs(self.shutdown_grace_period_s),
            cache_policy: self.cache,
            config_hash,
            depends_on: self.depends_on.iter().map(TestName::new).collect(),
        })
    }
}

fn default_cache_policy() -> CachePolicy {
    // Hard to choose a default here. Rationale for this choice: It's weird not
    // to want any caching at all. Almost all of the time you want ByTree, but
    // ByCommit will give you 80% of the value, and lots of people don't think
    // about the difference between tree and commit anyway.
    CachePolicy::ByCommit
}

fn default_shutdown_grace_period() -> u64 {
    60
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub num_worktrees: usize,
    resources: Option<Vec<Resource>>,
    tests: Vec<Test>,
}

type ResourceTokens = HashMap<ResourceKey, Vec<String>>;

impl Config {
    pub fn parse_resource_tokens(&self) -> ResourceTokens {
        self.resources
            .as_ref()
            .unwrap_or(&vec![])
            .iter()
            .map(|resource| {
                (
                    ResourceKey::UserToken(resource.name().to_owned()),
                    match resource {
                        Resource::Explicit { name: _, tokens } => tokens.clone(),
                        _ => (0..resource.count())
                            .map(|i| format!("{}-{}", resource.name(), i))
                            .collect(),
                    },
                )
            })
            .collect()
    }

    pub fn parse_tests(
        &self,
        resource_tokens: &ResourceTokens,
    ) -> anyhow::Result<Dag<TestName, Arc<test::Test>>> {
        let tests = Dag::new(self.tests.clone()).context("parsing test dependency graph")?;
        // This is beginning to be kinda cool but there's still an awkward
        // divide between the way the fold accumulator is a HashMap but the Dag
        // itself internally has a separate mechanism for indexing nodes (i.e.
        // it just uses Vec). Maybe if we fixed that it would also let us fix
        // the awkwardness where we have to repeat all the DAG traversal logic
        // when mapping to a new graph, even when the new graph is isomorphic to
        // the old one.
        // It's also awkward that users of this fold mechanism have to manually
        // insert their new nodes into the accumulator.
        let tests = tests
            .bottom_up()
            .try_fold(
                HashMap::new(),
                |mut parsed_tests, test_conf| -> anyhow::Result<HashMap<TestName, test::Test>> {
                    let new_test = test_conf.parse(&parsed_tests)?;
                    parsed_tests.insert(new_test.name.clone(), new_test);
                    Ok(parsed_tests)
                },
            )
            .context("parsing tests")?;
        // This bit is kinda inefficient: we just did all the graph-traversal
        // biz on the config::Test objects, and we know that the same logic
        // would be valid for the test::Test objects. But we've lost track of
        // that knowledge (and the graph code is based on fixed order of the
        // nodes in a Vec, which we've lost), so now we do it all again :(
        let tests = Dag::new(tests.into_values().map(Arc::new)).unwrap();

        // Check for invalid resource references.
        for test in tests.nodes() {
            for key in test.needs_resources.keys() {
                if let ResourceKey::UserToken(name) = key {
                    if !resource_tokens.contains_key(key) {
                        bail!(
                            "undefined resource {:?} referenced in test {:?}",
                            name,
                            test.name
                        );
                    }
                }
            }
        }

        Ok(tests)
    }
}
