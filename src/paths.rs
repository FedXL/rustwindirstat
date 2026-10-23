use std::path::{Path, PathBuf};

pub fn normalize_owned(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        match path.components().next() {
            Some(Component::Prefix(prefix)) => {
                if matches!(prefix.kind(), Prefix::Disk(letter) if letter.is_ascii_uppercase()) {
                    return path;
                }
            }
            _ => return path,
        }
    }
    #[cfg(not(windows))]
    {
        return path;
    }
    #[cfg(windows)]
    normalize(&path)
}

/// Normalize separators and drive prefixes without converting native names to Unicode.
pub fn normalize(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::path::{Component, Prefix};
        let mut components = path.components();
        let mut result = match components.next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(letter) => {
                    PathBuf::from(format!("{}:", char::from(letter.to_ascii_uppercase())))
                }
                Prefix::DeviceNS(name) => {
                    let units: Vec<_> = name.encode_wide().collect();
                    if units.len() == 2
                        && units[0] <= 127
                        && units[1] == u16::from(b':')
                        && (units[0] as u8).is_ascii_alphabetic()
                    {
                        PathBuf::from(format!(
                            "{}:",
                            char::from((units[0] as u8).to_ascii_uppercase())
                        ))
                    } else {
                        PathBuf::from(prefix.as_os_str())
                    }
                }
                _ => PathBuf::from(prefix.as_os_str()),
            },
            Some(component) => PathBuf::from(component.as_os_str()),
            None => return PathBuf::new(),
        };
        for component in components {
            result.push(component.as_os_str());
        }
        result
    }
    #[cfg(not(windows))]
    path.components().collect()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    #[test]
    fn normalization_preserves_distinct_native_names() {
        let a = PathBuf::from(OsString::from_wide(&[67, 58, 92, 0xD800]));
        let b = PathBuf::from(OsString::from_wide(&[67, 58, 92, 0xD801]));
        assert_ne!(normalize(&a), normalize(&b));
        assert_eq!(
            normalize(Path::new(r"\\.\c:\data")),
            PathBuf::from(r"C:\data")
        );
        assert_eq!(normalize(Path::new("c:/data")), PathBuf::from(r"C:\data"));
    }
}
