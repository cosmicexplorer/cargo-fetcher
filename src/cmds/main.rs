extern crate cargo_fetcher as cf;

use anyhow::{anyhow, Context, Error};
use std::{path::PathBuf, sync::Arc};
use structopt::StructOpt;
use tracing_subscriber::filter::LevelFilter;
use url::Url;

mod mirror;
mod sync;

#[derive(StructOpt)]
enum Command {
    /// Uploads any crates in the lockfile that aren't already present
    /// in the cloud storage location
    #[structopt(name = "mirror")]
    Mirror(mirror::Args),
    /// Downloads missing crates to the local cargo locations and unpacks
    /// them
    #[structopt(name = "sync")]
    Sync(sync::Args),
}

fn parse_level(s: &str) -> Result<LevelFilter, Error> {
    s.parse::<LevelFilter>()
        .map_err(|_| anyhow!("failed to parse level '{}'", s))
}

#[derive(StructOpt)]
struct Opts {
    /// Path to a service account credentials file used to obtain
    /// oauth2 tokens. By default uses GOOGLE_APPLICATION_CREDENTIALS
    /// environment variable.
    #[structopt(
        short,
        long,
        env = "GOOGLE_APPLICATION_CREDENTIALS",
        parse(from_os_str)
    )]
    credentials: Option<PathBuf>,
    /// A url to a cloud storage bucket and prefix path at which to store
    /// or retrieve archives
    #[structopt(short = "u", long = "url")]
    url: Url,
    /// Path to the lockfile used for determining what crates to operate on
    #[structopt(
        short,
        long = "lock-file",
        default_value = "Cargo.lock",
        parse(from_os_str)
    )]
    lock_file: PathBuf,
    #[structopt(
        short = "L",
        long = "log-level",
        default_value = "info",
        parse(try_from_str = parse_level),
        long_help = "The log level for messages, only log messages at or above the level will be emitted.

Possible values:
* off
* error
* warn
* info (default)
* debug
* trace"
    )]
    log_level: LevelFilter,
    /// Output log messages as json
    #[structopt(long)]
    json: bool,
    /// A snapshot of the registry index is also included when mirroring or syncing
    #[structopt(short, long = "include-index")]
    include_index: bool,
    #[structopt(subcommand)]
    cmd: Command,
}

async fn init_backend(
    loc: cf::CloudLocation<'_>,
    _credentials: Option<PathBuf>,
) -> Result<Arc<dyn cf::Backend + Sync + Send>, Error> {
    match loc {
        #[cfg(feature = "gcs")]
        cf::CloudLocation::Gcs(gcs) => {
            let cred_path = _credentials.context("GCS credentials not specified")?;

            let gcs = cf::backends::gcs::GcsBackend::new(gcs, &cred_path).await?;
            Ok(Arc::new(gcs))
        }
        #[cfg(not(feature = "gcs"))]
        cf::CloudLocation::Gcs(_) => anyhow::bail!("GCS backend not enabled"),
        #[cfg(feature = "s3")]
        cf::CloudLocation::S3(loc) => {
            // Special case local testing
            let make_bucket = loc.bucket == "testing" && loc.host.contains("localhost");

            let s3 = cf::backends::s3::S3Backend::new(loc)?;

            if make_bucket {
                s3.make_bucket()
                    .await
                    .context("failed to create test bucket")?;
            }

            Ok(Arc::new(s3))
        }
        #[cfg(not(feature = "s3"))]
        cf::CloudLocation::S3(_) => anyhow::bail!("S3 backend not enabled"),
        #[cfg(feature = "fs")]
        cf::CloudLocation::Fs(loc) => Ok(Arc::new(cf::backends::fs::FSBackend::new(loc).await?)),
        #[cfg(not(feature = "fs"))]
        cf::CloudLocation::Fs(_) => anyhow::bail!("filesystem backend not enabled"),
    }
}

async fn real_main() -> Result<(), Error> {
    let args = Opts::from_iter({
        std::env::args().enumerate().filter_map(|(i, a)| {
            if i == 1 && a == "fetcher" {
                None
            } else {
                Some(a)
            }
        })
    });

    let mut env_filter = tracing_subscriber::EnvFilter::from_default_env();

    // If a user specifies a log level, we assume it only pertains to cargo_fetcher,
    // if they want to trace other crates they can use the RUST_LOG env approach
    env_filter = env_filter.add_directive(args.log_level.into());

    let subscriber = tracing_subscriber::FmtSubscriber::builder().with_env_filter(env_filter);

    if args.json {
        tracing::subscriber::set_global_default(subscriber.json().finish())
            .context("failed to set default subscriber")?;
    } else {
        tracing::subscriber::set_global_default(subscriber.finish())
            .context("failed to set default subscriber")?;
    };

    let cloud_location = cf::util::CloudLocationUrl::from_url(args.url.clone())?;
    let location = cf::util::parse_cloud_location(&cloud_location)?;
    let backend = init_backend(location, args.credentials).await?;

    let krates =
        cf::read_lock_file(args.lock_file).context("failed to get crates from lock file")?;

    match args.cmd {
        Command::Mirror(margs) => {
            let ctx = cf::Ctx::new(None, backend, krates).context("failed to create context")?;
            mirror::cmd(ctx, args.include_index, margs).await
        }
        Command::Sync(sargs) => {
            let root_dir = cf::util::determine_cargo_root(sargs.cargo_root.as_ref())?;
            let ctx = cf::Ctx::new(Some(root_dir), backend, krates)
                .context("failed to create context")?;
            sync::cmd(ctx, args.include_index, sargs).await
        }
    }
}

#[tokio::main]
async fn main() {
    match real_main().await {
        Ok(_) => {}
        Err(e) => {
            tracing::error!("{:#}", e);
            std::process::exit(1);
        }
    }
}
