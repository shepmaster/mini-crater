use clap::{
    builder::{styling::AnsiColor, Styles},
    Parser,
};
use color_eyre::{eyre::bail, Result};
use crates_io_api::{Crate, CratesQuery, ReverseDependencies, SyncClient};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    env,
    ffi::OsStr,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    time::Duration,
};
use tempfile::TempDir;
use tracing::{info, level_filters::LevelFilter, trace};
use tracing_subscriber::EnvFilter;
use walkdir::WalkDir;

#[derive(Debug, Clone)]
struct PatchArg {
    name: String,
    path: PathBuf,
}

impl FromStr for PatchArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, path) = s
            .split_once("=")
            .ok_or_else(|| "Not a valid patch name/path pair")?;
        Ok(Self {
            name: name.into(),
            path: path.into(),
        })
    }
}

const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Blue.on_default().bold())
    .usage(AnsiColor::Blue.on_default().bold())
    .literal(AnsiColor::White.on_default())
    .placeholder(AnsiColor::Green.on_default());

/// Test reverse dependencies with an updated crate version
#[derive(Debug, Parser)]
#[clap(styles = HELP_STYLES)]
struct Opts {
    /// the name of the crate to crater
    crate_name: String,

    /// the version of the crate to look for reverse dependencies
    /// of. Performs simple substring comparison
    #[arg(long, short)]
    version: String,

    /// the crate name and path to use to patch, formatted as `name=path`
    #[arg(long, short)]
    patch: Vec<PatchArg>,

    /// the directory to compile in.
    #[arg(long, short, default_value = TempDir::new().unwrap().path().to_string_lossy().to_string())]
    working_dir: PathBuf,

    /// checkout the repository for dependant instead of using a
    /// released version
    #[arg(long, short = 'g')]
    use_git: bool,

    /// alternate command to run as the first step
    #[arg(long, alias = "pre")]
    pre_command: Option<String>,

    /// alternate command to run as the second step
    #[arg(long, alias = "post")]
    post_command: Option<String>,

    /// dry run, don't actually run the experiments
    #[arg(long, short = 'n')]
    dry_run: bool,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let opts = Opts::parse();

    let original_dir = env::current_dir()?;
    info!("Working in {}", opts.working_dir.display());

    let rate_limit = Duration::from_secs(1);
    let client = crates_io_api::SyncClient::new("mini-crater", rate_limit)?;

    setup_cargo_config(&opts.working_dir)?;

    let reverse_dependencies = fetch_reverse_dependencies(&opts, &client)?;
    // dbg!(&reverse_dependencies);
    info!(
        "Found {} total reverse dependencies",
        reverse_dependencies.dependencies.len(),
    );

    let name_and_download_counts = compute_name_and_download_counts(&opts, reverse_dependencies);
    // dbg!(&name_and_download_counts);
    info!(
        "Found {} reverse dependencies for version {}",
        name_and_download_counts.len(),
        opts.version,
    );

    let mut crates = fetch_crate_info(&opts, &client, &name_and_download_counts)?;
    // dbg!(&all_info);
    info!(
        "Found {} reverse dependencies with full information ({} with repositories)",
        crates.len(),
        crates.iter().filter(|c| c.repository.is_some()).count(),
    );

    crates.sort_by_key(|c| c.downloads);
    crates.reverse();
    let statistics = run_experiment(&opts, &crates)?;
    dbg!(&statistics);

    env::set_current_dir(original_dir)?;

    let mut results = File::create("results.json")?;
    serde_json::to_writer(&mut results, &statistics)?;

    Ok(())
}

/// Use a shared target directory to reduce (but not eliminate) disk space usage and needless
/// rebuilds.
fn setup_cargo_config(working_dir: &Path) -> Result<()> {
    let dir = working_dir.join(".cargo");
    fs::create_dir_all(&dir)?;

    let path = dir.join("config");
    let config = format!(
        "[build]
         target-dir = '{}/target'",
        working_dir.display(),
    );

    fs::write(path, config).map_err(Into::into)
}

/// Find all crates that use the target crate
fn fetch_reverse_dependencies(opts: &Opts, client: &SyncClient) -> Result<ReverseDependencies> {
    let cache_file = opts.working_dir.join("reverse_dependencies.json");
    use_cached_file_or_else(cache_file, || {
        let reverse_dependencies = client.crate_reverse_dependencies(&opts.crate_name)?;
        serde_json::to_vec(&reverse_dependencies).map_err(Into::into)
    })
}

#[derive(Debug)]
struct NameAndDownloadCount {
    name: String,
    #[allow(unused)]
    downloads: u64,
}

/// Associate a name to the specific crate version that depends on the
/// target crate.
fn compute_name_and_download_counts(
    opts: &Opts,
    reverse_dependencies: ReverseDependencies,
) -> Vec<NameAndDownloadCount> {
    let ReverseDependencies {
        dependencies,
        meta: _,
    } = reverse_dependencies;

    // TODO: better semver string application
    dependencies
        .into_iter()
        .filter(|d| d.dependency.req.contains(&opts.version))
        .map(|d| NameAndDownloadCount {
            name: d.crate_version.crate_name,
            downloads: d.crate_version.downloads,
        })
        .collect()
}

/// Gets the full information for the crates, including the repository link
fn fetch_crate_info(
    opts: &Opts,
    client: &SyncClient,
    name_and_download_counts: &[NameAndDownloadCount],
) -> Result<Vec<Crate>> {
    let cache_file = opts.working_dir.join("full_information.json");
    let mut fused_crate_info = Vec::new();
    use_cached_file_or_else(cache_file, || {
        // crates.io limits this to 10 and doesn't handle the paging quite right, so we do this
        // ourselves.
        for chunk in name_and_download_counts.chunks(10) {
            let query = CratesQuery::builder()
                .ids(chunk.iter().map(|c| c.name.clone()).collect())
                .build();
            trace!("Making request for {query:?}");
            let mut response = client.crates(query)?;
            fused_crate_info.append(&mut response.crates);
        }
        serde_json::to_vec(&fused_crate_info).map_err(Into::into)
    })
}

fn use_cached_file_or_else<T>(
    cache_filename: PathBuf,
    f: impl FnOnce() -> Result<Vec<u8>>,
) -> Result<T>
where
    T: DeserializeOwned,
{
    let data = match fs::read(&cache_filename) {
        Ok(file) => {
            trace!("Using cached information from {}", cache_filename.display());
            file
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let data = f()?;
            fs::write(&cache_filename, &data)?;
            trace!("Cached information to {}", cache_filename.display());
            data
        }
        Err(e) => return Err(e.into()),
    };

    serde_json::from_slice(&data).map_err(Into::into)
}

#[derive(Debug, Default, Serialize)]
struct Statistics {
    setup_successes: Vec<String>,
    setup_failures: Vec<String>,
    original_successes: Vec<String>,
    original_failures: Vec<String>,
    experiment_successes: Vec<String>,
    experiment_failures: Vec<String>,
}

fn run_experiment(opts: &Opts, crates: &[Crate]) -> Result<Statistics> {
    let mut statistics = Statistics::default();

    let experiments_dir = opts.working_dir.join("experiments");
    fs::create_dir_all(&experiments_dir)?;

    let checkouts_dir = opts.working_dir.join("checkouts");
    fs::create_dir_all(&checkouts_dir)?;

    let mut cargo_toml_patch = "\n[patch.crates-io]\n".to_string();
    for p in &opts.patch {
        use std::fmt::Write;
        writeln!(
            cargo_toml_patch,
            "{name} = {{ path = '{path}' }}",
            name = p.name,
            path = p.path.display(),
        )
        .unwrap();
    }

    for c in crates {
        println!("Beginning experiment for {}", c.name);
        if opts.dry_run {
            continue;
        }
        env::set_current_dir(&experiments_dir)?;

        // Create our calling project
        if Path::new(&c.name).exists() {
            trace!("Reusing experiment {}", c.name);
        } else {
            let mut cmd = Command::new("cargo");
            cmd.args(&["new", "--quiet", "--bin", "--name"])
                .arg(format!("experiment-{}", c.name))
                .arg(&c.name);
            trace!("Creating experiment {} with {:?}", c.name, cmd);
            let status = cmd.status()?;
            assert!(status.success());
        }

        let this_experiment_dir = experiments_dir.join(&c.name);
        env::set_current_dir(&this_experiment_dir)?;

        let cargo_toml = this_experiment_dir.join("Cargo.toml");
        let mut cargo_toml_contents = fs::read_to_string(&cargo_toml)?;

        let repository_info = match (opts.use_git, &c.repository) {
            (true, Some(repo)) => Some((repo, checkouts_dir.join(&c.name))),
            (true, None) => {
                info!("Experiment {} has no repository to clone", c.name);
                statistics.setup_failures.push(c.name.to_string());
                continue;
            }
            (false, _) => None,
        };

        // TODO: Something better with parsing the TOML
        let dep_prefix = format!("{name} =", name = c.name);
        if cargo_toml_contents.contains(&dep_prefix) {
            trace!("Reusing dependency for experiment {}", c.name);
        } else {
            if let Some((repository_url, repository_path)) = &repository_info {
                trace!("Adding git dependency for experiment {}", c.name);

                if repository_path.exists() {
                    trace!(
                        "Reusing experiment {} repository at {}",
                        c.name,
                        repository_path.display(),
                    );
                } else {
                    let mut cmd = Command::new("git");

                    cmd.args(&["clone", "--quiet"])
                        .arg(repository_url)
                        .arg(&repository_path);
                    trace!("Cloning experiment repository {} with {:?}", c.name, cmd);
                    let status = cmd.status()?;

                    if !status.success() {
                        info!("Experiment {} could not clone the repository", c.name);
                        statistics.setup_failures.push(c.name.to_string());
                        continue;
                    }
                }

                let inner_repository_path = match find_cargo_toml(&repository_path, &c.name) {
                    Ok(p) => p,
                    Err(_) => {
                        statistics.setup_failures.push(c.name.to_string());
                        continue;
                    }
                };

                let dep = format!(
                    "{name} = {{ path = '{repository_path}' }}\n",
                    name = c.name,
                    repository_path = inner_repository_path.display(),
                );
                cargo_toml_contents.push_str(&dep);
                fs::write(&cargo_toml, &cargo_toml_contents)?;
            } else {
                trace!("Adding crates.io dependency for experiment {}", c.name);
                let dep = format!(
                    "{name} = '={version}'\n",
                    name = c.name,
                    version = c.max_version,
                );
                cargo_toml_contents.push_str(&dep);
                fs::write(&cargo_toml, &cargo_toml_contents)?;
            }
        }

        statistics.setup_successes.push(c.name.to_string());

        let repository_path = repository_info.as_ref().map(|(_, p)| p);

        let mut pre_command = prepare_command(
            opts.pre_command.as_deref(),
            &opts.working_dir,
            &c.name,
            repository_path.as_ref(),
        );
        trace!(
            "Running pre command for experiment {}: {:?}",
            c.name,
            pre_command
        );
        let status = pre_command.status()?;
        if status.success() {
            statistics.original_successes.push(c.name.to_string());
        } else {
            info!("Experiment {} failed original build", c.name);
            statistics.original_failures.push(c.name.to_string());
            continue;
        }

        if cargo_toml_contents.contains("patch.crates-io") {
            trace!("Reusing patch for experiment {}", c.name);
        } else {
            trace!("Adding patch for experiment {}", c.name);

            cargo_toml_contents.push_str(&cargo_toml_patch);
            fs::write(&cargo_toml, &cargo_toml_contents)?;
        }

        let mut post_command = prepare_command(
            opts.post_command.as_deref(),
            &opts.working_dir,
            &c.name,
            repository_path.as_ref(),
        );
        trace!(
            "Running post command for experiment {}: {:?}",
            c.name,
            post_command
        );
        let status = post_command.status()?;
        if status.success() {
            statistics.experiment_successes.push(c.name.to_string());
        } else {
            info!("Experiment {} failed second build", c.name);
            statistics.experiment_failures.push(c.name.to_string());
            continue;
        }
    }

    Ok(statistics)
}

#[derive(Debug, Deserialize)]
struct CargoToml {
    package: Option<CargoTomlPackage>,
}

#[derive(Debug, Deserialize)]
struct CargoTomlPackage {
    name: String,
}

fn find_cargo_toml(root_dir: &Path, name: &str) -> Result<PathBuf> {
    let cargo_tomls = WalkDir::new(root_dir)
        .into_iter()
        .flatten()
        .map(|e| e.into_path())
        .filter(|p| {
            p.file_name()
                .map_or(false, |n| n.eq_ignore_ascii_case("Cargo.toml"))
        });

    for mut cargo_toml_path in cargo_tomls {
        trace!(
            "Checking {} to see if it is for {}",
            cargo_toml_path.display(),
            name
        );
        let file = fs::read_to_string(&cargo_toml_path)?;
        let cargo_toml: CargoToml = toml::from_str(&file)?;

        if let Some(package) = cargo_toml.package {
            if package.name == name {
                cargo_toml_path.pop();
                return Ok(cargo_toml_path);
            }
        }
    }

    bail!(
        "Could not find a suitable Cargo toml for {} in {}",
        name,
        root_dir.display()
    )
}

fn prepare_command(
    user: Option<&str>,
    working_dir: impl AsRef<OsStr>,
    experiment_name: impl AsRef<OsStr>,
    experiment_checkout_dir: Option<impl AsRef<OsStr>>,
) -> Command {
    let mut cmd = match user {
        Some(cmd) => Command::new(cmd),
        None => {
            let mut cmd = Command::new("cargo");
            cmd.args(&["build", "--quiet"]);
            cmd
        }
    };

    cmd.env("MINI_CRATER_WORKING_DIR", working_dir)
        .env("MINI_CRATER_EXPERIMENT_NAME", experiment_name);

    if let Some(experiment_checkout_dir) = experiment_checkout_dir {
        cmd.env(
            "MINI_CRATER_EXPERIMENT_CHECKOUT_DIR",
            experiment_checkout_dir,
        );
    }

    cmd
}
