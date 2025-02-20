use crate::event::emit::{
    emit_loading, init_or_edit_loading, loading_try_for_each_concurrent,
};
use crate::event::LoadingBarType;
use crate::pack::install_from::{
    set_profile_information, EnvType, PackFile, PackFileHash,
};
use crate::prelude::ProfilePathId;
use crate::state::{ProfileInstallStage, Profiles, SideType};
use crate::util::fetch::{fetch_mirrors, write};
use crate::util::io;
use crate::{profile, State};
use async_zip::tokio::read::seek::ZipFileReader;

use std::io::Cursor;
use std::path::{Component, PathBuf};

use super::install_from::{
    generate_pack_from_file, generate_pack_from_version_id, CreatePack,
    CreatePackLocation, PackFormat,
};

/// Install a pack
/// Wrapper around install_pack_files that generates a pack creation description, and
/// attempts to install the pack files. If it fails, it will remove the profile (fail safely)
/// Install a modpack from a mrpack file (a modrinth .zip format)
#[theseus_macros::debug_pin]
pub async fn install_zipped_mrpack(
    location: CreatePackLocation,
    profile_path: ProfilePathId,
) -> crate::Result<ProfilePathId> {
    // Get file from description
    let create_pack: CreatePack = match location {
        CreatePackLocation::FromVersionId {
            project_id,
            version_id,
            title,
            icon_url,
        } => {
            generate_pack_from_version_id(
                project_id,
                version_id,
                title,
                icon_url,
                profile_path.clone(),
            )
            .await?
        }
        CreatePackLocation::FromFile { path } => {
            generate_pack_from_file(path, profile_path.clone()).await?
        }
    };

    // Install pack files, and if it fails, fail safely by removing the profile
    let result = install_zipped_mrpack_files(create_pack).await;

    // Check existing managed packs for potential updates
    tokio::task::spawn(Profiles::update_modrinth_versions());

    match result {
        Ok(profile) => Ok(profile),
        Err(err) => {
            let _ = crate::api::profile::remove(&profile_path).await;

            Err(err)
        }
    }
}

/// Install all pack files from a description
/// Does not remove the profile if it fails
#[theseus_macros::debug_pin]
pub async fn install_zipped_mrpack_files(
    create_pack: CreatePack,
) -> crate::Result<ProfilePathId> {
    let state = &State::get().await?;

    let file = create_pack.file;
    let description = create_pack.description.clone(); // make a copy for profile edit function
    let icon = create_pack.description.icon;
    let project_id = create_pack.description.project_id;
    let version_id = create_pack.description.version_id;
    let existing_loading_bar = create_pack.description.existing_loading_bar;
    let profile_path = create_pack.description.profile_path;
    let icon_exists = icon.is_some();

    let reader: Cursor<&bytes::Bytes> = Cursor::new(&file);

    // Create zip reader around file
    let mut zip_reader = ZipFileReader::new(reader).await.map_err(|_| {
        crate::Error::from(crate::ErrorKind::InputError(
            "Failed to read input modpack zip".to_string(),
        ))
    })?;

    // Extract index of modrinth.index.json
    let zip_index_option = zip_reader
        .file()
        .entries()
        .iter()
        .position(|f| f.entry().filename() == "modrinth.index.json");
    if let Some(zip_index) = zip_index_option {
        let mut manifest = String::new();
        let entry = zip_reader
            .file()
            .entries()
            .get(zip_index)
            .unwrap()
            .entry()
            .clone();
        let mut reader = zip_reader.entry(zip_index).await?;
        reader.read_to_string_checked(&mut manifest, &entry).await?;

        let pack: PackFormat = serde_json::from_str(&manifest)?;

        if &*pack.game != "minecraft" {
            return Err(crate::ErrorKind::InputError(
                "Pack does not support Minecraft".to_string(),
            )
            .into());
        }

        // Sets generated profile attributes to the pack ones (using profile::edit)
        set_profile_information(
            profile_path.clone(),
            &description,
            &pack.name,
            &pack.dependencies,
        )
        .await?;

        let profile_path = profile_path.clone();
        let loading_bar = init_or_edit_loading(
            existing_loading_bar,
            LoadingBarType::PackDownload {
                profile_path: profile_path.get_full_path().await?.clone(),
                pack_name: pack.name.clone(),
                icon,
                pack_id: project_id,
                pack_version: version_id,
            },
            100.0,
            "Downloading modpack",
        )
        .await?;

        let num_files = pack.files.len();
        use futures::StreamExt;
        loading_try_for_each_concurrent(
            futures::stream::iter(pack.files.into_iter())
                .map(Ok::<PackFile, crate::Error>),
            None,
            Some(&loading_bar),
            70.0,
            num_files,
            None,
            |project| {
                let profile_path = profile_path.clone();
                async move {
                    //TODO: Future update: prompt user for optional files in a modpack
                    if let Some(env) = project.env {
                        if env
                            .get(&EnvType::Client)
                            .map(|x| x == &SideType::Unsupported)
                            .unwrap_or(false)
                        {
                            return Ok(());
                        }
                    }

                    let creds = state.credentials.read().await;
                    let file = fetch_mirrors(
                        &project
                            .downloads
                            .iter()
                            .map(|x| &**x)
                            .collect::<Vec<&str>>(),
                        project.hashes.get(&PackFileHash::Sha1).map(|x| &**x),
                        &state.fetch_semaphore,
                        &creds,
                    )
                    .await?;
                    drop(creds);

                    let path =
                        std::path::Path::new(&project.path).components().next();
                    if let Some(path) = path {
                        match path {
                            Component::CurDir | Component::Normal(_) => {
                                let path = profile_path
                                    .get_full_path()
                                    .await?
                                    .join(&project.path);
                                write(&path, &file, &state.io_semaphore)
                                    .await?;
                            }
                            _ => {}
                        };
                    }
                    Ok(())
                }
            },
        )
        .await?;

        emit_loading(&loading_bar, 0.0, Some("Extracting overrides")).await?;

        let mut total_len = 0;

        for index in 0..zip_reader.file().entries().len() {
            let file = zip_reader.file().entries().get(index).unwrap().entry();

            if (file.filename().starts_with("overrides")
                || file.filename().starts_with("client_overrides"))
                && !file.filename().ends_with('/')
            {
                total_len += 1;
            }
        }

        for index in 0..zip_reader.file().entries().len() {
            let file = zip_reader
                .file()
                .entries()
                .get(index)
                .unwrap()
                .entry()
                .clone();

            let file_path = PathBuf::from(file.filename());
            if (file.filename().starts_with("overrides")
                || file.filename().starts_with("client_overrides"))
                && !file.filename().ends_with('/')
            {
                // Reads the file into the 'content' variable
                let mut content = Vec::new();
                let mut reader = zip_reader.entry(index).await?;
                reader.read_to_end_checked(&mut content, &file).await?;

                let mut new_path = PathBuf::new();
                let components = file_path.components().skip(1);

                for component in components {
                    new_path.push(component);
                }

                if new_path.file_name().is_some() {
                    write(
                        &profile_path.get_full_path().await?.join(new_path),
                        &content,
                        &state.io_semaphore,
                    )
                    .await?;
                }

                emit_loading(
                    &loading_bar,
                    30.0 / total_len as f64,
                    Some(&format!(
                        "Extracting override {}/{}",
                        index, total_len
                    )),
                )
                .await?;
            }
        }

        // If the icon doesn't exist, we expect icon.png to be a potential icon.
        // If it doesn't exist, and an override to icon.png exists, cache and use that
        let potential_icon =
            profile_path.get_full_path().await?.join("icon.png");
        if !icon_exists && potential_icon.exists() {
            profile::edit_icon(&profile_path, Some(&potential_icon)).await?;
        }

        if let Some(profile_val) =
            crate::api::profile::get(&profile_path, None).await?
        {
            crate::launcher::install_minecraft(&profile_val, Some(loading_bar))
                .await?;

            State::sync().await?;
        }

        Ok::<ProfilePathId, crate::Error>(profile_path.clone())
    } else {
        Err(crate::Error::from(crate::ErrorKind::InputError(
            "No pack manifest found in mrpack".to_string(),
        )))
    }
}

#[tracing::instrument(skip(mrpack_file))]
#[theseus_macros::debug_pin]
pub async fn remove_all_related_files(
    profile_path: ProfilePathId,
    mrpack_file: bytes::Bytes,
) -> crate::Result<()> {
    let reader: Cursor<&bytes::Bytes> = Cursor::new(&mrpack_file);

    // Create zip reader around file
    let mut zip_reader = ZipFileReader::new(reader).await.map_err(|_| {
        crate::Error::from(crate::ErrorKind::InputError(
            "Failed to read input modpack zip".to_string(),
        ))
    })?;

    // Extract index of modrinth.index.json
    let zip_index_option = zip_reader
        .file()
        .entries()
        .iter()
        .position(|f| f.entry().filename() == "modrinth.index.json");
    if let Some(zip_index) = zip_index_option {
        let mut manifest = String::new();
        let entry = zip_reader
            .file()
            .entries()
            .get(zip_index)
            .unwrap()
            .entry()
            .clone();
        let mut reader = zip_reader.entry(zip_index).await?;
        reader.read_to_string_checked(&mut manifest, &entry).await?;

        let pack: PackFormat = serde_json::from_str(&manifest)?;

        if &*pack.game != "minecraft" {
            return Err(crate::ErrorKind::InputError(
                "Pack does not support Minecraft".to_string(),
            )
            .into());
        }

        // Set install stage to installing, and do not change it back (as files are being removed and are not being reinstalled here)
        crate::api::profile::edit(&profile_path, |prof| {
            prof.install_stage = ProfileInstallStage::PackInstalling;
            async { Ok(()) }
        })
        .await?;

        let num_files = pack.files.len();
        use futures::StreamExt;
        loading_try_for_each_concurrent(
            futures::stream::iter(pack.files.into_iter())
                .map(Ok::<PackFile, crate::Error>),
            None,
            None,
            0.0,
            num_files,
            None,
            |project| {
                let profile_path = profile_path.clone();
                async move {
                    // Remove this file if a corresponding one exists in the filesystem
                    let existing_file =
                        profile_path.get_full_path().await?.join(&project.path);
                    if existing_file.exists() {
                        io::remove_file(&existing_file).await?;
                    }

                    Ok(())
                }
            },
        )
        .await?;

        // Iterate over each 'overrides' file and remove it
        for index in 0..zip_reader.file().entries().len() {
            let file = zip_reader
                .file()
                .entries()
                .get(index)
                .unwrap()
                .entry()
                .clone();

            let file_path = PathBuf::from(file.filename());
            if (file.filename().starts_with("overrides")
                || file.filename().starts_with("client_overrides"))
                && !file.filename().ends_with('/')
            {
                let mut new_path = PathBuf::new();
                let components = file_path.components().skip(1);

                for component in components {
                    new_path.push(component);
                }

                // Remove this file if a corresponding one exists in the filesystem
                let existing_file =
                    profile_path.get_full_path().await?.join(&new_path);
                if existing_file.exists() {
                    io::remove_file(&existing_file).await?;
                }
            }
        }
        Ok(())
    } else {
        Err(crate::Error::from(crate::ErrorKind::InputError(
            "No pack manifest found in mrpack".to_string(),
        )))
    }
}
