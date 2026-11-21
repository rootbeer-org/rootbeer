use crate::cpio;
use crate::tree::Tree;
use crate::xar::Xar;
use flate2::read::GzDecoder;
use quick_xml::events::Event;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Unpacks each component's payload where the installer would put it, so
/// `/Applications/Tool.app` lands at `Applications/Tool.app`. Scripts never
/// run.
pub(crate) fn unpack(archive: &Path, tree: &Tree) -> Result<(), String> {
    let mut package = Xar::open(archive)?;

    // A product package holds a directory per component, and a component
    // package is one component at the top.
    let mut components = package
        .paths()
        .filter(|(path, is_directory)| {
            *is_directory && !path.contains('/') && path.ends_with(".pkg")
        })
        .map(|(path, _)| format!("{path}/"))
        .collect::<Vec<_>>();
    if components.is_empty() && package.has("PackageInfo") {
        components.push(String::new());
    }

    if components.is_empty() {
        return Err("the package has no components".into());
    }

    for component in components {
        let shown = component.trim_end_matches('/');
        let shown = if shown.is_empty() {
            "the package"
        } else {
            shown
        };

        let mut info = String::new();
        package
            .read(&format!("{component}PackageInfo"))?
            .read_to_string(&mut info)
            .map_err(|error| format!("{shown}: {error}"))?;
        let location = install_location(&info).map_err(|error| format!("{shown}: {error}"))?;

        // A component can install nothing and only run scripts.
        let payload = format!("{component}Payload");
        if !package.has(&payload) {
            continue;
        }

        let mut spooled = tempfile::tempfile().map_err(|error| error.to_string())?;
        io::copy(&mut package.read(&payload)?, &mut spooled)
            .map_err(|error| format!("{shown}: {error}"))?;
        spooled
            .seek(SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;

        let archive = decompress(spooled).map_err(|error| format!("{shown}: {error}"))?;
        cpio::unpack(archive, tree, &location).map_err(|error| format!("{shown}: {error}"))?;
    }

    Ok(())
}

/// The one package at the top of an unpacked disk image.
pub(crate) fn only(image: &Path) -> Result<PathBuf, String> {
    let packages = fs::read_dir(image)
        .map_err(|error| format!("{}: {error}", image.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "pkg") && path.is_file()
        })
        .collect::<Vec<_>>();

    match packages.as_slice() {
        [package] => Ok(package.clone()),
        _ => Err(format!(
            "expected one package in the image, found {}",
            packages.len()
        )),
    }
}

/// A payload is a cpio archive, compressed with pbzx in newer packages and
/// gzip in older ones.
fn decompress(mut payload: File) -> Result<Box<dyn Read>, String> {
    let mut magic = [0u8; 4];
    payload
        .read_exact(&mut magic)
        .map_err(|error| format!("its payload is empty ({error})"))?;
    payload
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;

    match &magic {
        b"pbzx" => {
            let mut archive = tempfile::tempfile().map_err(|error| error.to_string())?;
            pbzx::PbzxReader::new(BufReader::new(payload))
                .and_then(|mut reader| reader.decompress_to(&mut archive))
                .map_err(|error| error.to_string())?;
            archive
                .seek(SeekFrom::Start(0))
                .map_err(|error| error.to_string())?;
            Ok(Box::new(BufReader::new(archive)))
        }
        [0x1f, 0x8b, ..] => Ok(Box::new(GzDecoder::new(BufReader::new(payload)))),
        b"0707" => Ok(Box::new(BufReader::new(payload))),
        _ => Err("its payload is neither pbzx, gzip, nor cpio".into()),
    }
}

/// The directory a component installs into, from its PackageInfo.
fn install_location(info: &str) -> Result<String, String> {
    let mut reader = quick_xml::Reader::from_str(info);
    loop {
        match reader.read_event().map_err(|error| error.to_string())? {
            Event::Start(element) | Event::Empty(element)
                if element.name().as_ref() == "pkg-info" =>
            {
                let location = element
                    .try_get_attribute("install-location")
                    .map_err(|error| error.to_string())?;
                let Some(location) = location else {
                    return Ok("/".into());
                };

                let location = location
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .map_err(|error| error.to_string())?;
                return Ok(location.into_owned());
            }
            Event::Eof => return Err("its PackageInfo has no pkg-info".into()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_component_installs_where_its_package_info_says() {
        let info = r#"<?xml version="1.0"?>
<pkg-info format-version="2" identifier="com.example.tool" install-location="/Applications" auth="root">
<payload numberOfFiles="3"/>
</pkg-info>"#;
        assert_eq!(install_location(info).unwrap(), "/Applications");

        let info = r#"<pkg-info identifier="com.example.tool"/>"#;
        assert_eq!(install_location(info).unwrap(), "/");

        assert!(install_location("<other/>").is_err());
    }
}
