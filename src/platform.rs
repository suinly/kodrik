//! Handle-based filesystem protections shared by Unix and native Windows.
use std::{fs::File, io, path::Path};

pub(crate) struct OpenOptions {
    inner: std::fs::OpenOptions,
    #[cfg(windows)]
    read: bool,
    #[cfg(windows)]
    write: bool,
    #[cfg(windows)]
    append: bool,
    #[cfg(windows)]
    create: bool,
    #[cfg(windows)]
    create_new: bool,
    #[cfg(windows)]
    truncate: bool,
    #[cfg(unix)]
    flags: i32,
    private: bool,
    nofollow: bool,
}
impl OpenOptions {
    pub(crate) fn new() -> Self {
        Self {
            inner: std::fs::OpenOptions::new(),
            #[cfg(unix)]
            flags: 0,
            private: false,
            nofollow: false,
            #[cfg(windows)]
            read: false,
            #[cfg(windows)]
            write: false,
            #[cfg(windows)]
            append: false,
            #[cfg(windows)]
            create: false,
            #[cfg(windows)]
            create_new: false,
            #[cfg(windows)]
            truncate: false,
        }
    }
    pub(crate) fn read(&mut self, value: bool) -> &mut Self {
        self.inner.read(value);
        #[cfg(windows)]
        {
            self.read = value;
        }
        self
    }
    pub(crate) fn write(&mut self, value: bool) -> &mut Self {
        self.inner.write(value);
        #[cfg(windows)]
        {
            self.write = value;
        }
        self
    }
    pub(crate) fn append(&mut self, value: bool) -> &mut Self {
        self.inner.append(value);
        #[cfg(windows)]
        {
            self.append = value;
        }
        self
    }
    pub(crate) fn create(&mut self, value: bool) -> &mut Self {
        self.inner.create(value);
        #[cfg(windows)]
        {
            self.create = value;
        }
        self
    }
    pub(crate) fn create_new(&mut self, value: bool) -> &mut Self {
        self.inner.create_new(value);
        #[cfg(windows)]
        {
            self.create_new = value;
        }
        self
    }
    pub(crate) fn truncate(&mut self, value: bool) -> &mut Self {
        self.inner.truncate(value);
        #[cfg(windows)]
        {
            self.truncate = value;
        }
        self
    }
    pub(crate) fn mode(&mut self, mode: u32) -> &mut Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            self.inner.mode(mode);
        }
        self.private = mode & 0o077 == 0;
        self
    }
    pub(crate) fn no_follow(&mut self) -> &mut Self {
        self.nofollow = true;
        self
    }
    pub(crate) fn nonblock(&mut self) -> &mut Self {
        self
    }
    pub(crate) fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut options = self.inner.clone();
            options.custom_flags(
                self.flags | libc::O_NONBLOCK | if self.nofollow { libc::O_NOFOLLOW } else { 0 },
            );
            options.open(path)
        }
        #[cfg(windows)]
        {
            win::open(self, path.as_ref())
        }
    }
    #[cfg(unix)]
    pub(crate) fn custom_flags(&mut self, flags: i32) -> &mut Self {
        self.flags = flags;
        self
    }
}
pub(crate) struct DirBuilder {
    recursive: bool,
    mode: u32,
}
impl DirBuilder {
    pub(crate) fn new() -> Self {
        Self {
            recursive: false,
            mode: 0o777,
        }
    }
    pub(crate) fn recursive(&mut self, recursive: bool) -> &mut Self {
        self.recursive = recursive;
        self
    }
    pub(crate) fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }
    pub(crate) fn create(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(self.recursive)
                .mode(self.mode)
                .create(path)
        }
        #[cfg(windows)]
        {
            if path.exists() {
                if !self.recursive {
                    return Err(io::ErrorKind::AlreadyExists.into());
                }
                return set_private(path, self.mode);
            }
            if self.recursive {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    if !parent.exists() {
                        self.create(parent)?;
                    }
                }
            }
            win::create_dir(path, self.mode & 0o077 == 0)
        }
    }
}
pub(crate) fn set_private(path: impl AsRef<Path>, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        let _ = mode;
        let file = win::open_security(path.as_ref(), true)?;
        win::set_private(&file)
    }
}
pub(crate) fn set_file_private(file: &File, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        let _ = mode;
        win::set_private(file)
    }
}
pub(crate) fn is_private(file: &File) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(file.metadata()?.permissions().mode() & 0o077 == 0)
    }
    #[cfg(windows)]
    {
        win::is_private(file)
    }
}
pub(crate) fn path_is_private(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(std::fs::symlink_metadata(path)?.permissions().mode() & 0o077 == 0)
    }
    #[cfg(windows)]
    {
        is_private(&win::open_security(path, false)?)
    }
}
pub(crate) fn is_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.is_symlink()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
}
#[cfg(windows)]
mod win {
    use super::*;
    use std::{
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        },
        ptr::{null, null_mut},
    };
    use windows_sys::Win32::{
        Foundation::*,
        Security::{Authorization::*, *},
        Storage::FileSystem::*,
        System::Threading::*,
    };
    struct Descriptor(PSECURITY_DESCRIPTOR);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    fn descriptor() -> io::Result<Descriptor> {
        unsafe {
            let mut token = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut size = 0;
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut size);
            let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                size,
                &mut size,
            );
            CloseHandle(token);
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let user = &*(buffer.as_ptr().cast::<TOKEN_USER>());
            let mut sid = null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0;
            while *sid.add(len) != 0 {
                len += 1;
            }
            let sid_string = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
            LocalFree(sid.cast());
            let sddl: Vec<u16> = format!("D:P(A;OICI;FA;;;{sid_string})")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut sd = null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut sd,
                null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Descriptor(sd))
        }
    }
    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let p: Vec<u16> = path.as_os_str().encode_wide().collect();
        if p.contains(&0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(p.into_iter().chain(Some(0)).collect())
    }
    fn acl(sd: PSECURITY_DESCRIPTOR) -> io::Result<*mut ACL> {
        unsafe {
            let (mut present, mut defaulted, mut acl) = (0, 0, null_mut());
            if GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted) == 0
                || present == 0
                || acl.is_null()
            {
                return Err(io::Error::other("Missing private DACL"));
            }
            Ok(acl)
        }
    }
    pub(super) fn set_private(file: &File) -> io::Result<()> {
        unsafe {
            let sd = descriptor()?;
            // tempfile/std handles need not carry WRITE_DAC; reopen the same object,
            // never its pathname, so ACL updates cannot be redirected by a rename.
            let handle = ReOpenFile(
                file.as_raw_handle(),
                WRITE_DAC | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_FLAG_BACKUP_SEMANTICS,
            );
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let secured = File::from_raw_handle(handle);
            let status = SetSecurityInfo(
                secured.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                acl(sd.0)?,
                null_mut(),
            );
            if status != 0 {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            Ok(())
        }
    }
    pub(super) fn is_private(file: &File) -> io::Result<bool> {
        unsafe {
            let expected = descriptor()?;
            let mut sd = null_mut();
            let mut actual = null_mut();
            let status = GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut actual,
                null_mut(),
                &mut sd,
            );
            if status != 0 {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            let _guard = Descriptor(sd);
            if actual.is_null() {
                return Ok(false);
            }
            let wanted = acl(expected.0)?;
            Ok(
                std::slice::from_raw_parts(actual.cast::<u8>(), (*actual).AclSize as usize)
                    == std::slice::from_raw_parts(wanted.cast::<u8>(), (*wanted).AclSize as usize),
            )
        }
    }
    fn handle(
        path: &Path,
        access: u32,
        creation: u32,
        private: bool,
        nofollow: bool,
    ) -> io::Result<File> {
        unsafe {
            let path = wide(path)?;
            let sd = if private { Some(descriptor()?) } else { None };
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.as_ref().map_or(null_mut(), |s| s.0),
                bInheritHandle: 0,
            };
            let h = CreateFileW(
                path.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                if private { &sa } else { null() },
                creation,
                FILE_FLAG_BACKUP_SEMANTICS
                    | if nofollow {
                        FILE_FLAG_OPEN_REPARSE_POINT
                    } else {
                        0
                    },
                null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let f = File::from_raw_handle(h);
            if nofollow && is_link(&f.metadata()?) {
                return Err(io::Error::other("Reparse points are forbidden"));
            }
            Ok(f)
        }
    }
    pub(super) fn open_security(path: &Path, write: bool) -> io::Result<File> {
        handle(
            path,
            READ_CONTROL | if write { WRITE_DAC } else { 0 },
            OPEN_EXISTING,
            false,
            true,
        )
    }
    pub(super) fn open(o: &OpenOptions, path: &Path) -> io::Result<File> {
        let access = if o.read { GENERIC_READ } else { 0 }
            | if o.write { GENERIC_WRITE } else { 0 }
            | if o.append { GENERIC_WRITE } else { 0 }
            | READ_CONTROL
            | if o.private { WRITE_DAC } else { 0 };
        // Never truncate before rejecting a reparse point and securing an existing file.
        let creation = if o.create_new {
            CREATE_NEW
        } else if o.create {
            OPEN_ALWAYS
        } else {
            OPEN_EXISTING
        };
        let f = handle(path, access, creation, o.private, o.nofollow)?;
        if o.append {
            use std::io::{Seek, SeekFrom};
            (&f).seek(SeekFrom::End(0))?;
        }
        if o.private {
            set_private(&f)?;
        }
        if o.truncate {
            f.set_len(0)?;
        }
        Ok(f)
    }
    pub(super) fn create_dir(path: &Path, private: bool) -> io::Result<()> {
        unsafe {
            let p = wide(path)?;
            let sd = descriptor()?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: 0,
            };
            if CreateDirectoryW(p.as_ptr(), if private { &sa } else { null() }) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_files_and_directories() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("private");
        DirBuilder::new().mode(0o700).create(&p).unwrap();
        assert!(path_is_private(&p).unwrap());
        let f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .no_follow()
            .open(p.join("secret"))
            .unwrap();
        assert!(is_private(&f).unwrap());
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlinks() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("file"), "safe").unwrap();
        std::os::unix::fs::symlink(t.path().join("file"), t.path().join("link")).unwrap();
        assert!(
            OpenOptions::new()
                .read(true)
                .no_follow()
                .open(t.path().join("link"))
                .is_err()
        );
    }
}

/// Windows has no supported directory-fsync equivalent. File contents are flushed
/// before rename; unlike Unix, directory-entry crash durability is not guaranteed.
pub(crate) fn sync_directory(path: impl AsRef<Path>) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        let _handle = win::open_security(path.as_ref(), false)?;
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn append_handles_flush_and_preserve_existing_records() {
        use std::io::Write;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audit.jsonl");
        for record in [b"first\n".as_slice(), b"second\n".as_slice()] {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .no_follow()
                .open(&path)
                .unwrap();
            file.write_all(record).unwrap();
            file.sync_all().unwrap();
        }
        assert_eq!(std::fs::read(path).unwrap(), b"first\nsecond\n");
    }
    #[test]
    fn rejects_directory_junctions() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let junction = t.path().join("junction");
        let status = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(is_link(&std::fs::symlink_metadata(&junction).unwrap()));
        assert!(
            OpenOptions::new()
                .read(true)
                .no_follow()
                .open(&junction)
                .is_err()
        );
        assert!(set_private(&junction, 0o700).is_err());
        std::fs::remove_dir(junction).unwrap();
    }
    #[test]
    fn acl_is_private_at_creation_and_can_be_restored() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("secret");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        assert!(is_private(&file).unwrap());
        set_file_private(&file, 0o600).unwrap();
        assert!(is_private(&file).unwrap());
    }
}
