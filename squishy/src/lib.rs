use std::{
    collections::HashSet,
    fs::File,
    io::{BufReader, BufWriter, Read, Seek, Write},
    path::{Path, PathBuf},
};

use backhand::{kind::Kind, FilesystemReader, InnerNode};
use error::SquishyError;

pub mod error;

pub type Result<T> = std::result::Result<T, SquishyError>;

/// The SquashFS struct provides an interface for reading and interacting with a SquashFS filesystem.
/// It wraps a FilesystemReader, which is responsible for reading the contents of the SquashFS file.
pub struct SquashFS<'a> {
    reader: FilesystemReader<'a>,
}

/// The SquashFSEntry struct represents a single file or directory entry within the SquashFS filesystem.
/// It contains information about the path, size, and type of the entry.
#[derive(Debug)]
pub struct SquashFSEntry {
    pub path: PathBuf,
    pub size: u32,
    pub kind: EntryKind,
}

/// The EntryKind enum represents the different types of entries that can be found in the SquashFS filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink(PathBuf),
    Unknown,
}

impl<'a> SquashFS<'a> {
    /// Creates a new SquashFS instance from a BufReader.
    ///
    /// # Arguments
    /// * `reader` - A BufReader that provides access to the SquashFS data.
    ///
    /// # Returns
    /// A SquashFS instance if the SquashFS data is found and valid, or an error if it is not.
    pub fn new<R>(mut reader: BufReader<R>) -> Result<Self>
    where
        R: Read + Seek + Send + 'a,
    {
        let offset =
            Self::find_squashfs_offset(&mut reader).map_err(|_| SquishyError::NoSquashFsFound)?;
        let reader = FilesystemReader::from_reader_with_offset(reader, offset)
            .map_err(|e| SquishyError::InvalidSquashFS(e.to_string()))?;

        Ok(Self { reader })
    }

    /// Creates a new SquashFS instance from a file path.
    ///
    /// # Arguments
    /// * `path` - The path to the SquashFS file.
    ///
    /// # Returns
    /// A SquashFS instance if the SquashFS data is found and valid, or an error if it is not.
    pub fn from_path<P: AsRef<Path>>(path: &'a P) -> Result<Self> {
        let file = File::open(path).unwrap();
        let reader = BufReader::new(file);
        SquashFS::new(reader)
    }

    /// Finds the starting offset of the SquashFS data within the input file.
    ///
    /// # Arguments
    /// * `file` - The BufReader that provides access to the input file.
    ///
    /// # Returns
    /// The starting offset of the SquashFS data, or an error if the SquashFS data is not found.
    fn find_squashfs_offset<R>(file: &mut BufReader<R>) -> Result<u64>
    where
        R: Read + Seek,
    {
        let mut magic = [0_u8; 4];
        let kind = Kind::from_target("le_v4_0").unwrap();
        while file.read_exact(&mut magic).is_ok() {
            if magic == kind.magic() {
                let found = file.stream_position()? - magic.len() as u64;
                file.rewind()?;
                return Ok(found);
            }
        }
        Err(SquishyError::NoSquashFsFound)
    }

    /// Returns an iterator over all the entries in the SquashFS filesystem.
    pub fn entries(&self) -> impl Iterator<Item = SquashFSEntry> + '_ {
        self.reader.files().map(|node| {
            let size = match &node.inner {
                InnerNode::File(file) => file.basic.file_size,
                _ => 0,
            };

            let kind = match &node.inner {
                InnerNode::File(_) => EntryKind::File,
                InnerNode::Dir(_) => EntryKind::Directory,
                InnerNode::Symlink(symlink) => EntryKind::Symlink(
                    PathBuf::from(format!("/{}", symlink.link.display())).clone(),
                ),
                _ => EntryKind::Unknown,
            };

            SquashFSEntry {
                path: node.fullpath.clone(),
                size,
                kind,
            }
        })
    }

    /// Returns an iterator over all the entries in the SquashFS filesystem
    /// that match the provided predicate function.
    ///
    /// # Arguments
    /// * `predicate` - A function that takes a &Path and returns a bool, indicating whether the entry should be included.
    pub fn find_entries<F>(&self, predicate: F) -> impl Iterator<Item = SquashFSEntry> + '_
    where
        F: Fn(&Path) -> bool + 'a,
    {
        self.entries().filter(move |entry| predicate(&entry.path))
    }

    /// Reads the contents of the specified file from the SquashFS filesystem.
    ///
    /// # Arguments
    /// * `path` - The path to the file within the SquashFS filesystem.
    ///
    /// # Returns
    /// The contents of the file as a Vec<u8>, or an error if the file is not found.
    pub fn read_file<P: AsRef<Path>>(&self, path: P) -> Result<Vec<u8>> {
        let path = path.as_ref();

        for node in self.reader.files() {
            if node.fullpath == path {
                if let InnerNode::File(file) = &node.inner {
                    let mut reader = self.reader.file(&file.basic).reader().bytes();
                    let mut contents = Vec::new();

                    while let Some(Ok(byte)) = reader.next() {
                        contents.push(byte);
                    }

                    return Ok(contents);
                }
            }
        }

        Err(SquishyError::FileNotFound(path.to_path_buf()))
    }

    /// Writes the contents of the specified file from the SquashFS filesystem
    /// to the specified destination path.
    ///
    /// # Arguments
    /// * `source` - The path to the file within the SquashFS filesystem.
    /// * `dest` - The destination path to write the file to.
    ///
    /// # Returns
    /// An empty result, or an error if the file cannot be read or written.
    pub fn write_file<P: AsRef<Path>>(&self, source: P, dest: P) -> Result<()> {
        let contents = self.read_file(source)?;
        let output_file = File::create(dest)?;
        let mut writer = BufWriter::new(output_file);
        writer.write_all(&contents)?;
        Ok(())
    }

    /// Resolves the symlink chain starting from the specified entry,
    /// returning the final target entry or an error if a cycle is detected.
    ///
    /// # Arguments
    /// * `entry` - The entry to resolve the symlink for.
    ///
    /// # Returns
    /// The final target entry, or None if the entry is not a symlink, or an error if a cycle is detected.
    pub fn resolve_symlink(&self, entry: &SquashFSEntry) -> Result<Option<SquashFSEntry>> {
        match &entry.kind {
            EntryKind::Symlink(target) => {
                let mut visited = HashSet::new();
                visited.insert(entry.path.clone());
                self.follow_symlink(target, &mut visited)
            }
            _ => Ok(None),
        }
    }

    /// Recursively follows symlinks, keeping track of the visited paths
    /// to detect and report cycles.
    ///
    /// # Arguments
    /// * `target` - The path to the symlink target.
    /// * `visited` - A mutable HashSet to keep track of visited paths.
    ///
    /// # Returns
    /// The final target entry, or an error if a cycle is detected.
    fn follow_symlink(
        &self,
        target: &Path,
        visited: &mut HashSet<PathBuf>,
    ) -> Result<Option<SquashFSEntry>> {
        if !visited.insert(target.to_path_buf()) {
            return Err(SquishyError::SymlinkError("Cyclic symlink detected".into()));
        }

        let target_path = target.to_path_buf();

        if let Some(target_entry) = self.find_entries(move |p| p == target_path).next() {
            match &target_entry.kind {
                EntryKind::Symlink(next_target) => self.follow_symlink(next_target, visited),
                _ => Ok(Some(target_entry)),
            }
        } else {
            Ok(None)
        }
    }
}
