use argh::FromArgs;
use reqwest::blocking::Client;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
};
use tempfile::TempDir;
use tracing::{info, trace};
use url::Url;
use walkdir::WalkDir;

const API_BASE: &str = "https://crates.io/api/v1/";

type Error = Box<dyn std::error::Error>;
type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
struct PatchArg {
    name: String,
    path: PathBuf,
}

impl FromStr for PatchArg {
    type Err = Error;

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

/// Test reverse dependencies with an updated crate version
#[derive(Debug, FromArgs)]
struct CommandLineOpts {
    /// the name of the crate to crater
    #[argh(positional)]
    crate_name: String,

    /// the version of the crate to look for reverse dependencies of
    #[argh(option)]
    version: String,

    /// the crate name and path to use to patch, formatted as `name=path`
    #[argh(option)]
    patch: Vec<PatchArg>,

    /// the directory to compile in.
    #[argh(option, default = "TempDir::new().unwrap().into_path()")]
    working_dir: PathBuf,

    /// checkout the repository instead of using a released version
    #[argh(switch)]
    use_git: bool,

    /// alternate command to run as the first step
    #[argh(option)]
    pre_command: Option<String>,

    /// alternate command to run as the second step
    #[argh(option)]
    post_command: Option<String>,
}

struct Opts {
    api_base: Url,
    crate_name: String,
    version: String,
    patch: Vec<PatchArg>,
    working_dir: PathBuf,
    use_git: bool,
    pre_command: Option<String>,
    post_command: Option<String>,
}

impl Opts {
    fn enhance(cmd: CommandLineOpts) -> Result<Self> {
        let api_base = Url::parse(API_BASE)?;
        let CommandLineOpts {
            crate_name,
            version,
            patch,
            working_dir,
            use_git,
            pre_command,
            post_command,
        } = cmd;
        Ok(Self {
            api_base,
            crate_name,
            version,
            patch,
            working_dir,
            use_git,
            pre_command,
            post_command,
        })
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let original_dir = env::current_dir()?;

    let opts = Opts::enhance(argh::from_env())?;

    let client = reqwest::blocking::Client::builder()
        .user_agent("mini-crater")
        .build()?;

    info!("Working in {}", opts.working_dir.display());

    setup_cargo_config(&opts)?;

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

    let all_info = fetch_crate_info(&opts, &client, &name_and_download_counts)?;
    // dbg!(&all_info);
    info!(
        "Found {} reverse dependencies with full information ({} with repositories)",
        all_info.crates.len(),
        all_info
            .crates
            .iter()
            .filter(|c| c.repository.is_some())
            .count(),
    );

    let mut crates = all_info.crates;
    crates.sort_by_key(|c| c.downloads);
    crates.reverse();
    let statistics = run_experiment(&opts, &crates)?;
    dbg!(&statistics);

    env::set_current_dir(original_dir)?;

    let mut results = File::create("results.json")?;
    serde_json::to_writer(&mut results, &statistics)?;

    Ok(())
}

/// Use a shared target directory to reduce (but not eliminate) disk
/// space usage and needless rebuilds.
fn setup_cargo_config(opts: &Opts) -> Result<()> {
    let dir = opts.working_dir.join(".cargo");
    fs::create_dir_all(&dir)?;

    let path = dir.join("config");
    let config = format!(
        "[build]
         target-dir = '{}/target'",
        opts.working_dir.display(),
    );

    fs::write(path, config).map_err(Into::into)
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ReverseDependencies {
    dependencies: Vec<Dependency>,
    versions: Vec<Version>,
    meta: Meta,
}

#[derive(Debug, Deserialize, Serialize)]
struct Dependency {
    id: u64,
    version_id: u64,
    req: String, // ^0.6.10
    downloads: u64,

    #[serde(flatten)]
    _other: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Version {
    id: u64,
    #[serde(rename = "crate")]
    name: String,
    num: String,
    downloads: u64,

    #[serde(flatten)]
    _other: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Meta {
    total: u64,

    #[serde(flatten)]
    _other: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct ReverseDependenciesQuery {
    page: u64,
    per_page: u64, // max of 100
}

/// Find all crates that use the target crate
fn fetch_reverse_dependencies(opts: &Opts, client: &Client) -> Result<ReverseDependencies> {
    let cache_file = opts.working_dir.join("reverse_dependencies.json");

    use_cached_file_or_else(cache_file, || {
        let mut q = ReverseDependenciesQuery {
            page: 1,
            per_page: 100,
        };
        let mut fused_reverse_dependencies = ReverseDependencies::default();

        loop {
            let revdep_path = format!("./crates/{}/reverse_dependencies", &opts.crate_name);
            let mut revdep_url = opts.api_base.join(&revdep_path)?;

            let qs = serde_urlencoded::to_string(&q)?;
            revdep_url.set_query(Some(&qs));

            trace!("Making request to {}", revdep_url);
            let response = client.get(revdep_url).send()?;
            let mut chunk: ReverseDependencies = serde_json::from_slice(&response.bytes()?)?;

            fused_reverse_dependencies
                .dependencies
                .append(&mut chunk.dependencies);
            fused_reverse_dependencies
                .versions
                .append(&mut chunk.versions);
            fused_reverse_dependencies.meta = chunk.meta;

            if q.page * q.per_page < fused_reverse_dependencies.meta.total {
                q.page += 1;
            } else {
                break;
            }
        }

        serde_json::to_vec(&fused_reverse_dependencies).map_err(Into::into)
    })
}

#[derive(Debug)]
struct NameAndDownloadCount {
    name: String,
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
        versions,
        meta: _,
    } = reverse_dependencies;
    let mut version_map: BTreeMap<_, _> = versions.into_iter().map(|v| (v.id, v)).collect();

    // TODO: better semver string application
    dependencies
        .into_iter()
        .filter(|d| d.req.contains(&opts.version))
        .map(|d| {
            let version = version_map
                .remove(&d.version_id)
                .expect("Version key was missing");
            let name = version.name;
            let downloads = d.downloads;

            NameAndDownloadCount { name, downloads }
        })
        .collect()
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct CrateInfo {
    crates: Vec<Crate>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Crate {
    name: String,
    max_version: String,
    downloads: u64,
    repository: Option<String>,

    #[serde(flatten)]
    _other: BTreeMap<String, serde_json::Value>,
}

/// Gets the full information for the crates, including the repository link
fn fetch_crate_info(
    opts: &Opts,
    client: &Client,
    name_and_download_counts: &[NameAndDownloadCount],
) -> Result<CrateInfo> {
    let cache_file = opts.working_dir.join("full_information.json");

    use_cached_file_or_else(cache_file, || {
        let mut fused_crate_info = CrateInfo::default();
        let crate_url = opts.api_base.join("./crates")?;

        // crates.io limits this to 10 and doesn't handle the paging
        // quite right, so we do this ourselves.
        for chunk in name_and_download_counts.chunks(10) {
            let mut crate_url = crate_url.clone();
            let mut query = crate_url.query_pairs_mut();
            for info in chunk {
                query.append_pair("ids[]", &info.name);
            }
            drop(query);

            trace!("Making request to {}", crate_url);
            let response = client.get(crate_url).send()?;
            let mut chunk: CrateInfo = serde_json::from_slice(&response.bytes()?)?;

            fused_crate_info.crates.append(&mut chunk.crates);
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
        env::set_current_dir(&experiments_dir)?;

        // Create our calling project
        if Path::new(&c.name).exists() {
            trace!("Reusing experiment {}", c.name);
        } else {
            let mut cmd = Command::new("cargo");
            cmd.args(&["new", "--bin", "--name"])
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

                    cmd.arg("clone").arg(repository_url).arg(&repository_path);
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
        let file = fs::read(&cargo_toml_path)?;
        let cargo_toml: CargoToml = toml::from_slice(&file)?;

        if let Some(package) = cargo_toml.package {
            if package.name == name {
                cargo_toml_path.pop();
                return Ok(cargo_toml_path);
            }
        }
    }

    Err(format!(
        "Could not find a suitable Cargo toml for {} in {}",
        name,
        root_dir.display()
    )
    .into())
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
            cmd.args(&["build"]);
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
