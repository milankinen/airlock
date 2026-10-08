//! A builder of OCI layer tarballs for tests.

use std::io::Write;
use std::path::Path;

use flate2::Compression;
use flate2::write::GzEncoder;

/// An OCI layer tarball that is built in memory, one entry at a time, in
/// order.
pub struct LayerTar(tar::Builder<Vec<u8>>);

impl Default for LayerTar {
    fn default() -> Self {
        Self(tar::Builder::new(Vec::new()))
    }
}

impl LayerTar {
    /// A regular file `path` (mode 0644) with `content`.
    #[must_use]
    pub fn file(mut self, path: &str, content: impl AsRef<[u8]>) -> Self {
        let content = content.as_ref();
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        self.0.append_data(&mut header, path, content).unwrap();
        self
    }

    /// A directory `path`.
    #[must_use]
    pub fn dir(mut self, path: &str) -> Self {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        self.0
            .append_data(&mut header, path, std::io::empty())
            .unwrap();
        self
    }

    /// A symlink `path` to `target`.
    #[must_use]
    pub fn symlink(mut self, path: &str, target: impl AsRef<Path>) -> Self {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        self.0.append_link(&mut header, path, target).unwrap();
        self
    }

    /// An empty regular file with the exact name `raw_path`. Use it for
    /// names that a correct builder refuses, for example absolute paths.
    #[must_use]
    pub fn raw_file(mut self, raw_path: &str) -> Self {
        let mut header = tar::Header::new_old();
        header.as_old_mut().name[..raw_path.len()].copy_from_slice(raw_path.as_bytes());
        header.set_size(0);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        self.0.append(&header, std::io::empty()).unwrap();
        self
    }

    /// The uncompressed tarball.
    pub fn plain(self) -> Vec<u8> {
        self.0.into_inner().unwrap()
    }

    /// The gzip-compressed tarball.
    pub fn gz(self) -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(&self.plain()).unwrap();
        gz.finish().unwrap()
    }
}
