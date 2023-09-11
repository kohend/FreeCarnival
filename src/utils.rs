use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::ExitStatus,
    sync::Arc,
};

use async_recursion::async_recursion;
use bytes::Bytes;
use directories::ProjectDirs;
use human_bytes::human_bytes;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use os_path::OsPath;
use queues::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    fs::File,
    io::AsyncWriteExt,
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
};

use crate::{
    api::{
        self,
        auth::{Product, ProductVersion, BuildOs},
        product::{BuildManifestChunksRecord, BuildManifestRecord},
    },
    config::{GalaConfig, InstalledConfig, LibraryConfig},
    constants::*,
    shared::models::InstallInfo,
};

// TODO: Refactor info printing and chunk downloading to separate functions
pub(crate) async fn install<'a>(
    client: reqwest::Client,
    slug: &String,
    install_path: &PathBuf,
    version: Option<&ProductVersion>,
    max_download_workers: usize,
    max_memory_usage: usize,
    info_only: bool,
    skip_verify: bool,
) -> Result<Result<(String, Option<InstallInfo>), &'a str>, reqwest::Error> {
    let library = LibraryConfig::load().expect("Failed to load library");
    let product = match library
        .collection
        .iter()
        .find(|p| p.slugged_name == slug.to_owned())
    {
        Some(product) => product,
        None => {
            return Ok(Err("Could not find game in library"));
        }
    };

    let build_version = match version {
        Some(selected) => selected,
        None => match product.get_latest_version() {
            Some(latest) => latest,
            None => {
                return Ok(Err("Failed to fetch latest build number. Cannot install."));
            }
        },
    };
    println!("Found game. Installing build version {}...", build_version);

    println!("Fetching build manifest...");
    let build_manifest =
        api::product::get_build_manifest(&client, &product, &build_version).await?;
    store_build_manifest(
        &build_manifest,
        &build_version.version,
        &product.slugged_name,
        "manifest",
    )
    .await
    .expect("Failed to save build manifest");

    if info_only {
        let mut build_manifest_rdr = csv::Reader::from_reader(&build_manifest[..]);
        let download_size = build_manifest_rdr
            .byte_records()
            .map(|r| {
                let mut record = r.expect("Failed to get byte record");
                record.push_field(b"");
                record.deserialize::<BuildManifestRecord>(None)
            })
            .fold(0f64, |acc, record| match record {
                Ok(record) => acc + record.size_in_bytes as f64,
                Err(_) => acc,
            });

        let mut buf = String::new();
        buf.push_str(&format!("Download Size: {}", human_bytes(download_size)));
        buf.push_str(&format!("\nDisk Size: {}", human_bytes(download_size)));
        return Ok(Ok((buf, None)));
    }

    println!("Fetching build manifest chunks...");
    let build_manifest_chunks =
        api::product::get_build_manifest_chunks(&client, &product, &build_version).await?;
    store_build_manifest(
        &build_manifest_chunks,
        &build_version.version,
        &product.slugged_name,
        "manifest_chunks",
    )
    .await
    .expect("Failed to save build manifest chunks");

    let product_arc = Arc::new(product.clone());
    let os_arc = Arc::new(build_version.os.to_owned());

    println!("Installing game from manifest...");
    let result = build_from_manifest(
        client,
        product_arc,
        os_arc,
        &build_manifest[..],
        &build_manifest_chunks[..],
        install_path.into(),
        max_download_workers,
        max_memory_usage,
        skip_verify,
    )
    .await
    .expect("Failed to build from manifest");

    match result {
        true => {
            let install_info = InstallInfo::new(install_path.to_owned(), build_version.version.to_owned(), build_version.os.to_owned());
            Ok(Ok((
                format!("Successfully installed {} ({})", slug, build_version),
                Some(install_info),
            )))
        },
        false => Ok(Err(
            "Some chunks failed verification. Failed to install game.",
        )),
    }
}

pub(crate) async fn uninstall(install_path: &PathBuf) -> tokio::io::Result<()> {
    tokio::fs::remove_dir_all(install_path).await
}

pub(crate) async fn check_updates(
    library: LibraryConfig,
    installed: InstalledConfig,
) -> tokio::io::Result<HashMap<String, String>> {
    let mut available_updates = HashMap::new();
    for (slug, info) in installed {
        println!("Checking if {slug} has updates...");
        let product = match library.collection.iter().find(|p| p.slugged_name == slug) {
            Some(p) => p,
            None => {
                println!("Couldn't find {slug} in library. Try running `sync` first.");
                continue;
            }
        };
        let latest_version = match product.get_latest_version() {
            Some(v) => v,
            None => {
                println!("Couldn't find the latest version of {slug}");
                continue;
            }
        };

        if &info.version != &latest_version.version {
            available_updates.insert(slug, latest_version.version.to_owned());
        }
    }
    Ok(available_updates)
}

pub(crate) async fn update(
    client: reqwest::Client,
    library: &LibraryConfig,
    slug: &String,
    install_info: &InstallInfo,
    selected_version: Option<&ProductVersion>,
    max_download_workers: usize,
    max_memory_usage: usize,
    info_only: bool,
    skip_verify: bool,
) -> tokio::io::Result<(String, Option<InstallInfo>)> {
    let product = match library.collection.iter().find(|p| &p.slugged_name == slug) {
        Some(p) => p,
        None => {
            return Ok((format!("Couldn't find {slug} in library"), None));
        }
    };
    let version = match selected_version {
        Some(v) => v,
        None => {
            println!("Fetching latest version...");
            match product.get_latest_version() {
                Some(v) => v,
                None => {
                    return Ok((format!("Couldn't find the latest version of {slug}"), None));
                }
            }
        }
    };

    if install_info.version == version.version {
        return Ok((format!("Build {version} is already installed"), None));
    }

    let old_manifest = read_build_manifest(&install_info.version, slug, "manifest").await?;

    println!("Fetching {} build manifest...", version);
    let new_manifest = match api::product::get_build_manifest(&client, &product, &version).await {
        Ok(m) => m,
        Err(err) => {
            return Ok((format!("Failed to fetch build manifest: {:?}", err), None));
        }
    };
    store_build_manifest(&new_manifest, &version.version, slug, "manifest").await?;
    let new_manifest_chunks =
        match api::product::get_build_manifest_chunks(&client, &product, &version).await {
            Ok(m) => m,
            Err(err) => {
                return Ok((
                    format!("Failed to fetch build manifest chunks: {:?}", err),
                    None,
                ));
            }
        };
    store_build_manifest(
        &new_manifest_chunks,
        &version.version,
        slug,
        "manifest_chunks",
    )
    .await?;

    let delta_manifest = read_or_generate_delta_manifest(
        slug,
        &old_manifest[..],
        &new_manifest[..],
        &install_info.version,
        &version.version,
    )
    .await?;
    let delta_manifest_chunks = read_or_generate_delta_chunks_manifest(
        slug,
        &delta_manifest[..],
        &new_manifest_chunks[..],
        &install_info.version,
        &version.version,
    )
    .await?;

    if info_only {
        let mut delta_build_manifest_rdr = csv::Reader::from_reader(&delta_manifest[..]);
        let download_size = delta_build_manifest_rdr
            .byte_records()
            .map(|r| {
                r.expect("Failed to get byte record").deserialize::<BuildManifestRecord>(None)
            })
            .fold(0f64, |acc, record| match record {
                Ok(record) => match record.tag {
                    Some(ChangeTag::Removed) => acc,
                    _ => acc + record.size_in_bytes as f64,
                },
                Err(_) => acc,
            });
        let mut new_build_manifest_rdr = csv::Reader::from_reader(&new_manifest[..]);
        let disk_size = new_build_manifest_rdr
            .byte_records()
            .map(|r| {
                let mut record = r.expect("Failed to get byte record");
                record.push_field(b"");
                record.deserialize::<BuildManifestRecord>(None)
            })
            .fold(0f64, |acc, record| match record {
                Ok(record) => acc + record.size_in_bytes as f64,
                Err(_) => acc,
            });

        let mut old_manifest_rdr = csv::Reader::from_reader(&old_manifest[..]);
        let old_disk_size = old_manifest_rdr
            .byte_records()
            .map(|r| {
                let mut record = r.expect("Failed to get byte record");
                record.push_field(b"");
                record.deserialize::<BuildManifestRecord>(None)
            })
            .fold(0f64, |acc, record| match record {
                Ok(record) => acc + record.size_in_bytes as f64,
                Err(_) => acc,
            });

        let needed_space = disk_size - old_disk_size;
        println!("{}", needed_space);

        let mut buf = String::new();
        buf.push_str(&format!("Download Size: {}", human_bytes(download_size)));
        buf.push_str(&format!(
            "\nNeeded Space: {}{}",
            if needed_space < 0f64 { "-" } else { "" },
            human_bytes(needed_space.abs())
        ));
        buf.push_str(&format!("\nTotal Disk Size: {}", human_bytes(disk_size)));
        return Ok((buf, None));
    }

    let product_arc = Arc::new(product.clone());
    let version_arc = Arc::new(version.os.to_owned());
    build_from_manifest(
        client,
        product_arc,
        version_arc,
        &delta_manifest[..],
        &delta_manifest_chunks[..],
        OsPath::from(&install_info.install_path),
        max_download_workers,
        max_memory_usage,
        skip_verify,
    )
    .await?;

    let install_info = InstallInfo::new(install_info.install_path.to_owned(), version.version.to_owned(), version.os.to_owned());
    Ok((
        format!("Updated {slug} successfully."),
        Some(install_info),
    ))
}

pub(crate) async fn launch(
    client: &reqwest::Client,
    product: &Product,
    install_info: &InstallInfo,
    #[cfg(not(target_os = "windows"))] wine_bin: Option<PathBuf>,
    #[cfg(not(target_os = "windows"))] wine_prefix: Option<PathBuf>,
) -> tokio::io::Result<Option<ExitStatus>> {
    let os = &install_info.os;

    #[cfg(not(target_os = "windows"))]
    let wine_bin = match os {
        BuildOs::Windows => {
            match wine_bin {
                Some(wine_bin) => Some(wine_bin),
                None => {
                    println!("You need to set --wine-bin to run Windows games");
                    return Ok(None);
                }
            }  
        }
        _ => None,
    };
    if os == &BuildOs::Windows && wine_bin.is_none() {
    }

    let game_details = match api::product::get_game_details(&client, &product).await {
        Ok(details) => details,
        Err(err) => {
            println!("Failed to fetch game details. Launch might fail: {:?}", err);

            None
        }
    };

    let exe_path = match game_details {
        Some(details) => match details.exe_path {
            Some(exe_path) => {
                // Not too sure about this. At least syberia-ii prepends the slugged name to the
                // path of the exe. I assume the galaClient always installs in folders with the
                // slugged name, but since we don't do that here, we skip it.
                // This might break if some games don't do this, and if that happens, we should
                // find a better solution for handling this.
                let re = Regex::new(&format!("^{}\\\\", product.slugged_name)).unwrap();
                let dirless_path = re.replace(&exe_path, "");

                Some(dirless_path.into_owned())
            }
            None => None,
        },
        None => None,
    };
    let exe = match exe_path {
        Some(path) => OsPath::from(&install_info.install_path).join(path),
        None => match os {
            BuildOs::Windows => match find_exe_recursive(&install_info.install_path).await {
                Some(exe) => exe,
                None => {
                    println!("Couldn't find suitable exe...");
                    return Ok(None);
                }
            },
            #[cfg(target_os = "macos")]
            BuildOs::Mac => match find_app_recursive(&install_info.install_path).await {
                Some(app) => app,
                None => {
                    println!("Couldn't find a suitable app...");
                    return Ok(None);
                }
            },
            #[cfg(not(target_os = "macos"))]
            BuildOs::Mac => {
                println!("You can only launch macOS games on macOS");
                return Ok(None);
            },
            BuildOs::Linux => {
                println!("We don't support launching Linux games yet...");
                return Ok(None);
            },
        },
    };
    println!("{} was selected", exe);

    #[cfg(not(target_os = "windows"))]
    let should_use_wine = os == &BuildOs::Windows;
    let (binary, args) = (
        #[cfg(target_os = "windows")]
        exe.to_string(),
        #[cfg(target_os = "linux")]
        if should_use_wine { wine_bin.unwrap().to_str().unwrap().to_owned() } else { exe.to_string() },
        #[cfg(target_os = "macos")]
        if should_use_wine { wine_bin.unwrap().to_str().unwrap().to_owned() } else { "open".to_owned() },
        #[cfg(target_os = "windows")]
        "".to_owned(),
        #[cfg(target_os = "linux")]
        if should_use_wine { exe.to_string() } else { "".to_owned() },
        #[cfg(target_os = "macos")]
        exe.to_string(),
    );

    let mut command = tokio::process::Command::new(binary);
    command.arg(args);
    // TODO:
    // Handle cwd and launch args. Since I don't have games that have these I don't have a
    // reliable way to test...
    #[cfg(not(target_os = "windows"))]
    if let Some(wine_prefix) = wine_prefix {
        command.env("WINEPREFIX", wine_prefix);
    }
    let mut child = command.spawn()?;

    let status = child.wait().await?;

    Ok(Some(status))
}

pub(crate) async fn verify(slug: &String, install_info: &InstallInfo) -> tokio::io::Result<bool> {
    let mut handles: Vec<JoinHandle<bool>> = vec![];

    let build_manifest = read_build_manifest(&install_info.version, slug, "manifest").await?;
    let mut build_manifest_rdr = csv::Reader::from_reader(&build_manifest[..]);
    let build_manifest_byte_records = build_manifest_rdr.byte_records();

    for record in build_manifest_byte_records {
        let mut record = record.expect("Failed to get byte record");
        record.push_field(b"");
        let record = record.deserialize::<BuildManifestRecord>(None).expect("Failed to deserialize build manifest");

        if record.is_directory() {
            continue;
        }

        let file_path = OsPath::from(install_info.install_path.join(&record.file_name));
        if !tokio::fs::try_exists(&file_path).await? {
            println!("{} is missing", record.file_name);
            return Ok(false);
        }

        handles.push(tokio::spawn(async move {
            match verify_file_hash(&file_path, &record.sha) {
                Ok(result) => result,
                Err(err) => {
                    println!("Failed to verify {}: {:?}", record.file_name, err);

                    false
                }
            }
        }));
    }

    let mut result = true;
    for handle in handles {
        if !handle.await? {
            result = false;
            break;
        }
    }

    Ok(result)
}

#[async_recursion]
async fn find_exe_recursive(path: &PathBuf) -> Option<OsPath> {
    let mut subdirs = vec![];

    match tokio::fs::read_dir(path).await {
        Ok(mut subpath) => {
            while let Ok(Some(entry)) = subpath.next_entry().await {
                let entry_path = entry.path();
                if entry_path.is_file() {
                    // Check if the current path is a file with a .exe extension
                    println!("Checking file: {}", entry_path.display());
                    if let (Some(ext), Some(file_name)) =
                        (entry_path.extension(), entry_path.file_name())
                    {
                        let file_name_str = String::from(match file_name.to_str() {
                            Some(str) => str.to_lowercase(),
                            None => String::new(),
                        });
                        if ext == "exe"
                            && !file_name_str.contains("setup")
                            && !file_name_str.contains("unins")
                        {
                            return Some(OsPath::from(entry_path));
                        }
                    }
                } else if entry_path.is_dir() {
                    subdirs.push(entry_path);
                }
            }
        }
        Err(err) => {
            println!("Failed to iterate over {}: {:?}", path.display(), err);
        }
    }

    for dir in subdirs {
        println!("Checking directory: {}", dir.display());
        if let Some(exe_path) = find_exe_recursive(&dir.to_path_buf()).await {
            return Some(OsPath::from(exe_path));
        }
    }

    None
}

#[cfg(target_os = "macos")]
#[async_recursion]
async fn find_app_recursive(path: &PathBuf) -> Option<OsPath> {
    let mut subdirs = vec![];

    match tokio::fs::read_dir(path).await {
        Ok(mut subpath) => {
            while let Ok(Some(entry)) = subpath.next_entry().await {
                let entry_path = entry.path();
                // Check if the current path is a .app extension
                println!("Checking file: {}", entry_path.display());
                if let Some(ext) = entry_path.extension() {
                    if ext == "app" {
                        return Some(OsPath::from(entry_path));
                    }
                }

                if entry_path.is_dir() {
                    subdirs.push(entry_path);
                }
            }
        }
        Err(err) => {
            println!("Failed to iterate over {}: {:?}", path.display(), err);
        }
    }

    for dir in subdirs {
        println!("Checking directory: {}", dir.display());
        if let Some(app_path) = find_app_recursive(&dir.to_path_buf()).await {
            return Some(OsPath::from(app_path));
        }
    }

    None
}

#[derive(PartialEq, Clone, Debug, Serialize, Deserialize)]
pub(crate) enum ChangeTag {
    Added,
    Modified,
    Removed,
}

async fn read_or_generate_delta_manifest(
    slug: &String,
    old_manifest_bytes: &[u8],
    new_manifest_bytes: &[u8],
    old_version: &String,
    new_version: &String,
) -> tokio::io::Result<Vec<u8>> {
    let manifest_delta_version = format!("{}_{}", old_version, new_version);
    if let Ok(exising_delta) =
        read_build_manifest(&manifest_delta_version, slug, "manifest_delta").await
    {
        println!("Using existing delta manifest");
        return Ok(exising_delta);
    }

    println!("Generating delta manifest...");
    let mut new_manifest_rdr = csv::Reader::from_reader(new_manifest_bytes);
    let new_manifest_iter: Vec<BuildManifestRecord> = new_manifest_rdr
        .byte_records()
        .map(|r| {
            let mut record = r.expect("Failed to get byte record");
            record.push_field(b"");
            record.deserialize::<BuildManifestRecord>(None).expect("Failed to deserialize updated build manifest")
        })
        .collect();
    let mut old_manifest_rdr = csv::Reader::from_reader(old_manifest_bytes);
    let old_manifest_iter: Vec<BuildManifestRecord> = old_manifest_rdr
        .byte_records()
        .map(|r| {
            let mut record = r.expect("Failed to get byte record");
            record.push_field(b"");
            record.deserialize::<BuildManifestRecord>(None).expect("Failed to deserialize old build manifest")
        })
        .collect();

    let new_file_names: HashSet<&String> = new_manifest_iter
        .iter()
        .map(|entry| &entry.file_name)
        .collect();
    let mut build_manifest_delta_wtr = csv::Writer::from_writer(vec![]);

    for new_entry in &new_manifest_iter {
        let added = !old_manifest_iter
            .iter()
            .any(|entry| entry.file_name == new_entry.file_name);

        if added {
            build_manifest_delta_wtr
                .serialize(BuildManifestRecord {
                    tag: Some(ChangeTag::Added),
                    ..new_entry.clone()
                })
                .expect("Failed to serialize delta build manifest");
            continue;
        }

        let modified = match old_manifest_iter
            .iter()
            .find(|entry| entry.file_name == new_entry.file_name)
        {
            Some(old_entry) => old_entry.sha != new_entry.sha,
            None => false,
        };

        if modified {
            build_manifest_delta_wtr
                .serialize(BuildManifestRecord {
                    tag: Some(ChangeTag::Modified),
                    ..new_entry.clone()
                })
                .expect("Failed to serialize delta build manifest");
        }
    }

    for old_entry in old_manifest_iter {
        if !new_file_names.contains(&old_entry.file_name) {
            build_manifest_delta_wtr
                .serialize(BuildManifestRecord {
                    tag: Some(ChangeTag::Removed),
                    ..old_entry
                })
                .expect("Failed to serialize delta build manifest");
        }
    }
    let delta_bytes = build_manifest_delta_wtr.into_inner().unwrap();
    store_build_manifest(
        &delta_bytes,
        &format!("{}_{}", old_version, new_version),
        slug,
        "manifest_delta",
    )
    .await?;

    Ok(delta_bytes)
}

async fn read_or_generate_delta_chunks_manifest(
    slug: &String,
    delta_manifest_bytes: &[u8],
    new_manifest_bytes: &[u8],
    old_version: &String,
    new_version: &String,
) -> tokio::io::Result<Vec<u8>> {
    let manifest_delta_version = format!("{}_{}", old_version, new_version);
    if let Ok(exising_delta) =
        read_build_manifest(&manifest_delta_version, slug, "manifest_delta_chunks").await
    {
        println!("Using existing chunks delta manifest");
        return Ok(exising_delta);
    }

    println!("Generating chunks delta manifest...");
    let mut delta_manifest_rdr = csv::Reader::from_reader(delta_manifest_bytes);
    let mut delta_manifest = delta_manifest_rdr.byte_records().map(|r| {
        let record = r.expect("Failed to get byte record");
        record.deserialize::<BuildManifestRecord>(None)
    });
    let mut current_file = delta_manifest
        .next()
        .expect("Failed to deserialize build manifest delta")
        .expect("There were no changes in this update?");

    let mut new_manifest_rdr = csv::Reader::from_reader(new_manifest_bytes);
    let new_manifest_byte_records = new_manifest_rdr.byte_records();
    let mut build_manifest_delta_wtr = csv::Writer::from_writer(vec![]);

    for record in new_manifest_byte_records {
        let record = record.expect("Failed to get byte record");
        let record = record.deserialize::<BuildManifestChunksRecord>(None).expect("Failed to deserialize build manifest chunks");

        // Removed files are always last in the delta manifest, so we can break here
        if current_file.tag == Some(ChangeTag::Removed) {
            break;
        }

        // We want to ignore chunks for removed files and folders
        while current_file.is_directory() || current_file.is_empty() {
            current_file = match delta_manifest.next() {
                Some(file) => {
                    println!("Skipping over {}", current_file.file_name);
                    file.expect("Failed to deserialize build manifest delta")
                }
                None => {
                    println!("Done processing delta chunks");
                    break;
                }
            };
        }

        if record.file_path != current_file.file_name {
            continue;
        }

        build_manifest_delta_wtr
            .serialize(&record)
            .expect("Failed to serialize build manifest chunks");

        if usize::from(record.id) + 1 == current_file.chunks {
            println!("Done processing chunks for {}", record.file_path);
            // Move on to the next file
            current_file = match delta_manifest.next() {
                Some(file) => file.expect("Failed to deserialize build manifest delta"),
                None => {
                    println!("Done processing delta chunks");
                    break;
                }
            };
        }
    }

    let delta_bytes = build_manifest_delta_wtr.into_inner().unwrap();
    store_build_manifest(
        &delta_bytes,
        &format!("{}_{}", old_version, new_version),
        slug,
        "manifest_delta_chunks",
    )
    .await?;

    Ok(delta_bytes)
}

async fn store_build_manifest(
    body: &[u8],
    build_number: &String,
    product_slug: &String,
    file_suffix: &str,
) -> tokio::io::Result<()> {
    // TODO: Move appName to constant
    let project = ProjectDirs::from("rs", "", *PROJECT_NAME).unwrap();
    let path = project.config_dir().join("manifests").join(product_slug);
    tokio::fs::create_dir_all(&path).await?;

    let path = path.join(format!("{}_{}.csv", build_number, file_suffix));
    tokio::fs::write(path, body).await
}

async fn read_build_manifest(
    build_number: &String,
    product_slug: &String,
    file_suffix: &str,
) -> tokio::io::Result<Vec<u8>> {
    // TODO: Move appName to constant
    let project = ProjectDirs::from("rs", "", *PROJECT_NAME).unwrap();
    let path = project
        .config_dir()
        .join("manifests")
        .join(product_slug)
        .join(format!("{}_{}.csv", build_number, file_suffix));
    tokio::fs::read(path).await
}

#[cfg(target_os = "macos")]
struct MacAppExecutables {
    plist: Option<PathBuf>,
}

#[cfg(target_os = "macos")]
impl MacAppExecutables {
    fn new() -> Self {
        Self {
            plist: None
        }
    }

    fn set_plist(&mut self, plist: PathBuf) {
        self.plist = Some(plist);
    }

    async fn mark_as_executable(&self) -> tokio::io::Result<()> {
        use std::{os::unix::prelude::PermissionsExt, fs::Permissions};

        match &self.plist {
            Some(plist_path) => {
                let permissions: Permissions = PermissionsExt::from_mode(0o755); // Read/write/execute
                let plist: apple_bundle::prelude::InfoPlist = apple_bundle::from_file(&plist_path).unwrap();
                let executable_path = plist_path.parent().unwrap().join("MacOS").join(plist.launch.bundle_executable.unwrap());
                tokio::fs::set_permissions(executable_path, permissions).await?;
            },
            None => {
                println!("No executable set, cannot mark as executable.");
            }
        };

        Ok(())
    }
}

async fn build_from_manifest(
    client: reqwest::Client,
    product: Arc<Product>,
    os: Arc<BuildOs>,
    build_manifest_bytes: &[u8],
    build_manifest_chunks_bytes: &[u8],
    install_path: OsPath,
    max_download_workers: usize,
    max_memory_usage: usize,
    skip_verify: bool,
) -> tokio::io::Result<bool> {
    let mut write_queue = queue![];
    let mut chunk_queue = queue![];

    // Create install directory if it doesn't exist
    tokio::fs::create_dir_all(&install_path).await?;

    let mut file_chunk_num_map = HashMap::new();
    let mut total_bytes = 0u64;

    let m = MultiProgress::new();

    println!("Building folder structure...");
    let mut manifest_rdr = csv::Reader::from_reader(build_manifest_bytes);
    let byte_records = manifest_rdr.byte_records();
    #[cfg(target_os = "macos")]
    let mut mac_app = MacAppExecutables::new();

    for record in byte_records {
        let mut record = record.expect("Failed to get byte record");
        if let None = record.get(5) {
            record.push_field(b"");
        }
        let record = record.deserialize::<BuildManifestRecord>(None).expect("Failed to deserialize build manifest");

        if record.tag == Some(ChangeTag::Modified) || record.tag == Some(ChangeTag::Removed) {
            let file_path = install_path.join(&record.file_name);
            println!("Removing {}", file_path);
            if record.is_directory() {
                println!("{} is a directory", file_path);
                // Is a directory
                if file_path.exists() && file_path.to_path().is_dir() {
                    println!("Deleting {}", file_path);
                    // Delete this directory
                    tokio::fs::remove_dir_all(file_path).await?;
                }
                continue;
            }

            println!("{} is a file", file_path);
            if file_path.exists() && file_path.is_file() {
                println!("Deleting {}", file_path);
                // Delete this file
                tokio::fs::remove_file(file_path).await?;
            }

            if record.tag == Some(ChangeTag::Removed) {
                continue;
            }
        }

        prepare_file(
            &install_path,
            &os,
            &record.file_name,
            record.is_directory(),
            #[cfg(target_os = "macos")] &mut mac_app,
        ).await?;

        if !record.is_directory() {
            file_chunk_num_map.insert(record.file_name.clone(), record.chunks);
            total_bytes += record.size_in_bytes as u64;
        }
    }

    let dl_sty =
        ProgressStyle::with_template("{wide_msg} Download: {binary_bytes_per_sec}").unwrap();
    let wr_sty = ProgressStyle::with_template(
        "{wide_msg} Disk: {binary_bytes_per_sec}\n[{percent}%] {wide_bar} {bytes:>7}/{total_bytes:7} [{eta_precise}]",
    )
    .unwrap()
    .progress_chars("##-");

    let dl_prog = Arc::new(m.add(ProgressBar::new(total_bytes).with_style(dl_sty)));
    let wrt_prog =
        Arc::new(m.insert_after(&dl_prog, ProgressBar::new(total_bytes).with_style(wr_sty)));

    println!("Building queue...");
    let mut manifest_chunks_rdr = csv::Reader::from_reader(build_manifest_chunks_bytes);
    let byte_records = manifest_chunks_rdr.byte_records();
    for record in byte_records {
        let record = record.expect("Failed to get byte record");
        let record = record.deserialize::<BuildManifestChunksRecord>(None).expect("Failed to deserialize chunks manifest");

        let is_last = file_chunk_num_map[&record.file_path] - 1 == usize::from(record.id);
        if is_last {
            file_chunk_num_map.remove(&record.file_path);
        }
        write_queue
            .add((record.sha.clone(), record.id, is_last))
            .unwrap();
        chunk_queue.add(record).unwrap();
    }
    drop(file_chunk_num_map);

    let (tx, rx) =
        async_channel::unbounded::<(BuildManifestChunksRecord, Bytes, OwnedSemaphorePermit)>();

    println!("Spawning write thread...");
    let write_handler = tokio::spawn(async move {
        println!("Write thread started.");

        let mut in_buffer = HashMap::new();
        let mut file_map = HashMap::new();

        while write_queue.size() > 0 {
            let (record, chunk, permit) = match rx.recv().await {
                Ok(msg) => msg,
                Err(_) => {
                    println!("Write channel has closed");
                    break;
                }
            };

            // Some files don't have the chunk id in the sha parts, so they can have reused
            // SHAs for chunks (e.g. DieYoungPrologue-WindowsNoEditor.pak)
            let chunk_key = format!("{},{}", record.id, record.sha);
            in_buffer.insert(chunk_key, (record.file_path, chunk, permit));

            loop {
                match write_queue.peek() {
                    Ok((next_chunk, chunk_id, is_last_chunk)) => {
                        let next_chunk_key = format!("{},{}", chunk_id, next_chunk);
                        if let Some((file_path, bytes, permit)) = in_buffer.remove(&next_chunk_key)
                        {
                            if !file_map.contains_key(&file_path) {
                                let chunk_file_path = install_path.join(&file_path);
                                let file = open_file(&chunk_file_path)
                                    .await
                                    .expect(&format!("Failed to open {}", chunk_file_path));
                                file_map.insert(file_path.clone(), file);
                            }
                            let file = file_map.get_mut(&file_path).unwrap();
                            write_queue.remove().unwrap();
                            // println!("Writing {}", next_chunk);
                            let bytes_written = bytes.len();
                            append_chunk(file, bytes).await.expect(&format!(
                                "Failed to write {}.bin to {}",
                                next_chunk, file_path
                            ));
                            drop(permit);

                            wrt_prog.inc(bytes_written as u64);

                            if is_last_chunk {
                                file_map.remove(&file_path);
                            }

                            continue;
                        }

                        // println!(
                        //     "Not ready to write {}: {} pending",
                        //     next_chunk,
                        //     in_buffer.len()
                        // );

                        break;
                    }
                    Err(_) => {
                        println!("No more chunks to write");
                        return;
                    }
                }
            }
        }
        println!("Write thread finished.");
    });

    println!("Downloading chunks...");
    let max_chunks_in_memory = max_memory_usage / *MAX_CHUNK_SIZE;
    let mem_semaphore = Arc::new(Semaphore::new(max_chunks_in_memory));
    let dl_semaphore = Arc::new(Semaphore::new(max_download_workers));
    while let Ok(record) = chunk_queue.remove() {
        let mem_permit = mem_semaphore.clone().acquire_owned().await.unwrap();
        let client = client.clone();
        let product = product.clone();
        let os = os.clone();
        let thread_tx = tx.clone();
        let dl_prog = dl_prog.clone();
        let dl_semaphore = dl_semaphore.clone();

        tokio::spawn(async move {
            // println!("Downloading {}", record.sha);
            let dl_permit = dl_semaphore.acquire().await.unwrap();
            let chunk = api::product::download_chunk(&client, &product, &os, &record.sha)
                .await
                .expect(&format!("Failed to download {}.bin", &record.sha));
            drop(dl_permit);

            dl_prog.inc(chunk.len() as u64);

            if !skip_verify {
                let chunk_parts = &record.sha.split("_").collect::<Vec<&str>>();
                match chunk_parts.last() {
                    Some(chunk_sha) => {
                        // println!("Verifying {}", record.sha);
                        let chunk_corrupted = !verify_chunk(&chunk, chunk_sha);

                        if chunk_corrupted {
                            println!("Sha: {}", chunk_sha);
                            println!(
                                "{} failed verification. {} is corrupted.",
                                &record.sha, &record.file_path
                            );
                            return false;
                        }
                    }
                    None => {
                        println!("Couldn't find Chunk SHA. Skipping verification...");
                    }
                }
            }

            thread_tx.send((record, chunk, mem_permit)).await.unwrap();

            true
        });
    }

    println!("Waiting for write thread to finish...");
    write_handler.await?;

    #[cfg(target_os = "macos")]
    if *os == BuildOs::Mac {
        mac_app.mark_as_executable().await?;
    }

    // TODO: Redo logic for verification
    Ok(true)
}

async fn open_file(file_path: &OsPath) -> tokio::io::Result<File> {
    tokio::fs::OpenOptions::new()
        .append(true)
        .open(file_path)
        .await
}

async fn append_chunk(file: &mut tokio::fs::File, chunk: Bytes) -> tokio::io::Result<()> {
    file.write_all(&chunk).await
}

async fn prepare_file(
    base_install_path: &OsPath,
    os: &BuildOs,
    file_name: &String,
    is_directory: bool,
    #[cfg(target_os = "macos")]
    mac_executable: &mut MacAppExecutables,
) -> tokio::io::Result<()> {
    let file_path = base_install_path.join(file_name);

    // File is a directory. We should create this directory.
    if is_directory {
        if !file_path.exists() {
            tokio::fs::create_dir(&file_path).await?;
        }
    } else {
        // Create empty file.
        tokio::fs::File::create(&file_path).await?;
    }

    #[cfg(target_os = "macos")]
    if os == &BuildOs::Mac && mac_executable.plist.is_none() {
        match file_path.extension() {
            Some(ext) => {
                let is_plist = &ext == "plist" && match file_path.parent() {
                    Some(parent) => parent.name() == Some(&String::from("Contents")) && match parent.parent() {
                        Some(parent) => {
                            parent.name().unwrap().ends_with(".app")
                        }
                        None => false,
                    },
                    None => false,
                };
                if is_plist {
                    mac_executable.set_plist(file_path.to_pathbuf());
                }
            }
            None => {},
        };
    }

    Ok(())
}

fn verify_file_hash(file_path: &OsPath, sha: &str) -> std::io::Result<bool> {
    let mut file = std::fs::File::open(file_path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    let hash = hasher.finalize();
    let file_sha = base16ct::lower::encode_string(&hash);

    Ok(file_sha == sha)
}

fn verify_chunk(chunk: &Bytes, sha: &str) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(chunk);
    let hash = hasher.finalize();
    let sha_str = base16ct::lower::encode_string(&hash);

    sha_str == sha
}
