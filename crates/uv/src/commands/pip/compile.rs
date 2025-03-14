use std::env;
use std::path::Path;

use anyhow::{anyhow, Result};
use itertools::Itertools;
use owo_colors::OwoColorize;
use rustc_hash::FxHashSet;
use tracing::debug;

use uv_cache::Cache;
use uv_client::{BaseClientBuilder, Connectivity, FlatIndexClient, RegistryClientBuilder};
use uv_configuration::{
    BuildOptions, Concurrency, ConfigSettings, Constraints, ExtrasSpecification, IndexStrategy,
    LowerBound, NoBinary, NoBuild, Reinstall, SourceStrategy, TrustedHost, Upgrade,
};
use uv_configuration::{KeyringProviderType, TargetTriple};
use uv_dispatch::BuildDispatch;
use uv_distribution_types::{
    DependencyMetadata, Index, IndexCapabilities, IndexLocations, NameRequirementSpecification,
    Origin, UnresolvedRequirementSpecification, Verbatim,
};
use uv_fs::Simplified;
use uv_git::GitResolver;
use uv_install_wheel::linker::LinkMode;
use uv_normalize::PackageName;
use uv_pypi_types::{Requirement, SupportedEnvironments};
use uv_python::{
    EnvironmentPreference, PythonEnvironment, PythonInstallation, PythonPreference, PythonRequest,
    PythonVersion, VersionRequest,
};
use uv_requirements::{
    upgrade::read_requirements_txt, RequirementsSource, RequirementsSpecification,
};
use uv_resolver::{
    AnnotationStyle, DependencyMode, DisplayResolutionGraph, ExcludeNewer, FlatIndex,
    InMemoryIndex, OptionsBuilder, PrereleaseMode, PythonRequirement, RequiresPython,
    ResolutionMode, ResolverEnvironment,
};
use uv_types::{BuildIsolation, EmptyInstalledPackages, HashStrategy, InFlight};
use uv_warnings::warn_user;

use crate::commands::pip::loggers::DefaultResolveLogger;
use crate::commands::pip::{operations, resolution_environment};
use crate::commands::{diagnostics, ExitStatus, OutputWriter};
use crate::printer::Printer;

/// Resolve a set of requirements into a set of pinned versions.
#[allow(clippy::fn_params_excessive_bools)]
pub(crate) async fn pip_compile(
    requirements: &[RequirementsSource],
    constraints: &[RequirementsSource],
    overrides: &[RequirementsSource],
    build_constraints: &[RequirementsSource],
    constraints_from_workspace: Vec<Requirement>,
    overrides_from_workspace: Vec<Requirement>,
    environments: SupportedEnvironments,
    extras: ExtrasSpecification,
    output_file: Option<&Path>,
    resolution_mode: ResolutionMode,
    prerelease_mode: PrereleaseMode,
    dependency_mode: DependencyMode,
    upgrade: Upgrade,
    generate_hashes: bool,
    no_emit_packages: Vec<PackageName>,
    include_extras: bool,
    include_markers: bool,
    include_annotations: bool,
    include_header: bool,
    custom_compile_command: Option<String>,
    include_index_url: bool,
    include_find_links: bool,
    include_build_options: bool,
    include_marker_expression: bool,
    include_index_annotation: bool,
    index_locations: IndexLocations,
    index_strategy: IndexStrategy,
    dependency_metadata: DependencyMetadata,
    keyring_provider: KeyringProviderType,
    allow_insecure_host: &[TrustedHost],
    config_settings: ConfigSettings,
    connectivity: Connectivity,
    no_build_isolation: bool,
    no_build_isolation_package: Vec<PackageName>,
    build_options: BuildOptions,
    python_version: Option<PythonVersion>,
    python_platform: Option<TargetTriple>,
    universal: bool,
    exclude_newer: Option<ExcludeNewer>,
    sources: SourceStrategy,
    annotation_style: AnnotationStyle,
    link_mode: LinkMode,
    python: Option<String>,
    system: bool,
    python_preference: PythonPreference,
    concurrency: Concurrency,
    native_tls: bool,
    quiet: bool,
    cache: Cache,
    printer: Printer,
) -> Result<ExitStatus> {
    // If the user requests `extras` but does not provide a valid source (e.g., a `pyproject.toml`),
    // return an error.
    if !extras.is_empty() && !requirements.iter().any(RequirementsSource::allows_extras) {
        return Err(anyhow!(
            "Requesting extras requires a `pyproject.toml`, `setup.cfg`, or `setup.py` file."
        ));
    }

    let client_builder = BaseClientBuilder::new()
        .connectivity(connectivity)
        .native_tls(native_tls)
        .keyring(keyring_provider)
        .allow_insecure_host(allow_insecure_host.to_vec());

    // Read all requirements from the provided sources.
    let RequirementsSpecification {
        project,
        requirements,
        constraints,
        overrides,
        source_trees,
        extras: used_extras,
        index_url,
        extra_index_urls,
        no_index,
        find_links,
        no_binary,
        no_build,
    } = RequirementsSpecification::from_sources(
        requirements,
        constraints,
        overrides,
        &client_builder,
    )
    .await?;

    let constraints = constraints
        .iter()
        .cloned()
        .chain(
            constraints_from_workspace
                .into_iter()
                .map(NameRequirementSpecification::from),
        )
        .collect();

    let overrides: Vec<UnresolvedRequirementSpecification> = overrides
        .iter()
        .cloned()
        .chain(
            overrides_from_workspace
                .into_iter()
                .map(UnresolvedRequirementSpecification::from),
        )
        .collect();

    // Read build constraints.
    let build_constraints =
        operations::read_constraints(build_constraints, &client_builder).await?;

    // If all the metadata could be statically resolved, validate that every extra was used. If we
    // need to resolve metadata via PEP 517, we don't know which extras are used until much later.
    if source_trees.is_empty() {
        if let ExtrasSpecification::Some(extras) = &extras {
            let mut unused_extras = extras
                .iter()
                .filter(|extra| !used_extras.contains(extra))
                .collect::<Vec<_>>();
            if !unused_extras.is_empty() {
                unused_extras.sort_unstable();
                unused_extras.dedup();
                let s = if unused_extras.len() == 1 { "" } else { "s" };
                return Err(anyhow!(
                    "Requested extra{s} not found: {}",
                    unused_extras.iter().join(", ")
                ));
            }
        }
    }

    // Find an interpreter to use for building distributions
    let environment_preference = EnvironmentPreference::from_system_flag(system, false);
    let interpreter = if let Some(python) = python.as_ref() {
        let request = PythonRequest::parse(python);
        PythonInstallation::find(&request, environment_preference, python_preference, &cache)
    } else {
        // TODO(zanieb): The split here hints at a problem with the abstraction; we should be able to use
        // `PythonInstallation::find(...)` here.
        let request = if let Some(version) = python_version.as_ref() {
            // TODO(zanieb): We should consolidate `VersionRequest` and `PythonVersion`
            PythonRequest::Version(VersionRequest::from(version))
        } else {
            PythonRequest::default()
        };
        PythonInstallation::find_best(&request, environment_preference, python_preference, &cache)
    }?
    .into_interpreter();

    debug!(
        "Using Python {} interpreter at {} for builds",
        interpreter.python_version(),
        interpreter.sys_executable().user_display().cyan()
    );

    if let Some(python_version) = python_version.as_ref() {
        // If the requested version does not match the version we're using warn the user
        // _unless_ they have not specified a patch version and that is the only difference
        // _or_ if builds are disabled
        let matches_without_patch = {
            python_version.major() == interpreter.python_major()
                && python_version.minor() == interpreter.python_minor()
        };
        if no_build.is_none()
            && python_version.version() != interpreter.python_version()
            && (python_version.patch().is_some() || !matches_without_patch)
        {
            warn_user!(
                "The requested Python version {} is not available; {} will be used to build dependencies instead.",
                python_version.version(),
                interpreter.python_version(),
            );
        }
    }

    // Create a shared in-memory index.
    let source_index = InMemoryIndex::default();

    // If we're resolving against a different Python version, use a separate index. Source
    // distributions will be built against the installed version, and so the index may contain
    // different package priorities than in the top-level resolution.
    let top_level_index = if python_version.is_some() {
        InMemoryIndex::default()
    } else {
        source_index.clone()
    };

    // Determine the Python requirement, if the user requested a specific version.
    let python_requirement = if universal {
        let requires_python = RequiresPython::greater_than_equal_version(
            if let Some(python_version) = python_version.as_ref() {
                &python_version.version
            } else {
                interpreter.python_version()
            },
        );
        PythonRequirement::from_requires_python(&interpreter, requires_python)
    } else if let Some(python_version) = python_version.as_ref() {
        PythonRequirement::from_python_version(&interpreter, python_version)
    } else {
        PythonRequirement::from_interpreter(&interpreter)
    };

    // Determine the environment for the resolution.
    let (tags, resolver_env) = if universal {
        (
            None,
            ResolverEnvironment::universal(environments.into_markers()),
        )
    } else {
        let (tags, marker_env) =
            resolution_environment(python_version, python_platform, &interpreter)?;
        (Some(tags), ResolverEnvironment::specific(marker_env))
    };

    // Generate, but don't enforce hashes for the requirements.
    let hasher = if generate_hashes {
        HashStrategy::Generate
    } else {
        HashStrategy::None
    };

    // Ignore development dependencies.
    let dev = Vec::default();

    // Incorporate any index locations from the provided sources.
    let index_locations = index_locations.combine(
        extra_index_urls
            .into_iter()
            .map(Index::from_extra_index_url)
            .chain(index_url.map(Index::from_index_url))
            .map(|index| index.with_origin(Origin::RequirementsTxt))
            .collect(),
        find_links
            .into_iter()
            .map(Index::from_find_links)
            .map(|index| index.with_origin(Origin::RequirementsTxt))
            .collect(),
        no_index,
    );

    // Add all authenticated sources to the cache.
    for index in index_locations.allowed_indexes() {
        if let Some(credentials) = index.credentials() {
            uv_auth::store_credentials(index.raw_url(), credentials);
        }
    }

    // Initialize the registry client.
    let client = RegistryClientBuilder::try_from(client_builder)?
        .cache(cache.clone())
        .index_urls(index_locations.index_urls())
        .index_strategy(index_strategy)
        .markers(interpreter.markers())
        .platform(interpreter.platform())
        .build();

    // Read the lockfile, if present.
    let preferences = read_requirements_txt(output_file, &upgrade).await?;
    let git = GitResolver::default();
    let capabilities = IndexCapabilities::default();

    // Combine the `--no-binary` and `--no-build` flags from the requirements files.
    let build_options = build_options.combine(no_binary, no_build);

    // Resolve the flat indexes from `--find-links`.
    let flat_index = {
        let client = FlatIndexClient::new(&client, &cache);
        let entries = client
            .fetch(index_locations.flat_indexes().map(Index::url))
            .await?;
        FlatIndex::from_entries(entries, tags.as_deref(), &hasher, &build_options)
    };

    // Track in-flight downloads, builds, etc., across resolutions.
    let in_flight = InFlight::default();

    // Determine whether to enable build isolation.
    let environment;
    let build_isolation = if no_build_isolation {
        environment = PythonEnvironment::from_interpreter(interpreter.clone());
        BuildIsolation::Shared(&environment)
    } else if no_build_isolation_package.is_empty() {
        BuildIsolation::Isolated
    } else {
        environment = PythonEnvironment::from_interpreter(interpreter.clone());
        BuildIsolation::SharedPackage(&environment, &no_build_isolation_package)
    };

    // Don't enforce hashes in `pip compile`.
    let build_hashes = HashStrategy::None;
    let build_constraints = Constraints::from_requirements(
        build_constraints
            .iter()
            .map(|constraint| constraint.requirement.clone()),
    );

    let build_dispatch = BuildDispatch::new(
        &client,
        &cache,
        build_constraints,
        &interpreter,
        &index_locations,
        &flat_index,
        &dependency_metadata,
        &source_index,
        &git,
        &capabilities,
        &in_flight,
        index_strategy,
        &config_settings,
        build_isolation,
        link_mode,
        &build_options,
        &build_hashes,
        exclude_newer,
        LowerBound::Warn,
        sources,
        concurrency,
    );

    let options = OptionsBuilder::new()
        .resolution_mode(resolution_mode)
        .prerelease_mode(prerelease_mode)
        .dependency_mode(dependency_mode)
        .exclude_newer(exclude_newer)
        .index_strategy(index_strategy)
        .build();

    // Resolve the requirements.
    let resolution = match operations::resolve(
        requirements,
        constraints,
        overrides,
        dev,
        source_trees,
        project,
        None,
        &extras,
        preferences,
        EmptyInstalledPackages,
        &hasher,
        &Reinstall::None,
        &upgrade,
        tags.as_deref(),
        resolver_env.clone(),
        python_requirement,
        &client,
        &flat_index,
        &top_level_index,
        &build_dispatch,
        concurrency,
        options,
        Box::new(DefaultResolveLogger),
        printer,
    )
    .await
    {
        Ok(resolution) => resolution,
        Err(operations::Error::Resolve(uv_resolver::ResolveError::NoSolution(err))) => {
            diagnostics::no_solution(&err);
            return Ok(ExitStatus::Failure);
        }
        Err(operations::Error::Resolve(uv_resolver::ResolveError::DownloadAndBuild(dist, err))) => {
            diagnostics::download_and_build(dist, err);
            return Ok(ExitStatus::Failure);
        }
        Err(operations::Error::Resolve(uv_resolver::ResolveError::Build(dist, err))) => {
            diagnostics::build(dist, err);
            return Ok(ExitStatus::Failure);
        }
        Err(operations::Error::Requirements(uv_requirements::Error::DownloadAndBuild(
            dist,
            err,
        ))) => {
            diagnostics::download_and_build(dist, err);
            return Ok(ExitStatus::Failure);
        }
        Err(operations::Error::Requirements(uv_requirements::Error::Build(dist, err))) => {
            diagnostics::build(dist, err);
            return Ok(ExitStatus::Failure);
        }
        Err(err) => return Err(err.into()),
    };

    // Write the resolved dependencies to the output channel.
    let mut writer = OutputWriter::new(!quiet || output_file.is_none(), output_file);

    if include_header {
        writeln!(
            writer,
            "{}",
            "# This file was autogenerated by uv via the following command:".green()
        )?;
        writeln!(
            writer,
            "{}",
            format!(
                "#    {}",
                cmd(
                    include_index_url,
                    include_find_links,
                    custom_compile_command
                )
            )
            .green()
        )?;
    }

    if include_marker_expression {
        if let Some(marker_env) = resolver_env.marker_environment() {
            let relevant_markers = resolution.marker_tree(&top_level_index, marker_env)?;
            if let Some(relevant_markers) = relevant_markers.contents() {
                writeln!(
                    writer,
                    "{}",
                    "# Pinned dependencies known to be valid for:".green()
                )?;
                writeln!(writer, "{}", format!("#    {relevant_markers}").green())?;
            }
        }
    }

    let mut wrote_preamble = false;

    // If necessary, include the `--index-url` and `--extra-index-url` locations.
    if include_index_url {
        if let Some(index) = index_locations.default_index() {
            writeln!(writer, "--index-url {}", index.url().verbatim())?;
            wrote_preamble = true;
        }
        let mut seen = FxHashSet::default();
        for extra_index in index_locations.implicit_indexes() {
            if seen.insert(extra_index.url()) {
                writeln!(writer, "--extra-index-url {}", extra_index.url().verbatim())?;
                wrote_preamble = true;
            }
        }
    }

    // If necessary, include the `--find-links` locations.
    if include_find_links {
        for flat_index in index_locations.flat_indexes() {
            writeln!(writer, "--find-links {}", flat_index.url().verbatim())?;
            wrote_preamble = true;
        }
    }

    // If necessary, include the `--no-binary` and `--only-binary` options.
    if include_build_options {
        match build_options.no_binary() {
            NoBinary::None => {}
            NoBinary::All => {
                writeln!(writer, "--no-binary :all:")?;
                wrote_preamble = true;
            }
            NoBinary::Packages(packages) => {
                for package in packages {
                    writeln!(writer, "--no-binary {package}")?;
                    wrote_preamble = true;
                }
            }
        }
        match build_options.no_build() {
            NoBuild::None => {}
            NoBuild::All => {
                writeln!(writer, "--only-binary :all:")?;
                wrote_preamble = true;
            }
            NoBuild::Packages(packages) => {
                for package in packages {
                    writeln!(writer, "--only-binary {package}")?;
                    wrote_preamble = true;
                }
            }
        }
    }

    // If we wrote an index, add a newline to separate it from the requirements
    if wrote_preamble {
        writeln!(writer)?;
    }

    write!(
        writer,
        "{}",
        DisplayResolutionGraph::new(
            &resolution,
            &resolver_env,
            &no_emit_packages,
            generate_hashes,
            include_extras,
            include_markers || universal,
            include_annotations,
            include_index_annotation,
            annotation_style,
        )
    )?;

    // If any "unsafe" packages were excluded, notify the user.
    let excluded = no_emit_packages
        .into_iter()
        .filter(|name| resolution.contains(name))
        .collect::<Vec<_>>();
    if !excluded.is_empty() {
        writeln!(writer)?;
        writeln!(
            writer,
            "{}",
            "# The following packages were excluded from the output:".green()
        )?;
        for package in excluded {
            writeln!(writer, "# {package}")?;
        }
    }

    // Commit the output to disk.
    writer.commit().await?;

    // Notify the user of any resolution diagnostics.
    operations::diagnose_resolution(resolution.diagnostics(), printer)?;

    Ok(ExitStatus::Success)
}

/// Format the uv command used to generate the output file.
#[allow(clippy::fn_params_excessive_bools)]
fn cmd(
    include_index_url: bool,
    include_find_links: bool,
    custom_compile_command: Option<String>,
) -> String {
    if let Some(cmd_str) = custom_compile_command {
        return cmd_str;
    }
    let args = env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().to_string())
        .scan(None, move |skip_next, arg| {
            if matches!(skip_next, Some(true)) {
                // Reset state; skip this iteration.
                *skip_next = None;
                return Some(None);
            }

            // Skip any index URLs, unless requested.
            if !include_index_url {
                if arg.starts_with("--extra-index-url=") || arg.starts_with("--index-url=") {
                    // Reset state; skip this iteration.
                    *skip_next = None;
                    return Some(None);
                }

                // Mark the next item as (to be) skipped.
                if arg == "--index-url" || arg == "--extra-index-url" {
                    *skip_next = Some(true);
                    return Some(None);
                }
            }

            // Skip any `--find-links` URLs, unless requested.
            if !include_find_links {
                // Always skip the `--find-links` and mark the next item to be skipped
                if arg == "--find-links" || arg == "-f" {
                    *skip_next = Some(true);
                    return Some(None);
                }

                // Skip only this argument if option and value are together
                if arg.starts_with("--find-links=") || arg.starts_with("-f") {
                    // Reset state; skip this iteration.
                    *skip_next = None;
                    return Some(None);
                }
            }

            // Always skip the `--upgrade` flag.
            if arg == "--upgrade" || arg == "-U" {
                *skip_next = None;
                return Some(None);
            }

            // Always skip the `--upgrade-package` and mark the next item to be skipped
            if arg == "--upgrade-package" || arg == "-P" {
                *skip_next = Some(true);
                return Some(None);
            }

            // Skip only this argument if option and value are together
            if arg.starts_with("--upgrade-package=") || arg.starts_with("-P") {
                // Reset state; skip this iteration.
                *skip_next = None;
                return Some(None);
            }

            // Always skip the `--quiet` flag.
            if arg == "--quiet" || arg == "-q" {
                *skip_next = None;
                return Some(None);
            }

            // Always skip the `--verbose` flag.
            if arg == "--verbose" || arg == "-v" {
                *skip_next = None;
                return Some(None);
            }

            // Return the argument.
            Some(Some(arg))
        })
        .flatten()
        .join(" ");
    format!("uv {args}")
}
