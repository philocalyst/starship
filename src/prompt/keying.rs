//! How each module earns its place on the fast path.
//!
//! Two questions decide everything about a module's participation in an
//! incremental prompt: *is it worth not recomputing?* and *when does a previous
//! result stop being true?* The old design answered both with one measurement —
//! anything slower than 5ms was cached, everything else was not — which
//! conflated cost with stability. That is why a `custom` module (always slow,
//! frequently volatile) was cached under a key that could not detect its change,
//! while a module could silently drop out of the cache on a faster machine.
//!
//! Here the two questions are separated, and the type makes the meaningless
//! combinations unrepresentable. A module is one of three things ([`Profile`]):
//! computed every time, reused while a stated condition holds, or sampled in
//! the background because it is both costly and inherently unstable.
//!
//! The condition is drawn from a deliberately small vocabulary ([`Keying`]).
//! Five recipes cover every module in the prompt, because the modules are far
//! more alike than the count suggests: most are "look for project files in this
//! directory, then ask a tool on `PATH` for its version", which is one recipe
//! used fifty times.

use crate::context::Context;

use super::deps::{Deps, DepsBuilder};

/// How a module participates in an incremental prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Recomputed on every paint.
    ///
    /// Either the module is cheap enough that bookkeeping would cost more than
    /// the work, or its input arrives fresh with each invocation — the exit
    /// status, the command duration, the job count, the keymap, the clock.
    /// These never go stale because they are never reused.
    Live,

    /// Reused for as long as its [`Keying`] still holds.
    ///
    /// This is the only variant that reuses a previous render, and it may do so
    /// only against a stated condition — which is what makes staleness
    /// impossible rather than merely unlikely.
    Keyed(Keying),

    /// Computed away from the paint path and shown from the most recent
    /// completed run, without any claim that it is still current.
    ///
    /// For modules that are expensive *and* have no stable key: battery charge,
    /// memory pressure, whether a sudo ticket is still valid. There is nothing
    /// to observe that would prove such a value unchanged, so the honest
    /// framing is a sample rather than a cache. The prompt shows the last
    /// reading; the background refresh takes the next one.
    Sampled,
}

/// What a [`Profile::Keyed`] module's output rests on.
///
/// Each recipe knows how to observe its subject against a [`Context`],
/// producing the [`Deps`] that serve as cache key, watch set, and explanation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keying {
    /// The working directory: its identity, and its own modification time —
    /// which moves whenever an entry is added or removed, and so detects the
    /// appearance of the project files a detector looks for.
    Dir,

    /// The version-control state the working directory sits in: the marker
    /// directory plus the refs that move as you work.
    Repo,

    /// Tools resolved from `PATH`, together with [`Keying::Dir`].
    ///
    /// A version module's output is a function of two things: whether the
    /// directory still looks like that kind of project, and which binary the
    /// lookup finds. Pinning the resolved path *and* its modification time
    /// catches both a `PATH` change and an in-place toolchain upgrade — the two
    /// ways `node --version` starts returning something else. Several commands
    /// may be listed, for modules that consult a version manager before the
    /// tool itself.
    Toolchain(&'static [&'static str]),

    /// Named environment variables, which is how most cloud, context, and
    /// shell-integration modules are actually steered.
    Env(&'static [&'static str]),

    /// The union of several recipes.
    ///
    /// Composition rather than a wider enum: a module keyed on both its
    /// directory and an environment variable is spelled as those two recipes,
    /// not as a distinct sixth case that must be kept in step with both.
    All(&'static [Keying]),
}

impl Keying {
    /// Observe this recipe's subject, producing the module's dependencies.
    ///
    /// Called only while a module is actually being computed. The fast paint
    /// never evaluates a recipe: it re-observes the [`Deps`] a previous run
    /// already recorded, which costs a few `stat` calls and — critically — no
    /// repository discovery and no `PATH` resolution.
    pub fn observe(&self, context: &Context) -> Deps {
        self.observe_into(context, Deps::builder()).build()
    }

    /// Recipes thread the builder by value, so composition is a fold and
    /// [`Keying::All`] needs no special handling beyond one.
    fn observe_into(&self, context: &Context, builder: DepsBuilder) -> DepsBuilder {
        match self {
            Self::Dir => builder.path(&context.current_dir),
            Self::Repo => observe_repo(context, builder),
            Self::Toolchain(commands) => commands.iter().fold(
                Self::Dir.observe_into(context, builder),
                |builder, command| observe_command(command, builder),
            ),
            Self::Env(names) => names
                .iter()
                .fold(builder, |builder, name| builder.env(*name)),
            Self::All(recipes) => recipes.iter().fold(builder, |builder, recipe| {
                recipe.observe_into(context, builder)
            }),
        }
    }
}

/// Pin the repository by the files that move as work happens.
///
/// `HEAD` covers branch switches and detached checkouts, `index` covers staging,
/// and the worktree covers every unstaged edit. A directory's own mtime cannot
/// see edits to existing descendants, so the worktree is explicitly a
/// Watchman-backed tree dependency; without Watchman we recompute rather than
/// pretend a cheap stat establishes that `git status` is still current.
fn observe_repo(context: &Context, builder: DepsBuilder) -> DepsBuilder {
    let builder = builder.path(&context.current_dir);

    let Ok(repo) = context.get_repo() else {
        // Not in a repository. The directory observation above is what detects
        // one appearing (`git init`, or a `cd` into one).
        return builder;
    };

    let git_dir = &repo.path;
    let builder = builder
        .path(git_dir.join("HEAD"))
        .path(git_dir.join("index"))
        .path(git_dir.join("MERGE_HEAD"))
        .path(git_dir.join("REBASE_HEAD"));
    let builder = match &repo.workdir {
        Some(workdir) => builder.tree(workdir),
        None => builder,
    };
    builder.maybe_mark(
        "workdir",
        repo.workdir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
    )
}

/// Pin a tool by where it resolves and when it was last written.
fn observe_command(command: &str, builder: DepsBuilder) -> DepsBuilder {
    // Record the lookup input as well as its result: if `PATH` changes such
    // that a *different* binary would be found, the recorded resolution is no
    // longer the one that would be used, and the entry must not be reused.
    let builder = builder.env("PATH");

    match which::which(command) {
        // Observing the resolved path pins both the location and the binary's
        // own mtime, so an in-place upgrade invalidates the entry.
        Ok(path) => builder.path(path),
        // The tool is absent. Recorded as such, because a module that rendered
        // nothing because `node` was missing must recompute once it appears —
        // and an absent path is watched via its parent, so installation is seen.
        Err(_) => builder.mark("missing", command),
    }
}

/// The profile of every module in the prompt.
///
/// One table rather than a hundred scattered declarations, because the useful
/// property here is *comparability*: whether `nodejs` and `bun` are treated
/// alike should be answerable by looking, not by opening two files. The grouping
/// below is the argument for the vocabulary — the long run of `Toolchain` arms
/// is fifty modules that genuinely are the same module with a different binary.
pub fn profile(module: &str) -> Profile {
    use Keying::{All, Dir, Env, Repo, Toolchain};
    use Profile::{Keyed, Live, Sampled};

    match module {
        // ---- Live: input arrives with the invocation, or the work is trivial.
        //
        // Caching any of these would be a bug, not an optimization: they are
        // the parts of the prompt that must describe *this* moment.
        "character" | "cmd_duration" | "status" | "jobs" | "time" | "shell" | "shlvl"
        | "line_break" | "fill" | "username" | "hostname" | "os" | "localip" | "netns"
        | "container" | "singularity" | "directory" => Live,

        // Claude Code's data arrives on stdin, so it is as fresh as the call.
        "claude_context" | "claude_cost" | "claude_model" => Live,

        // ---- Sampled: costly, and with nothing observable that would prove a
        // previous reading still true.
        "battery" | "memory_usage" | "sudo" => Sampled,

        // ---- Version control.
        "git_branch" | "git_commit" | "git_metrics" | "git_state" | "git_status" | "hg_branch"
        | "hg_state" | "fossil_branch" | "fossil_metrics" | "pijul_channel" | "vcsh" | "vcs" => {
            Keyed(Repo)
        }

        // ---- Environment-steered context modules.
        "aws" => Keyed(Env(&[
            "AWS_PROFILE",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
            "AWS_CONFIG_FILE",
            "AWS_SHARED_CREDENTIALS_FILE",
            "AWSU_PROFILE",
            "AWS_VAULT",
            "AWSUME_PROFILE",
        ])),
        "azure" => Keyed(Env(&["AZURE_CONFIG_DIR"])),
        "gcloud" => Keyed(Env(&["CLOUDSDK_CONFIG", "CLOUDSDK_ACTIVE_CONFIG_NAME"])),
        "openstack" => Keyed(Env(&["OS_CLOUD"])),
        "kubernetes" => Keyed(All(&[Dir, Env(&["KUBECONFIG"])])),
        "docker_context" => Keyed(All(&[Dir, Env(&["DOCKER_CONTEXT", "DOCKER_HOST"])])),
        "conda" => Keyed(Env(&["CONDA_DEFAULT_ENV", "CONDA_PREFIX"])),
        "nix_shell" => Keyed(Env(&["IN_NIX_SHELL", "NIX_SHELL_PACKAGES", "name"])),
        "guix_shell" => Keyed(Env(&["GUIX_ENVIRONMENT"])),
        "direnv" => Keyed(All(&[
            Toolchain(&["direnv"]),
            Env(&["DIRENV_FILE", "DIRENV_DIR"]),
        ])),
        "nats" => Keyed(Env(&["XDG_CONFIG_HOME"])),
        "terraform" => Keyed(All(&[Toolchain(&["terraform"]), Env(&["TF_WORKSPACE"])])),
        "pulumi" => Keyed(All(&[Toolchain(&["pulumi"]), Env(&["PULUMI_HOME"])])),
        "vagrant" => Keyed(Toolchain(&["vagrant"])),
        "spack" => Keyed(Env(&["SPACK_ENV"])),
        "mise" => Keyed(Toolchain(&["mise"])),
        "pixi" => Keyed(All(&[
            Toolchain(&["pixi"]),
            Env(&["PIXI_ENVIRONMENT_NAME"]),
        ])),
        "helm" => Keyed(Toolchain(&["helm"])),

        // Reads project manifests rather than consulting a tool.
        "package" => Keyed(Dir),

        // ---- Toolchains: detect in the directory, then ask the tool.
        "bun" => Keyed(Toolchain(&["bun"])),
        "buf" => Keyed(Toolchain(&["buf"])),
        "c" => Keyed(Toolchain(&["cc", "gcc", "clang"])),
        "cpp" => Keyed(Toolchain(&["c++", "g++", "clang++"])),
        "cmake" => Keyed(Toolchain(&["cmake"])),
        "cobol" => Keyed(Toolchain(&["cobc"])),
        "crystal" => Keyed(Toolchain(&["crystal"])),
        "daml" => Keyed(Toolchain(&["daml"])),
        "dart" => Keyed(Toolchain(&["dart"])),
        "deno" => Keyed(Toolchain(&["deno"])),
        "dotnet" => Keyed(Toolchain(&["dotnet"])),
        "elixir" => Keyed(Toolchain(&["elixir"])),
        "elm" => Keyed(Toolchain(&["elm"])),
        "erlang" => Keyed(Toolchain(&["erl"])),
        "fennel" => Keyed(Toolchain(&["fennel"])),
        "fortran" => Keyed(Toolchain(&["gfortran"])),
        "gleam" => Keyed(Toolchain(&["gleam"])),
        "golang" => Keyed(Toolchain(&["go"])),
        "gradle" => Keyed(Toolchain(&["gradle"])),
        "haskell" => Keyed(Toolchain(&["ghc", "stack"])),
        "haxe" => Keyed(Toolchain(&["haxe"])),
        "java" => Keyed(Toolchain(&["java"])),
        "julia" => Keyed(Toolchain(&["julia"])),
        "kotlin" => Keyed(Toolchain(&["kotlin", "kotlinc"])),
        "lua" => Keyed(Toolchain(&["lua"])),
        "maven" => Keyed(Toolchain(&["mvn"])),
        "meson" => Keyed(Toolchain(&["meson"])),
        "mojo" => Keyed(Toolchain(&["mojo"])),
        "nim" => Keyed(Toolchain(&["nim"])),
        "nodejs" => Keyed(Toolchain(&["node"])),
        "ocaml" => Keyed(Toolchain(&["ocaml", "esy"])),
        "odin" => Keyed(Toolchain(&["odin"])),
        "opa" => Keyed(Toolchain(&["opa"])),
        "perl" => Keyed(Toolchain(&["perl"])),
        "php" => Keyed(Toolchain(&["php"])),
        "purescript" => Keyed(Toolchain(&["purs"])),
        "python" => Keyed(All(&[
            Toolchain(&["python", "python3", "pyenv"]),
            Env(&["VIRTUAL_ENV", "CONDA_DEFAULT_ENV", "PYENV_VERSION"]),
        ])),
        "quarto" => Keyed(Toolchain(&["quarto"])),
        "raku" => Keyed(Toolchain(&["raku"])),
        "red" => Keyed(Toolchain(&["red"])),
        "rlang" => Keyed(Toolchain(&["R"])),
        "ruby" => Keyed(Toolchain(&["ruby"])),
        "rust" => Keyed(All(&[
            Toolchain(&["rustc", "rustup"]),
            Env(&["RUSTUP_TOOLCHAIN"]),
        ])),
        "scala" => Keyed(Toolchain(&["scalac", "scala"])),
        "solidity" => Keyed(Toolchain(&["solc"])),
        "swift" => Keyed(Toolchain(&["swift"])),
        "typst" => Keyed(Toolchain(&["typst"])),
        "vlang" => Keyed(Toolchain(&["v"])),
        "xmake" => Keyed(Toolchain(&["xmake"])),
        "zig" => Keyed(Toolchain(&["zig"])),

        // ---- Unknown, and `custom` in particular.
        //
        // `custom` modules run arbitrary commands, which makes them the most
        // expensive modules in a typical prompt *and* the ones whose inputs
        // starship cannot see. The old threshold cached them for exactly the
        // first reason while being blind to the second, which is how a custom
        // module could show last hour's answer indefinitely. Treating the
        // unknown as `Sampled` keeps the cost off the paint path without ever
        // claiming the value is still current.
        _ => Sampled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::ALL_MODULES;

    #[test]
    fn every_module_is_classified_deliberately() {
        // `profile` has a catch-all arm, so an unclassified module would fall
        // through silently rather than fail to compile. This asserts the arm is
        // reserved for genuinely unknown names (`custom`), not a resting place
        // for modules nobody got around to.
        let unclassified: Vec<_> = ALL_MODULES
            .iter()
            .filter(|m| profile(m) == Profile::Sampled)
            .filter(|m| !matches!(**m, "battery" | "memory_usage" | "sudo"))
            .collect();

        assert!(
            unclassified.is_empty(),
            "these modules fell through to the catch-all and need an explicit \
             profile: {unclassified:?}",
        );
    }

    #[test]
    fn volatile_inputs_are_never_keyed() {
        // The prompt's job is to describe the command that just ran. Any of
        // these being reusable would be a correctness bug, so pin them.
        for module in ["character", "cmd_duration", "status", "jobs", "time"] {
            assert_eq!(
                profile(module),
                Profile::Live,
                "{module} depends on per-invocation input and must never be reused",
            );
        }
    }

    #[test]
    fn toolchain_pins_both_the_directory_and_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let mut context = crate::test::default_context();
        context.current_dir = dir.path().to_path_buf();

        let deps = Keying::Toolchain(&["definitely-not-a-real-binary"]).observe(&context);

        assert!(
            deps.paths().iter().any(|p| p.path == context.current_dir),
            "a version module renders only when the directory looks like that \
             kind of project, so the directory is part of the key",
        );
        assert!(
            deps.env().iter().any(|e| e.name == "PATH"),
            "resolution depends on PATH, so a PATH change must invalidate",
        );
    }

    #[test]
    fn composition_unions_its_parts() {
        let context = crate::test::default_context();
        let composed = Keying::All(&[Keying::Dir, Keying::Env(&["HOME"])]).observe(&context);

        assert!(!composed.paths().is_empty());
        assert!(composed.env().iter().any(|e| e.name == "HOME"));
    }
}
