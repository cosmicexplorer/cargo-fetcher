use bytes::{Buf, IntoBuf};
use cargo_fetcher::{fetch, util, Krate, Source};
use failure::{format_err, Error};
use log::{error, info};
use rayon::prelude::*;
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(structopt::StructOpt)]
pub struct Args {
    /// The root path for cargo. This defaults to either
    /// CARGO_HOME or HOME/.cargo.
    #[structopt(short, long, parse(from_os_str))]
    cache: Option<PathBuf>,
}

const INDEX_DIR: &str = "registry/index/github.com-1ecc6299db9ec823";
const CACHE_DIR: &str = "registry/cache/github.com-1ecc6299db9ec823";
const SRC_DIR: &str = "registry/src/github.com-1ecc6299db9ec823";
const GIT_DB_DIR: &str = "git/db";

fn sync_registry_index(ctx: &crate::Context<'_>, root_dir: &Path) -> Result<(), Error> {
    let index_path = root_dir.join(INDEX_DIR);

    // Just skip the index if the git directory already exists,
    // as a patch on top of an existing repo via git fetch is
    // presumably faster
    if index_path.join(".git").exists() {
        info!("skippipng crates.io-index download, index repository already present");
        return Ok(());
    }

    let url = url::Url::parse("git+https://github.com/rust-lang/crates.io-index.git")?;
    let canonicalized = util::canonicalize_url(&url)?;
    let ident = util::ident(&canonicalized);

    let krate = Krate {
        name: "crates.io-index".to_owned(),
        version: "1.0.0".to_owned(),
        source: cargo_fetcher::Source::Git {
            url: canonicalized,
            ident,
        },
    };

    let index_data = fetch::from_gcs(&ctx.client, &krate, &ctx.gcs_bucket, ctx.prefix)?;

    let buf_reader = index_data.into_buf().reader();
    let zstd_decoder = zstd::Decoder::new(buf_reader)?;

    if let Err((_, e)) = cargo_fetcher::unpack_tar(zstd_decoder, index_path) {
        error!("failed to unpack crates.io-index: {}", e);
    }

    Ok(())
}

fn sync_locked_crates(ctx: &crate::Context<'_>, root_dir: &Path) -> Result<(), Error> {
    info!("synchronizing {} crates...", ctx.krates.len());

    let cache_dir = root_dir.join(CACHE_DIR);
    let src_dir = root_dir.join(SRC_DIR);
    let git_db_dir = root_dir.join(GIT_DB_DIR);
    std::fs::create_dir_all(&cache_dir)?;
    std::fs::create_dir_all(&src_dir)?;
    std::fs::create_dir_all(&git_db_dir)?;

    let cache_iter = std::fs::read_dir(&cache_dir)?;
    let db_iter = std::fs::read_dir(&git_db_dir)?;

    // TODO: Also check the untarred crates
    info!("checking local cache for missing crates...");

    let mut cached_crates: Vec<String> = cache_iter
        .chain(db_iter)
        .filter_map(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_name().to_str().map(|s| s.to_owned()))
        })
        .collect();

    cached_crates.sort();

    let mut to_sync = Vec::with_capacity(ctx.krates.len());
    let mut krate_name = String::with_capacity(128);

    for krate in ctx.krates {
        use std::fmt::Write;
        write!(&mut krate_name, "{}", krate.local_id()).unwrap();

        if cached_crates.binary_search(&krate_name).is_err() {
            to_sync.push(krate);
        }

        krate_name.clear();
    }

    if to_sync.is_empty() {
        info!("all crates already available on local disk");
        return Ok(());
    }

    info!("synchronizing {} missing crates...", to_sync.len());

    ctx.krates.par_iter().for_each(|krate| {
        match fetch::from_gcs(&ctx.client, krate, &ctx.gcs_bucket, ctx.prefix) {
            Err(e) => error!("failed to download {}: {}", krate, e),
            Ok(krate_data) => {
                match &krate.source {
                    Source::CratesIo(_) => {
                        let packed_krate_path = cache_dir.join(format!("{}", krate.local_id()));

                        match std::fs::File::create(&packed_krate_path) {
                            Ok(mut f) => {
                                let _ = f.set_len(krate_data.len() as u64);

                                if let Err(e) = f.write_all(&krate_data) {
                                    error!("failed to write {} to disk: {}", krate, e);
                                }
                            }
                            Err(e) => {
                                error!("failed to create {}: {}", krate, e);
                            }
                        }

                        // Decompress and splat the tar onto the filesystem
                        let buf_reader = krate_data.into_buf().reader();
                        let gz_decoder = flate2::read::GzDecoder::new(buf_reader);

                        let mut src_path = src_dir.join(format!("{}", krate.local_id()));
                        // Remove the .crate extension
                        src_path.set_extension("");

                        if !src_path.exists() {
                            log::debug!("unpacking {} to src/", krate);
                            if let Err((_, e)) = cargo_fetcher::unpack_tar(gz_decoder, src_path) {
                                error!("failed to unpack dependency {}: {}", krate, e);
                            }
                        }
                    }
                    Source::Git { .. } => {
                        let buf_reader = krate_data.into_buf().reader();
                        let zstd_decoder = match zstd::Decoder::new(buf_reader) {
                            Ok(zd) => zd,
                            Err(e) => {
                                error!("failed to create decompressor for {}: {}", krate, e);
                                return;
                            }
                        };

                        let db_path = git_db_dir.join(format!("{}", krate.local_id()));
                        if let Err((_, e)) = cargo_fetcher::unpack_tar(zstd_decoder, db_path) {
                            error!("failed to unpack dependency {}: {}", krate, e);
                        }
                    }
                }
            }
        }
    });

    Ok(())
}

pub fn cmd(ctx: crate::Context<'_>, args: Args) -> Result<(), Error> {
    let root_dir = args
        .cache
        .or_else(|| std::env::var_os("CARGO_HOME").map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|hd| hd.join(".cargo")));

    let root_dir = root_dir.ok_or_else(|| format_err!("unable to determine cargo root"))?;

    // There should always be a bin/cargo(.exe) relative to the root directory, at a minimum
    let cargo_path = {
        let mut cpath = root_dir.join("bin/cargo");

        if cfg!(target_os = "windows") {
            cpath.set_extension("exe");
        }

        cpath
    };

    if !cargo_path.exists() {
        return Err(format_err!(
            "cargo root {} does not seem to contain the cargo binary",
            root_dir.display()
        ));
    }

    // Create the registry directory as it is the root of multiple other ones
    std::fs::create_dir_all(root_dir.join("registry"))?;

    rayon::join(
        || {
            if !ctx.include_index {
                return;
            }

            info!("syncing crates.io index");
            match sync_registry_index(&ctx, &root_dir) {
                Ok(_) => info!("successfully synced crates.io index"),
                Err(e) => error!("failed to sync crates.io index: {}", e),
            }
        },
        || match sync_locked_crates(&ctx, &root_dir) {
            Ok(_) => {
                info!("finished syncing crates");
            }
            Err(e) => error!("failed to sync crates: {}", e),
        },
    );

    Ok(())
}
