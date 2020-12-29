use itertools::Itertools;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};
use structopt::StructOpt;
use toml::Value;

use log::{debug, error, info, warn};

/// An enum that represents the commands that will be passed to the remote cargo
/// instance.
///
/// It uses `external_subcommand` to capture any given subcommand plus arguments.
/// This means that everything that is passed as subcommand it "blindly" passed
/// to the remote cargo instance and is required to be a valid cargo subcommand.
///
/// # Example
///
/// `cargo remote -r 100.100.100.100 build --release --all`
///
/// In this example `build --release --all` are passed to the remote cargo instance.
#[derive(StructOpt, Debug)]
enum RemoteCommands {
    #[structopt(external_subcommand)]
    Commands(Vec<String>),
}

impl RemoteCommands {
    /// Convert into the raw commands.
    fn into_commands(self) -> Vec<String> {
        let Self::Commands(cmds) = self;
        cmds
    }
}

#[derive(StructOpt, Debug)]
#[structopt(name = "cargo-remote", bin_name = "cargo")]
enum Opts {
    #[structopt(name = "remote")]
    Remote {
        /// Remote ssh build server.
        #[structopt(short = "r", long)]
        remote: Option<String>,

        /// Set remote environment variables. RUST_BACKTRACE, CC, LIB, etc.
        #[structopt(short = "b", long, default_value = "RUST_BACKTRACE=1")]
        build_env: Vec<String>,

        /// Rustup default (stable|beta|nightly)
        #[structopt(short = "d", long, default_value = "stable")]
        rustup_default: String,

        /// Environment profile.
        #[structopt(short = "e", long, default_value = "/etc/profile")]
        env: String,

        /// Transfer the target folder or specific file from that folder back
        /// to the local machine.
        #[structopt(short = "c", long)]
        copy_back: Option<Option<String>>,

        /// Don't transfer the Cargo.lock file back to the local machine
        #[structopt(long)]
        no_copy_lock: bool,

        /// Path to the manifest to execute
        #[structopt(long, default_value = "Cargo.toml", parse(from_os_str))]
        manifest_path: PathBuf,

        /// Transfer hidden files and directories to the build server
        #[structopt(short = "h", long = "transfer-hidden")]
        hidden: bool,

        #[structopt(flatten)]
        remote_commands: RemoteCommands,
    },
}

/// Tries to parse the file [`config_path`]. Logs warnings and returns [`None`] if errors occur
/// during reading or parsing, [`Some(Value)`] otherwise.
fn config_from_file(config_path: &Path) -> Option<Value> {
    let config_file = std::fs::read_to_string(config_path)
        .map_err(|e| {
            if let std::io::ErrorKind::NotFound = e.kind() {
                debug!(
                    "Can't parse config file '{}' (error: {})",
                    config_path.to_string_lossy(),
                    e
                );
            } else {
                warn!(
                    "Can't parse config file '{}' (error: {})",
                    config_path.to_string_lossy(),
                    e
                );
            }
        })
        .ok()?;

    let value = config_file
        .parse::<Value>()
        .map_err(|e| {
            warn!(
                "Can't parse config file '{}' (error: {})",
                config_path.to_string_lossy(),
                e
            );
        })
        .ok()?;

    Some(value)
}

fn main() {
    let mut log_builder = env_logger::Builder::from_default_env();
    log_builder.filter(None, log::LevelFilter::Info).init();

    let Opts::Remote {
        remote,
        build_env,
        rustup_default,
        env,
        copy_back,
        no_copy_lock,
        manifest_path,
        hidden,
        remote_commands,
    } = Opts::from_args();

    let mut metadata_cmd = cargo_metadata::MetadataCommand::new();
    metadata_cmd.manifest_path(manifest_path).no_deps();

    let project_metadata = metadata_cmd.exec().unwrap();
    let project_dir = project_metadata.workspace_root;
    info!("Project dir: {:?}", project_dir);

    let configs = vec![
        config_from_file(&project_dir.join(".cargo-remote.toml")),
        xdg::BaseDirectories::with_prefix("cargo-remote")
            .ok()
            .and_then(|base| base.find_config_file("cargo-remote.toml"))
            .and_then(|p: PathBuf| config_from_file(&p)),
    ];

    // TODO: move Opts::Remote fields into own type and implement complete_from_config(&mut self, config: &Value)
    let build_server = remote
        .or_else(|| {
            configs
                .into_iter()
                .flat_map(|config| config.and_then(|c| c["remote"].as_str().map(String::from)))
                .next()
        })
        .unwrap_or_else(|| {
            error!("No remote build server was defined (use config file or --remote flag)");
            exit(-3);
        });

    // generate a unique build path by using the hashed project dir as folder on the remote machine
    let mut hasher = DefaultHasher::new();
    project_dir.hash(&mut hasher);
    let build_path = format!("~/remote-builds/{}/", hasher.finish());

    info!("Transferring sources to build server.");
    // transfer project to build server
    let mut rsync_to = Command::new("rsync");
    rsync_to
        .arg("-a".to_owned())
        .arg("--delete")
        .arg("--compress")
        .arg("--info=progress2")
        .arg("--exclude")
        .arg("target");

    if !hidden {
        rsync_to.arg("--exclude").arg(".*");
    }

    rsync_to
        .arg("--rsync-path")
        .arg("mkdir -p remote-builds && rsync")
        .arg(format!("{}/", project_dir.to_string_lossy()))
        .arg(format!("{}:{}", build_server, build_path))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            error!("Failed to transfer project to build server (error: {})", e);
            exit(-4);
        });
    info!("Build ENV: {:?}", build_env);
    info!("Environment profile: {:?}", env);
    info!("Build path: {:?}", build_path);
    let build_command = format!(
        "source {}; rustup default {}; cd {}; {} cargo {}",
        env,
        rustup_default,
        build_path,
        build_env.into_iter().join(" "),
        remote_commands.into_commands().join(" "),
    );

    info!("Starting build process.");
    let output = Command::new("ssh")
        .arg("-t")
        .arg(&build_server)
        .arg(build_command)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            error!("Failed to run cargo command remotely (error: {})", e);
            exit(-5);
        });

    if let Some(file_name) = copy_back {
        info!("Transferring artifacts back to client.");
        let file_name = file_name.unwrap_or_default();
        Command::new("rsync")
            .arg("-a")
            .arg("--delete")
            .arg("--compress")
            .arg("--info=progress2")
            .arg(format!(
                "{}:{}/target/{}",
                build_server, build_path, file_name
            ))
            .arg(format!(
                "{}/target/{}",
                project_dir.to_string_lossy(),
                file_name,
            ))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdin(Stdio::inherit())
            .output()
            .unwrap_or_else(|e| {
                error!(
                    "Failed to transfer target back to local machine (error: {})",
                    e
                );
                exit(-6);
            });
    }

    if !no_copy_lock {
        info!("Transferring Cargo.lock file back to client.");
        Command::new("rsync")
            .arg("-a")
            .arg("--delete")
            .arg("--compress")
            .arg("--info=progress2")
            .arg(format!("{}:{}/Cargo.lock", build_server, build_path))
            .arg(format!("{}/Cargo.lock", project_dir.to_string_lossy()))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdin(Stdio::inherit())
            .output()
            .unwrap_or_else(|e| {
                error!(
                    "Failed to transfer Cargo.lock back to local machine (error: {})",
                    e
                );
                exit(-7);
            });
    }

    if !output.status.success() {
        exit(output.status.code().unwrap_or(1))
    }
}
