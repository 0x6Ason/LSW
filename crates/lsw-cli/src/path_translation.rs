// SPDX-License-Identifier: GPL-3.0-or-later

//! Explicit, syntactic path conversion for command composition.
//!
//! Translation does not imply that the independent Windows guest can access a
//! host file. Use `lsw push`, `lsw pull`, or `lsw sync` to move content.

use std::ffi::OsString;

pub(super) fn command(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let [direction, value] = arguments else {
        return Err("usage: lsw path <--windows|-w|--unix|-u> PATH".into());
    };
    let direction = direction
        .to_str()
        .ok_or("path direction must be valid UTF-8")?;
    let value = value.to_str().ok_or("path must be valid UTF-8")?;
    let translated = match direction {
        "--windows" | "-w" => to_windows(value)?,
        "--unix" | "--host" | "-u" => to_unix(value)?,
        _ => return Err("usage: lsw path <--windows|-w|--unix|-u> PATH".into()),
    };
    println!("{translated}");
    Ok(())
}

fn to_windows(value: &str) -> Result<String, Box<dyn std::error::Error>> {
    let normalized = value.replace('\\', "/");
    let Some(remainder) = normalized.strip_prefix("/mnt/") else {
        return Err("Unix path must begin with /mnt/<drive>/".into());
    };
    let mut components = remainder.split('/');
    let drive = components.next().unwrap_or_default();
    if drive.len() != 1 || !drive.as_bytes()[0].is_ascii_alphabetic() {
        return Err("Unix path must begin with /mnt/<drive>/".into());
    }
    let components = validated_components(components)?;
    let mut windows = format!("{}:\\", drive.to_ascii_uppercase());
    windows.push_str(&components.join("\\"));
    Ok(windows)
}

fn to_unix(value: &str) -> Result<String, Box<dyn std::error::Error>> {
    let bytes = value.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return Err("Windows path must begin with an absolute drive path such as C:\\".into());
    }
    if bytes.len() > 2 && bytes[2] != b'\\' && bytes[2] != b'/' {
        return Err("drive-relative Windows paths are not supported".into());
    }
    let drive = (bytes[0] as char).to_ascii_lowercase();
    let remainder = value
        .get(2..)
        .unwrap_or_default()
        .trim_start_matches(['/', '\\']);
    let components = validated_components(remainder.split(['/', '\\']))?;
    if components.is_empty() {
        Ok(format!("/mnt/{drive}"))
    } else {
        Ok(format!("/mnt/{drive}/{}", components.join("/")))
    }
}

fn validated_components<'a>(
    components: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<&'a str>, Box<dyn std::error::Error>> {
    let mut validated = Vec::new();
    for component in components
        .into_iter()
        .filter(|component| !component.is_empty())
    {
        if component == "."
            || component == ".."
            || component.ends_with([' ', '.'])
            || component
                .chars()
                .any(|character| character <= '\u{1f}' || "<>:\"|?*".contains(character))
        {
            return Err(format!("ambiguous or invalid path component {component:?}").into());
        }
        validated.push(component);
    }
    Ok(validated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_paths_round_trip_without_touching_the_filesystem() {
        assert_eq!(
            to_windows("/mnt/c/Users/Jason/project").unwrap(),
            "C:\\Users\\Jason\\project"
        );
        assert_eq!(
            to_unix("D:\\Build Output\\app.exe").unwrap(),
            "/mnt/d/Build Output/app.exe"
        );
        assert_eq!(to_windows("/mnt/e").unwrap(), "E:\\");
        assert_eq!(to_unix("E:\\").unwrap(), "/mnt/e");
    }

    #[test]
    fn ambiguous_paths_are_rejected() {
        assert!(to_windows("/home/user/file").is_err());
        assert!(to_unix("relative\\file").is_err());
        assert!(to_unix("C:relative").is_err());
        assert!(to_windows("/mnt/c/../Windows").is_err());
        assert!(to_unix("C:\\src\\..\\Windows").is_err());
        assert!(to_windows("/mnt/c/file:name").is_err());
    }
}
