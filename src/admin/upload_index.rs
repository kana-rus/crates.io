use crate::admin::dialoguer;
use crate::storage::Storage;
use anyhow::Context;
use crates_io_index::{Repository, RepositoryConfig};
use indicatif::{ProgressBar, ProgressIterator, ProgressStyle};

#[derive(clap::Parser, Debug)]
#[command(
    name = "upload-index",
    about = "Upload index from git to S3 (http-based index)"
)]
pub struct Opts {
    /// Incremental commit. Any changed files made after this commit will be uploaded.
    incremental_commit: Option<String>,
}

pub fn run(opts: Opts) -> anyhow::Result<()> {
    let storage = Storage::from_environment();

    println!("fetching git repo");
    let config = RepositoryConfig::from_environment();
    let repo = Repository::open(&config)?;
    repo.reset_head()?;
    println!("HEAD is at {}", repo.head_oid()?);

    let files = repo.get_files_modified_since(opts.incremental_commit.as_deref())?;
    println!("found {} files to upload", files.len());
    if !dialoguer::confirm("continue with upload?") {
        return Ok(());
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to initialize tokio runtime")
        .unwrap();

    let pb = ProgressBar::new(files.len() as u64);
    pb.set_style(ProgressStyle::with_template("{bar:60} ({pos}/{len}, ETA {eta})").unwrap());

    for file in files.iter().progress_with(pb.clone()) {
        let crate_name = file.file_name().unwrap().to_str().unwrap();
        let path = repo.index_file(crate_name);
        if !path.exists() {
            pb.suspend(|| println!("skipping file `{crate_name}`"));
            continue;
        }

        let contents = std::fs::read_to_string(&path)?;
        rt.block_on(storage.sync_index(crate_name, Some(contents)))?;
    }

    println!(
        "uploading completed; use `upload-index {}` for an incremental run",
        repo.head_oid()?
    );
    Ok(())
}
