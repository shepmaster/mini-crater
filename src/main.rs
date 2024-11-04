use argh::FromArgs;
use cargo_util_schemas::manifest::{
    InheritableDependency, PackageName, TomlDependency, TomlDetailedDependency, TomlManifest,
};
use futures::{future, FutureExt as _, StreamExt as _, TryStream, TryStreamExt as _};
use ratatui::{
    crossterm::event::{self, KeyCode, KeyEventKind},
    prelude::*,
    widgets,
};
use regex::Regex;
use reqwest::blocking::Client;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::convert::TryFrom;
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
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, trace, warn};
use tracing_subscriber::{prelude::*, EnvFilter};
use url::Url;
use walkdir::WalkDir;

const API_BASE: &str = "https://crates.io/api/v1/";

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
struct PatchArg {
    name: PackageName,
    dep: TomlDependency,
}

impl FromStr for PatchArg {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut keys: BTreeMap<PackageName, TomlDependency> = toml::from_str(s)?;
        let (name, dep) = keys.pop_first().ok_or("No patch provided")?;
        if !keys.is_empty() {
            Err("Too many patches found")?;
        }

        Ok(Self { name, dep })
    }
}

/// Test reverse dependencies with an updated crate version
#[derive(Debug, FromArgs)]
struct CommandLineOpts {
    /// the name of the crate to crater
    #[argh(positional)]
    crate_name: String,

    /// the version of the crate to look for reverse dependencies
    /// of. Performs simple substring comparison
    #[argh(option)]
    version: semver::Version,

    /// a patch entry as found in Cargo.toml. `name = {{ path = "..." }}`
    #[argh(option)]
    patch: Vec<PatchArg>,

    /// the directory to compile in.
    #[argh(option, default = "TempDir::new().unwrap().into_path()")]
    working_dir: PathBuf,

    /// checkout the repository for dependant instead of using a
    /// released version
    #[argh(switch)]
    use_git: bool,

    /// alternate command to run as the first step
    #[argh(option)]
    pre_command: Option<String>,

    /// alternate command to run as the second step
    #[argh(option)]
    post_command: Option<String>,

    /// only run experiments on crates matching this regex
    #[argh(option)]
    select: Option<Regex>,
}

struct Opts {
    api_base: Url,
    crate_name: String,
    version: semver::Version,
    patch: Vec<PatchArg>,
    working_dir: PathBuf,
    use_git: bool,
    pre_command: Option<String>,
    post_command: Option<String>,
    select: Option<Regex>,
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
            select,
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
            select,
        })
    }
}

trait SaturateToU16 {
    fn saturate_to_u16(self) -> u16;
}

impl SaturateToU16 for usize {
    fn saturate_to_u16(self) -> u16 {
        u16::try_from(self).unwrap_or(u16::MAX)
    }
}

mod state {
    use ratatui::{layout::Constraint::*, prelude::*, style::Style, text::Text, widgets};
    use tui_widgets::scrollview;

    use crate::{SaturateToU16, Trace};

    #[derive(Default)]
    pub struct Root {
        pub focus: RootFocus,
        pub progress: Progress,
        pub tracing: Tracing,
    }

    #[derive(Copy, Clone, Default, PartialEq)]
    pub enum RootFocus {
        #[default]
        Progress,
        Tracing,
    }

    impl RootFocus {
        const ALL: [Self; 2] = [Self::Progress, Self::Tracing];

        pub fn nth(&self) -> usize {
            Self::ALL.iter().position(|v| v == self).unwrap()
        }
    }

    #[derive(Default)]
    pub enum Progress {
        #[default]
        Initialized,

        RunExperiments(RunExperiments),
    }

    pub struct RunExperiments {
        pub experiments: Vec<RunExperiment>,
        pub focus: RunExperimentsFocus,
        pub crate_list: widgets::ListState,
    }

    impl RunExperiments {
        pub fn new(crates: Vec<super::Crate>) -> Self {
            let experiments = crates
                .into_iter()
                .map(|c| RunExperiment {
                    name: c.name,
                    progress: Default::default(),
                    original_output: Default::default(),
                    output: Default::default(),
                    crate_detail: Default::default(),
                })
                .collect();

            Self {
                experiments,
                focus: Default::default(),
                crate_list: Default::default(),
            }
        }
    }

    #[derive(Debug, Copy, Clone, Default, PartialEq)]
    pub enum RunExperimentsFocus {
        #[default]
        List,
        Detail,
    }

    impl RunExperimentsFocus {
        const ALL: [Self; 2] = [Self::List, Self::Detail];

        pub fn next(&self) -> Self {
            Self::ALL
                .iter()
                .cycle()
                .skip_while(|&f| f != self)
                .nth(1)
                .copied()
                .unwrap()
        }

        pub fn is_list(&self) -> bool {
            matches!(self, Self::List)
        }

        pub fn is_detail(&self) -> bool {
            matches!(self, Self::Detail)
        }
    }

    impl widgets::Widget for &mut RunExperiments {
        fn render(self, area: Rect, buf: &mut Buffer) {
            let RunExperiments {
                experiments,
                focus,
                crate_list,
            } = self;

            let symbol = ">>";
            // Doesn't handle UTF-8...
            let name_width = experiments
                .iter()
                .map(|e| e.name.len())
                .max()
                .unwrap_or(0)
                .saturate_to_u16();
            let border_width = 2;
            let symbol_width = symbol.len().saturate_to_u16();
            let right_padding = symbol_width;
            let name_width = name_width + border_width + symbol_width + right_padding;

            let [left, right] = Layout::horizontal([Max(name_width), Fill(1)]).areas(area);

            let selected_border = |selected| {
                if selected {
                    widgets::BorderType::Double
                } else {
                    widgets::BorderType::Plain
                }
            };

            let left_block = widgets::Block::bordered()
                .border_type(selected_border(focus.is_list()))
                .title("Crates");

            let crates = widgets::List::new(&*experiments)
                .block(left_block)
                .highlight_symbol(symbol)
                .highlight_spacing(widgets::HighlightSpacing::Always);

            StatefulWidget::render(crates, left, buf, crate_list);

            let right_block = widgets::Block::bordered()
                .border_type(selected_border(focus.is_detail()))
                .title("Details");
            let right_inner = right_block.inner(right);

            right_block.render(right, buf);

            match crate_list.selected().and_then(|i| experiments.get_mut(i)) {
                Some(experiment) => experiment.render(right_inner, buf),

                None => {
                    let details = widgets::Paragraph::new("Select an experiment");
                    details.render(right_inner, buf);
                }
            };
        }
    }

    pub struct RunExperiment {
        pub name: String,
        pub progress: RunExperimentProgress,
        pub original_output: RunExperimentOutput,
        pub output: RunExperimentOutput,
        pub crate_detail: scrollview::ScrollViewState,
    }

    impl widgets::Widget for &mut RunExperiment {
        fn render(self, area: Rect, buf: &mut Buffer) {
            let RunExperiment {
                original_output,
                output,
                crate_detail,
                ..
            } = self;

            fn one_output(output: &RunExperimentOutput) -> [widgets::Paragraph<'_>; 2] {
                [
                    widgets::Paragraph::new(&*output.stdout)
                        .block(widgets::Block::bordered().title("Stdout")),
                    widgets::Paragraph::new(&*output.stderr)
                        .block(widgets::Block::bordered().title("Stderr")),
                ]
            }
            let [a, b] = one_output(original_output);
            let [c, d] = one_output(output);

            let widgets = [a, b, c, d];
            let width = widgets
                .each_ref()
                .iter()
                .map(|p| p.line_width())
                .max()
                .unwrap_or(0)
                .saturate_to_u16();
            let heights = widgets
                .each_ref()
                .map(|p| p.line_count(width).saturate_to_u16());
            let height = heights.iter().sum();

            let width = u16::max(width, area.width);
            let height = u16::max(height, area.height);

            let size = Size { width, height };
            let mut sv = scrollview::ScrollView::new(size);

            let layouts = Layout::vertical(heights.map(Constraint::Length)).areas::<4>(sv.area());

            for (w, l) in widgets.iter().zip(layouts) {
                sv.render_widget(w, l);
            }

            sv.render(area, buf, crate_detail);
        }
    }

    #[derive(Debug, Copy, Clone, Default)]
    pub enum RunExperimentProgress {
        #[default]
        Pending,
        Skipped,
        Running,
        Success,
        OriginalError,
        Error,
    }

    #[derive(Debug, Clone, Default)]
    pub struct RunExperimentOutput {
        pub stdout: String,
        pub stderr: String,
    }

    impl RunExperimentOutput {
        pub fn from_bytes(stdout: Vec<u8>, stderr: Vec<u8>) -> Self {
            let stdout = String::from_utf8_lossy(&stdout).into_owned();
            let stderr = String::from_utf8_lossy(&stderr).into_owned();

            Self { stdout, stderr }
        }
    }

    impl<'a> From<&'a RunExperiment> for Text<'a> {
        fn from(value: &'a RunExperiment) -> Self {
            let mut t = Text::from(&*value.name);

            t = match value.progress {
                RunExperimentProgress::Pending => t,
                RunExperimentProgress::Skipped => t.style(Style::new().dim()),
                RunExperimentProgress::Running => t.style(Style::new().cyan()),
                RunExperimentProgress::Success => t.style(Style::new().green()),
                RunExperimentProgress::OriginalError => t.style(Style::new().yellow()),
                RunExperimentProgress::Error => t.style(Style::new().red()),
            };

            t
        }
    }

    #[derive(Default)]
    pub struct Tracing {
        pub traces: Vec<Trace>,
        pub offset: scrollview::ScrollViewState,
    }

    impl Tracing {
        pub fn push(&mut self, t: Trace) {
            self.traces.push(t);
        }

        pub fn messages(&self) -> String {
            self.traces.iter().fold(String::new(), |mut acc, s| {
                acc.push_str(&s.message);
                acc.push('\n');
                acc
            })
        }
    }
}

#[derive(Debug)]
enum Message {
    RunExperimentsStart {
        crates: Vec<Crate>,
    },

    ExperimentSkip {
        name: String,
    },

    ExperimentSetupFailure {
        name: String,
    },

    ExperimentSetupSuccess {
        name: String,
    },

    ExperimentOriginalOutput {
        name: String,
        output: state::RunExperimentOutput,
    },

    ExperimentOriginalSuccess {
        name: String,
    },

    ExperimentOriginalFailure {
        name: String,
    },

    ExperimentOutput {
        name: String,
        output: state::RunExperimentOutput,
    },

    ExperimentSuccess {
        name: String,
    },

    ExperimentFailure {
        name: String,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let opts = Opts::enhance(argh::from_env())?;

    let (tracing_tx, tracing_rx) = ChannelTracing::new();

    tracing_subscriber::registry()
        .with(tracing_tx)
        .with(EnvFilter::from_default_env())
        .init();

    let terminal_events = event::EventStream::new();

    let (runner_tx, runner_rx) = mpsc::channel(4);
    let runner = tokio::task::spawn_blocking(|| run(opts, runner_tx));

    let meta_events = {
        use MetaEvent::*;

        let tracing_rx = ReceiverStream::new(tracing_rx)
            .map(|e| Ok(Tracing(e)))
            .chain(future::err("tracing events ended".into()).into_stream());
        let runner_rx = ReceiverStream::new(runner_rx).map(|e| Ok(Program(e)));
        let terminal_events = terminal_events.map(|e| Ok(Terminal(e?)));
        let runner = runner.into_stream().map(|e| Ok(Runner(e??)));

        futures::stream_select!(terminal_events, runner_rx, tracing_rx, runner)
    };

    tokio::task::spawn_blocking(|| ui(meta_events)).await?
}

#[derive(Debug)]
enum MetaEvent {
    Terminal(event::Event),
    Program(Message),
    Tracing(Trace),
    Runner(()),
}

fn ui(events: impl TryStream<Ok = MetaEvent, Error = Error> + Unpin) -> Result<()> {
    let mut terminal = ratatui::init();
    terminal.clear()?;

    let r = ui_core(&mut terminal, events);

    ratatui::restore();

    r
}

fn ui_core(
    terminal: &mut Terminal<impl ratatui::backend::Backend>,
    mut events: impl TryStream<Ok = MetaEvent, Error = Error> + Unpin,
) -> Result<()> {
    let mut state = state::Root::default();

    loop {
        terminal.draw(|frame| {
            use ratatui::layout::Constraint::{Length, Min};

            let [top, bottom] = Layout::vertical([Min(0), Length(1)]).areas(frame.area());

            let t = widgets::Tabs::new(vec!["Progress (F1)", "Tracing (F2)"])
                .highlight_style(Style::default().cyan().bold())
                .select(state.focus.nth());

            frame.render_widget(t, bottom);

            match &mut state.focus {
                state::RootFocus::Progress => match &mut state.progress {
                    state::Progress::Initialized => {
                        let greeting = widgets::Paragraph::new("Mini-Crater run started. Hang on.");
                        frame.render_widget(greeting, top);
                    }

                    state::Progress::RunExperiments(run_experiments) => {
                        frame.render_widget(run_experiments, top);
                    }
                },

                state::RootFocus::Tracing => {
                    let messages = state.tracing.messages();
                    let messages = widgets::Paragraph::new(messages);

                    let width = messages.line_width().saturate_to_u16();
                    let height = messages.line_count(width).saturate_to_u16();
                    let size = Size { width, height };

                    let mut sv = tui_widgets::scrollview::ScrollView::new(size);
                    sv.render_widget(messages, sv.area());

                    frame.render_stateful_widget(sv, top, &mut state.tracing.offset);
                }
            }
        })?;

        fn update_experiment(
            state: &mut state::Root,
            name: &str,
            mut f: impl FnMut(&mut state::RunExperiment),
        ) {
            if let state::Progress::RunExperiments(e) = &mut state.progress {
                let state::RunExperiments { experiments, .. } = e;
                for e in experiments {
                    if e.name == name {
                        f(e);
                    }
                }
            }
        }

        fn update_progress(state: &mut state::Root, name: &str, p: state::RunExperimentProgress) {
            update_experiment(state, name, |e| e.progress = p);
        }

        let event = futures::executor::block_on(events.try_next());
        let event = event?.ok_or("event stream ended")?;

        match event {
            MetaEvent::Terminal(event) => {
                if let event::Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::F(1) => state.focus = state::RootFocus::Progress,
                            KeyCode::F(2) => state.focus = state::RootFocus::Tracing,
                            _ => {}
                        }
                    }

                    match &mut state.progress {
                        state::Progress::Initialized => {}

                        state::Progress::RunExperiments(run_experiments) => {
                            let state::RunExperiments {
                                experiments,
                                focus,
                                crate_list,
                            } = run_experiments;

                            let experiment =
                                crate_list.selected().and_then(|i| experiments.get_mut(i));

                            if key.kind == KeyEventKind::Press {
                                match key.code {
                                    KeyCode::Tab => *focus = focus.next(),
                                    _ => {}
                                }

                                match focus {
                                    state::RunExperimentsFocus::List => match key.code {
                                        KeyCode::Down => crate_list.select_next(),
                                        KeyCode::Up => crate_list.select_previous(),
                                        _ => {}
                                    },

                                    state::RunExperimentsFocus::Detail => {
                                        if let Some(e) = experiment {
                                            match key.code {
                                                KeyCode::Down => e.crate_detail.scroll_down(),
                                                KeyCode::Up => e.crate_detail.scroll_up(),
                                                KeyCode::PageDown => {
                                                    e.crate_detail.scroll_page_down()
                                                }
                                                KeyCode::PageUp => e.crate_detail.scroll_page_up(),
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if state.focus == state::RootFocus::Tracing && key.kind == KeyEventKind::Press {
                        match key.code {
                            KeyCode::Down => state.tracing.offset.scroll_down(),
                            KeyCode::Up => state.tracing.offset.scroll_up(),
                            KeyCode::Right => state.tracing.offset.scroll_right(),
                            KeyCode::Left => state.tracing.offset.scroll_left(),
                            KeyCode::PageDown => state.tracing.offset.scroll_page_down(),
                            KeyCode::PageUp => state.tracing.offset.scroll_page_up(),
                            _ => {}
                        }
                    }
                }
            }

            MetaEvent::Program(message) => {
                use state::RunExperimentProgress::*;

                match message {
                    Message::RunExperimentsStart { crates } => {
                        state.progress =
                            state::Progress::RunExperiments(state::RunExperiments::new(crates));
                    }

                    Message::ExperimentSkip { name } => update_progress(&mut state, &name, Skipped),

                    Message::ExperimentSetupFailure { name } => {
                        update_progress(&mut state, &name, OriginalError)
                    }

                    Message::ExperimentSetupSuccess { name } => {
                        update_progress(&mut state, &name, Running)
                    }

                    Message::ExperimentOriginalOutput { name, output } => {
                        update_experiment(&mut state, &name, |e| {
                            e.original_output = output.clone();
                        });
                    }

                    Message::ExperimentOriginalSuccess { name } => {
                        update_progress(&mut state, &name, Running)
                    }

                    Message::ExperimentOriginalFailure { name } => {
                        update_progress(&mut state, &name, OriginalError)
                    }

                    Message::ExperimentOutput { name, output } => {
                        update_experiment(&mut state, &name, |e| {
                            e.output = output.clone();
                        });
                    }

                    Message::ExperimentSuccess { name } => {
                        update_progress(&mut state, &name, Success)
                    }

                    Message::ExperimentFailure { name } => {
                        update_progress(&mut state, &name, Error)
                    }
                }
            }

            MetaEvent::Tracing(t) => {
                state.tracing.push(t);
            }

            MetaEvent::Runner(()) => {}
        }
    }

    Ok(())
}

#[derive(Debug, Default)]
struct Trace {
    message: String,
    fields: BTreeMap<&'static str, String>,
}

struct ChannelTracing(mpsc::Sender<Trace>);

impl<S: tracing::Subscriber> tracing_subscriber::layer::Layer<S> for ChannelTracing {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        #[derive(Default)]
        struct Collector(Trace);

        impl Collector {
            fn add(&mut self, name: &'static str, value: String) {
                if name == "message" {
                    self.0.message = value;
                } else {
                    self.0.fields.insert(name, value);
                }
            }
        }

        impl tracing_subscriber::field::Visit for Collector {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.add(field.name(), value.to_owned());
            }

            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.add(field.name(), format!("{value:?}"));
            }
        }

        let mut collector = Collector::default();
        event.record(&mut collector);

        let _ = self.0.blocking_send(collector.0);
    }
}

impl ChannelTracing {
    fn new() -> (Self, mpsc::Receiver<Trace>) {
        let (t_tx, t_rx) = mpsc::channel(10);
        (ChannelTracing(t_tx), t_rx)
    }
}

fn run(opts: Opts, tx: mpsc::Sender<Message>) -> Result<()> {
    let original_dir = env::current_dir()?;

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

    tx.blocking_send(Message::RunExperimentsStart {
        crates: crates.clone(),
    })?;

    let statistics = run_experiment(&opts, &crates, &tx)?;
    // dbg!(&statistics);

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

    let path = dir.join("config.toml");
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
    req: semver::VersionReq, // ^0.6.10
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
    #[allow(unused)]
    downloads: u64,
}

/// Associate a name to the specific crate version that depends on the
/// target crate.
fn compute_name_and_download_counts(
    opts: &Opts,
    reverse_dependencies: ReverseDependencies,
) -> Vec<NameAndDownloadCount> {
    use semver::Op;

    let ReverseDependencies {
        mut dependencies,
        versions,
        meta: _,
    } = reverse_dependencies;
    let mut version_map: BTreeMap<_, _> = versions.into_iter().map(|v| (v.id, v)).collect();

    for d in &mut dependencies {
        for c in &mut d.req.comparators {
            if c.op == Op::Exact {
                warn!(
                    "ignoring exact semver requirement in {}",
                    &version_map[&d.version_id].name
                );
                c.op = Op::Caret;
            }
        }
    }

    dependencies
        .into_iter()
        .filter(|d| d.req.matches(&opts.version))
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Crate {
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

fn run_experiment(opts: &Opts, crates: &[Crate], tx: &mpsc::Sender<Message>) -> Result<Statistics> {
    let mut statistics = Statistics::default();

    let experiments_dir = opts.working_dir.join("experiments");
    fs::create_dir_all(&experiments_dir)?;

    let checkouts_dir = opts.working_dir.join("checkouts");
    fs::create_dir_all(&checkouts_dir)?;

    for c in crates {
        if let Some(select) = &opts.select {
            if !select.is_match(&c.name) {
                trace!("Skipping {} as it does not match the regex", c.name);
                tx.blocking_send(Message::ExperimentSkip {
                    name: c.name.clone(),
                })?;
                continue;
            }
        }

        let c_pkg_name = PackageName::new(c.name.to_owned())?;

        env::set_current_dir(&experiments_dir)?;

        // Create our calling project
        if Path::new(c_pkg_name.as_ref()).exists() {
            trace!("Reusing experiment {c_pkg_name}");
        } else {
            let mut cmd = Command::new("cargo");
            cmd.args(["new", "--quiet", "--bin", "--name"])
                .arg(format!("experiment-{}", c_pkg_name))
                .arg(c_pkg_name.as_ref());
            trace!("Creating experiment {c_pkg_name} with {cmd:?}");
            let status = cmd.status()?;
            assert!(status.success());
        }

        let this_experiment_dir = experiments_dir.join(c_pkg_name.as_ref());
        env::set_current_dir(&this_experiment_dir)?;

        let cargo_toml_path = this_experiment_dir.join("Cargo.toml");
        let cargo_toml = fs::read_to_string(&cargo_toml_path)?;
        let mut cargo_toml: TomlManifest = toml::from_str(&cargo_toml)?;

        let write_cargo_toml = |cargo_toml: &TomlManifest| {
            let cargo_toml = toml::to_string_pretty(&cargo_toml)?;
            fs::write(&cargo_toml_path, cargo_toml)?;
            Ok::<_, Error>(())
        };

        // Reset any dependencies or patches, in case we are rerunning
        cargo_toml.dependencies = None;
        cargo_toml.patch = None;
        write_cargo_toml(&cargo_toml)?;

        let repository_info = match (opts.use_git, &c.repository) {
            (true, Some(repo)) => Some((repo, checkouts_dir.join(c_pkg_name.as_ref()))),
            (true, None) => {
                info!("Experiment {c_pkg_name} has no repository to clone");
                statistics.setup_failures.push(c_pkg_name.to_string());
                tx.blocking_send(Message::ExperimentSetupFailure {
                    name: c.name.clone(),
                })?;
                continue;
            }
            (false, _) => None,
        };

        let dependencies = cargo_toml.dependencies.get_or_insert(Default::default());
        let dep = if let Some((repository_url, repository_path)) = &repository_info {
            trace!("Adding git dependency for experiment {c_pkg_name}");

            if repository_path.exists() {
                trace!(
                    "Reusing experiment {c_pkg_name} repository at {}",
                    repository_path.display(),
                );
            } else {
                let mut cmd = Command::new("git");

                cmd.args(["clone", "--quiet"])
                    .arg(repository_url)
                    .arg(repository_path);
                trace!("Cloning experiment repository {c_pkg_name} with {cmd:?}");
                let status = cmd.status()?;

                if !status.success() {
                    info!("Experiment {c_pkg_name} could not clone the repository");
                    statistics.setup_failures.push(c_pkg_name.to_string());
                    tx.blocking_send(Message::ExperimentSetupFailure {
                        name: c.name.clone(),
                    })?;
                    continue;
                }
            }

            let inner_repository_path = match find_cargo_toml(repository_path, c_pkg_name.as_ref())
            {
                Ok(p) => p,
                Err(_) => {
                    statistics.setup_failures.push(c_pkg_name.to_string());
                    tx.blocking_send(Message::ExperimentSetupFailure {
                        name: c.name.clone(),
                    })?;
                    continue;
                }
            };

            let path = inner_repository_path
                .to_str()
                .ok_or("Path was not UTF-8")?
                .to_owned();
            TomlDetailedDependency {
                path: Some(path),
                ..Default::default()
            }
        } else {
            trace!("Adding crates.io dependency for experiment {c_pkg_name}");

            TomlDetailedDependency {
                version: Some(format!("={}", c.max_version)),
                ..Default::default()
            }
        };

        dependencies.insert(
            c_pkg_name.clone(),
            InheritableDependency::Value(TomlDependency::Detailed(dep)),
        );

        write_cargo_toml(&cargo_toml)?;

        statistics.setup_successes.push(c_pkg_name.to_string());
        tx.blocking_send(Message::ExperimentSetupSuccess {
            name: c.name.clone(),
        })?;

        let repository_path = repository_info.as_ref().map(|(_, p)| p);

        let mut pre_command = prepare_command(
            opts.pre_command.as_deref(),
            &opts.working_dir,
            c_pkg_name.as_ref(),
            repository_path.as_ref(),
        );
        trace!("Running pre command for experiment {c_pkg_name}: {pre_command:?}");
        let output = pre_command.output()?;

        tx.blocking_send(Message::ExperimentOriginalOutput {
            name: c.name.clone(),
            output: state::RunExperimentOutput::from_bytes(output.stdout, output.stderr),
        })?;

        if output.status.success() {
            statistics.original_successes.push(c_pkg_name.to_string());
            tx.blocking_send(Message::ExperimentOriginalSuccess {
                name: c.name.clone(),
            })?;
        } else {
            info!("Experiment {c_pkg_name} failed original build");
            statistics.original_failures.push(c_pkg_name.to_string());
            tx.blocking_send(Message::ExperimentOriginalFailure {
                name: c.name.clone(),
            })?;
            continue;
        }

        let dependencies = cargo_toml.dependencies.get_or_insert(Default::default());
        let patch = cargo_toml.patch.get_or_insert(Default::default());
        let patch_crates_io = patch.entry("crates-io".to_owned()).or_default();

        for mut p in opts.patch.clone() {
            trace!("Adding patch {} for experiment {c_pkg_name}", p.name);

            // You can't set `features` / `default-features` in the
            // `[patch]` section. Instead, we add our own dependency
            // on the crate and set them there.
            if let TomlDependency::Detailed(p_dep) = &mut p.dep {
                if p_dep.default_features.is_some() {
                    unimplemented!("Patch is trying to set `default-features`");
                }

                if let Some(feat) = p_dep.features.take() {
                    let dep = dependencies.entry(p.name.clone()).or_insert_with(|| {
                        InheritableDependency::Value(TomlDependency::Detailed(Default::default()))
                    });
                    if let InheritableDependency::Value(TomlDependency::Detailed(dep)) = dep {
                        dep.version = Some("*".into());
                        dep.default_features = Some(false);
                        dep.features = Some(feat);
                    } else {
                        unimplemented!("Dependency is not detailed");
                    }
                }
            }

            patch_crates_io.insert(p.name, p.dep);
        }
        write_cargo_toml(&cargo_toml)?;

        let mut post_command = prepare_command(
            opts.post_command.as_deref(),
            &opts.working_dir,
            c_pkg_name.as_ref(),
            repository_path.as_ref(),
        );
        trace!("Running post command for experiment {c_pkg_name}: {post_command:?}");
        let output = post_command.output()?;

        tx.blocking_send(Message::ExperimentOutput {
            name: c.name.clone(),
            output: state::RunExperimentOutput::from_bytes(output.stdout, output.stderr),
        })?;

        if output.status.success() {
            statistics.experiment_successes.push(c_pkg_name.to_string());
            tx.blocking_send(Message::ExperimentSuccess {
                name: c.name.clone(),
            })?;
        } else {
            info!("Experiment {c_pkg_name} failed second build");
            statistics.experiment_failures.push(c_pkg_name.to_string());
            tx.blocking_send(Message::ExperimentFailure {
                name: c.name.clone(),
            })?;
            continue;
        }
    }

    Ok(statistics)
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
        let cargo_toml: TomlManifest = toml::from_str(&file)?;

        if let Some(package) = cargo_toml.package {
            if package.name.as_ref() == name {
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
            cmd.args(["build", "--quiet"]);
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
