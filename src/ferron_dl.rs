use std::path::PathBuf;

pub async fn obtain_ferron() -> std::io::Result<PathBuf> {
    let ferron_dl_dir = dirs::data_local_dir()
        .ok_or(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "data local directory not found",
        ))?
        .join("ferron-language-server");

    // 1. Obtain the latest version info for Ferron 3 from https://dl.ferron.sh/latest3.ferron
    let latest_ferron_version = match get_latest_ferron_version().await {
        Ok(version) => version,
        Err(e) => {
            if tokio::fs::try_exists(&ferron_dl_dir).await.unwrap_or(false) {
                return Ok(ferron_dl_dir);
            } else {
                return Err(e);
            }
        }
    };

    if !tokio::fs::try_exists(&ferron_dl_dir).await.unwrap_or(false) {
        tokio::fs::create_dir_all(&ferron_dl_dir).await?;
    }

    let current_ferron_version = tokio::fs::read_to_string(ferron_dl_dir.join("ferron.exe"))
        .await
        .ok()
        .map(|s| s.trim().to_owned());
    if current_ferron_version.as_deref() == Some(&*latest_ferron_version) {
        // Already on the latest version, no need to download
        return Ok(ferron_dl_dir);
    }

    // 2. Obtain the latest version of Ferron 3 from:
    // - Windows: https://dl.ferron.sh/<version>/ferron-<version>-<targettriple>.zip
    // - Others: https://dl.ferron.sh/<version>/ferron-<version>-<targettriple>.tar.gz
    let ferron_url = if cfg!(windows) {
        format!(
            "https://dl.ferron.sh/{}/ferron-{}-{}.zip",
            latest_ferron_version,
            latest_ferron_version,
            crate::build::BUILD_TARGET
        )
    } else {
        format!(
            "https://dl.ferron.sh/{}/ferron-{}-{}.tar.gz",
            latest_ferron_version,
            latest_ferron_version,
            crate::build::BUILD_TARGET
        )
    };
    let ferron_archive = reqwest::get(&ferron_url)
        .await
        .map_err(std::io::Error::other)?
        .error_for_status()
        .map_err(std::io::Error::other)?
        .bytes()
        .await
        .map_err(std::io::Error::other)?;

    // 3. Extract the archive to the download directory
    #[cfg(not(windows))]
    {
        let mut gunzipped = async_compression::tokio::bufread::GzipDecoder::new(
            std::io::Cursor::new(ferron_archive),
        );
        tokio_tar::Archive::new(&mut gunzipped)
            .unpack(&ferron_dl_dir)
            .await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            // Make `ferron` and `ferron-fmt` executable
            if let Ok(metadata) = tokio::fs::metadata(&ferron_dl_dir.join("ferron")).await {
                let mut permissions = metadata.permissions();
                let old_mode = permissions.mode();
                let mode_read = old_mode & 0o444;
                let mode_exec = mode_read >> 2; // r--r--r-- => --x--x--x
                permissions.set_mode(old_mode | mode_exec);
                tokio::fs::set_permissions(&ferron_dl_dir.join("ferron"), permissions).await?;
            }
            if let Ok(metadata) = tokio::fs::metadata(&ferron_dl_dir.join("ferron-fmt")).await {
                let mut permissions = metadata.permissions();
                let old_mode = permissions.mode();
                let mode_read = old_mode & 0o444;
                let mode_exec = mode_read >> 2; // r--r--r-- => --x--x--x
                permissions.set_mode(old_mode | mode_exec);
                tokio::fs::set_permissions(&ferron_dl_dir.join("ferron-fmt"), permissions).await?;
            }
        }
    }
    #[cfg(windows)]
    {
        use tokio_util::compat::TokioAsyncWriteCompatExt;

        let mut reader = async_zip::tokio::read::seek::ZipFileReader::with_tokio(
            std::io::Cursor::new(ferron_archive),
        )
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        // Taken from https://github.com/Majored/rs-async-zip/blob/main/examples/file_extraction.rs
        for index in 0..reader.file().entries().len() {
            let Some(entry) = reader.file().entries().get(index) else {
                continue;
            };
            let path = ferron_dl_dir.join(
                entry
                    .filename()
                    .as_str()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
                    .replace('\\', "/")
                    .split('/')
                    .map(sanitize_filename::sanitize)
                    .collect::<PathBuf>(),
            );
            // If the filename of the entry ends with '/', it is treated as a directory.
            // This is implemented by previous versions of this crate and the Python Standard Library.
            // https://docs.rs/async_zip/0.0.8/src/async_zip/read/mod.rs.html#63-65
            // https://github.com/python/cpython/blob/820ef62833bd2d84a141adedd9a05998595d6b6d/Lib/zipfile.py#L528
            let entry_is_dir = entry
                .dir()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

            let mut entry_reader = reader
                .reader_without_entry(index)
                .await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

            if entry_is_dir {
                // The directory may have been created if iteration is out of order.
                if !path.exists() {
                    tokio::fs::create_dir_all(&path).await.map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Failed to create extracted directory: {}", e),
                        )
                    })?;
                }
            } else {
                // Creates parent directories. They may not exist if iteration is out of order
                // or the archive does not contain directory entries.
                let parent = path.parent().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!(
                            "A file entry should have parent directories: {}",
                            path.display()
                        ),
                    )
                })?;
                if !parent.is_dir() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Failed to create parent directories: {}", e),
                        )
                    })?;
                }
                let writer = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .await
                    .map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Failed to create extracted file: {}", e),
                        )
                    })?;
                futures_util::io::copy(&mut entry_reader, &mut writer.compat_write())
                    .await
                    .map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Failed to copy to extracted file: {}", e),
                        )
                    })?;

                // Closes the file and manipulates its metadata here if you wish to preserve its metadata from the archive.
            }
        }
    }

    Ok(ferron_dl_dir)
}

#[inline]
async fn get_latest_ferron_version() -> Result<String, std::io::Error> {
    Ok(reqwest::get("https://dl.ferron.sh/latest3.ferron")
        .await
        .map_err(std::io::Error::other)?
        .error_for_status()
        .map_err(std::io::Error::other)?
        .text()
        .await
        .map_err(std::io::Error::other)?
        .trim()
        .to_owned())
}
