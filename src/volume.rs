use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Volume {
    pub id: u64,
    pub mount_path: PathBuf,
    pub filesystem: String,
    pub internal: bool,
}

#[cfg(target_os = "macos")]
pub fn discover_mounted_volumes() -> std::io::Result<Vec<Volume>> {
    use std::ffi::CStr;
    use std::hash::{DefaultHasher, Hash, Hasher};

    let mut mounts = std::ptr::null_mut();
    let count = unsafe { libc::getmntinfo(&mut mounts, libc::MNT_NOWAIT) };
    if count <= 0 || mounts.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let mut volumes = Vec::new();
    for mount in unsafe { std::slice::from_raw_parts(mounts, count as usize) } {
        let mount_path = unsafe { CStr::from_ptr(mount.f_mntonname.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let filesystem = unsafe { CStr::from_ptr(mount.f_fstypename.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let local = mount.f_flags & libc::MNT_LOCAL as u32 != 0;
        let mount_path = PathBuf::from(mount_path);
        let mut id = DefaultHasher::new();
        mount_path.hash(&mut id);
        volumes.push(Volume {
            id: id.finish(),
            internal: local && !mount_path.starts_with("/Volumes"),
            mount_path,
            filesystem,
        });
    }
    volumes.sort_by(|left, right| left.mount_path.cmp(&right.mount_path));
    Ok(volumes)
}

#[cfg(not(target_os = "macos"))]
pub fn discover_mounted_volumes() -> std::io::Result<Vec<Volume>> {
    Ok(Vec::new())
}

pub fn volume_containing<'a>(volumes: &'a [Volume], root: &Path) -> Option<&'a Volume> {
    volumes
        .iter()
        .filter(|volume| root.starts_with(&volume.mount_path))
        .max_by_key(|volume| volume.mount_path.as_os_str().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_the_deepest_mount_boundary() {
        let volumes = vec![
            Volume {
                id: 1,
                mount_path: PathBuf::from("/"),
                filesystem: "apfs".into(),
                internal: true,
            },
            Volume {
                id: 2,
                mount_path: PathBuf::from("/Volumes/Test"),
                filesystem: "apfs".into(),
                internal: false,
            },
        ];
        assert_eq!(
            volume_containing(&volumes, Path::new("/Volumes/Test/report.txt"))
                .unwrap()
                .id,
            2
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn discovers_the_root_internal_volume() {
        let volumes = discover_mounted_volumes().unwrap();
        let root = volume_containing(&volumes, Path::new("/")).unwrap();
        assert!(root.internal);
    }
}
