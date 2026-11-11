use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, bail};
use image::{ImageFormat, ImageReader};
use rayon::prelude::*;
use strum::{EnumIter, EnumMessage, EnumString};

mod pdf;
mod rar;
mod zip;

#[derive(Clone, Copy, EnumIter, EnumMessage, EnumString)]
#[strum(ascii_case_insensitive)]
pub(super) enum Format {
    #[strum(
        serialize = "png",
        serialize = "jpg",
        serialize = "jpeg",
        serialize = "webp"
    )]
    Raster,
    #[strum(serialize = "cbz", serialize = "zip")]
    Zip,
    #[strum(serialize = "rar")]
    Rar,
    #[strum(serialize = "pdf")]
    Pdf,
}

#[derive(Debug)]
pub(super) struct EncodedPage {
    pub(super) name: String,
    pub(super) bytes: Vec<u8>,
}

pub(super) struct Page {
    pub(super) name: String,
    pub(super) bytes: Arc<[u8]>,
    pub(super) format: ImageFormat,
    pub(super) width: u32,
    pub(super) height: u32,
}

fn decode(path: &Path, source: EncodedPage) -> Result<Page> {
    let EncodedPage { name, bytes } = source;
    let format = image::guess_format(&bytes).with_context(|| {
        format!(
            "failed to identify imported image {} ({name})",
            path.display()
        )
    })?;
    let (width, height) = ImageReader::with_format(Cursor::new(bytes.as_slice()), format)
        .into_dimensions()
        .with_context(|| {
            format!(
                "failed to read dimensions of imported image {} ({name})",
                path.display()
            )
        })?;
    Ok(Page {
        name,
        bytes: Arc::<[u8]>::from(bytes),
        format,
        width,
        height,
    })
}

fn raster_name(path: &Path, folder: Option<&Path>) -> String {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "page".to_owned());
    let Some(folder) = folder else {
        return file_name;
    };
    let Some(parent) = path.parent() else {
        return file_name;
    };
    if parent == folder {
        return file_name;
    }
    parent
        .file_name()
        .map(|name| format!("{}_{}", name.to_string_lossy(), file_name))
        .unwrap_or(file_name)
}

pub(super) fn import(mut paths: Vec<PathBuf>, folder: Option<&Path>) -> Result<Vec<Page>> {
    alphanumeric_sort::sort_slice_by_os_str_key(&mut paths, |path| {
        path.file_name().unwrap_or_else(|| path.as_os_str())
    });
    let mut groups = paths
        .into_par_iter()
        .map(|path| -> Result<Vec<Page>> {
            let extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .and_then(|extension| extension.parse::<Format>().ok());
            let encoded = match extension {
                Some(Format::Raster) => vec![EncodedPage {
                    name: raster_name(&path, folder),
                    bytes: fs::read(&path)
                        .with_context(|| format!("failed to read {}", path.display()))?,
                }],
                Some(Format::Zip) => zip::extract(&path)?,
                Some(Format::Rar) => rar::extract(&path)?,
                Some(Format::Pdf) => pdf::render(&path)?,
                None => bail!("unsupported page import path {}", path.display()),
            };
            encoded
                .into_iter()
                .map(|source| decode(&path, source))
                .collect()
        })
        .collect::<Result<Vec<_>>>()?;
    let page_count = groups.iter().map(Vec::len).sum();
    let mut pages = Vec::with_capacity(page_count);
    for group in &mut groups {
        pages.append(group);
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_paths_are_naturally_sorted() {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "koharu-import-order-{}-{timestamp}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("create fixture directory");
        let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255]));
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, ImageFormat::Png)
            .expect("encode fixture");
        let paths = ["page10.PNG", "page2.png", "page1.png"].map(|name| directory.join(name));
        for path in &paths {
            fs::write(path, encoded.get_ref()).expect("write fixture");
        }

        let pages = import(paths.into(), None).expect("import fixtures");
        fs::remove_dir_all(&directory).expect("remove fixture directory");
        assert_eq!(
            pages
                .iter()
                .map(|page| page.name.as_str())
                .collect::<Vec<_>>(),
            ["page1.png", "page2.png", "page10.PNG"]
        );
    }

    #[test]
    fn prefixes_raster_names_with_their_subfolder() {
        let folder = Path::new("book");
        assert_eq!(
            raster_name(Path::new("book/chapter 1/page01.png"), Some(folder)),
            "chapter 1_page01.png"
        );
        assert_eq!(
            raster_name(Path::new("book/page01.png"), Some(folder)),
            "page01.png"
        );
    }
}
