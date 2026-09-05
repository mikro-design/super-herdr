//! Writing a file fetched off a target onto this machine.
//!
//! The transfer protocol already describes the wire: an offer saying what the
//! file is, chunks pulled a window at a time, and a finish frame. What is here
//! is the receiving end's bookkeeping — where the bytes land, how many are
//! expected, and whether what arrived is what the host attested to.
//!
//! It is a separate module from the frontend that drives it because it is the
//! part worth testing on its own. A download that silently writes a truncated
//! file, or one that leaves a half-written file behind when it fails, is a bug
//! nobody notices until they open the file, and neither failure is visible from
//! a screenshot of a terminal.
//!
//! Two rules shape it:
//!
//! * **Nothing is written where somebody would look for a finished file until
//!   it is finished.** Bytes go to a temporary file beside the destination and
//!   are renamed into place only after the length and the digest both check
//!   out, so an interrupted transfer leaves nothing rather than something that
//!   looks complete.
//! * **The host's digest is checked here**, because this is the end that can.
//!   The daemon carries the attestation without verifying it, exactly as it
//!   carries a client's in the other direction.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// How many chunks the daemon may send before being asked again.
///
/// Flow control exists in this direction because the daemon is the sender and
/// its queue to a client is unbounded. On a desktop the window can be wider
/// than a phone's without meaning much, and it still bounds the queue.
pub const SAVE_WINDOW_CHUNKS: u32 = 8;

#[derive(Debug)]
pub struct FileSave {
    request: u64,
    /// Where the finished file goes. Derived from the host's own name for it
    /// and never from a path a target sent: what a client does with a name is
    /// its own business, and joining a remote path onto a local directory is
    /// how a download escapes the directory it was meant for.
    destination: PathBuf,
    length: u64,
    digest: String,
    received: u64,
    hasher: Sha256,
    file: tempfile::NamedTempFile,
    outstanding: u32,
}

/// What the caller should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveProgress {
    /// Ask for more, and how many.
    Pull(u32),
    /// Bytes are still arriving and the window is not empty yet.
    Waiting,
    /// Finished and verified, at this path.
    Saved(PathBuf),
}

impl FileSave {
    /// Begin a save into `directory`, for a file the host called `name`.
    ///
    /// The name is reduced to its last component before it is used. The
    /// protocol already promises a name rather than a path, and this is the
    /// place where trusting that promise would cost somebody a file written
    /// outside the directory they chose.
    pub fn begin(
        request: u64,
        directory: &Path,
        name: &str,
        length: u64,
        digest: String,
    ) -> Result<Self> {
        let name = Path::new(name)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty() && name != "." && name != "..")
            .context("that file has no name to save it under")?;
        let destination = directory.join(&name);
        if destination.exists() {
            // Refused rather than renamed around. A download that quietly
            // became `report (2).txt` is one somebody opens the old copy of.
            bail!("{} already exists", destination.display());
        }
        std::fs::create_dir_all(directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
        let file = tempfile::Builder::new()
            .prefix(".super-herdr-download-")
            .tempfile_in(directory)
            .with_context(|| {
                format!("failed to open a temporary file in {}", directory.display())
            })?;
        Ok(Self {
            request,
            destination,
            length,
            digest,
            received: 0,
            hasher: Sha256::new(),
            file,
            outstanding: 0,
        })
    }

    pub fn request(&self) -> u64 {
        self.request
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    pub fn length(&self) -> u64 {
        self.length
    }

    /// Open the window. Called once after the offer is accepted.
    pub fn start(&mut self) -> SaveProgress {
        self.outstanding = SAVE_WINDOW_CHUNKS;
        SaveProgress::Pull(SAVE_WINDOW_CHUNKS)
    }

    /// Take one chunk. Refuses more than the offer declared rather than
    /// growing a file past the length its digest was computed over.
    pub fn chunk(&mut self, bytes: &[u8]) -> Result<SaveProgress> {
        let remaining = self.length.saturating_sub(self.received);
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > remaining {
            bail!("that host sent more than it offered");
        }
        self.file
            .write_all(bytes)
            .context("failed to write the downloaded file")?;
        self.hasher.update(bytes);
        self.received += u64::try_from(bytes.len()).unwrap_or_default();
        self.outstanding = self.outstanding.saturating_sub(1);
        if self.received >= self.length || self.outstanding > 0 {
            return Ok(SaveProgress::Waiting);
        }
        self.outstanding = SAVE_WINDOW_CHUNKS;
        Ok(SaveProgress::Pull(SAVE_WINDOW_CHUNKS))
    }

    /// Check what arrived and put it where it belongs.
    ///
    /// Consumes the save either way: a failure here has already discarded the
    /// temporary file, so there is nothing to resume and nothing left behind.
    pub fn finish(self) -> Result<PathBuf> {
        if self.received != self.length {
            bail!(
                "that transfer stopped after {} of {} bytes",
                self.received,
                self.length
            );
        }
        let digest = format!("{:x}", self.hasher.finalize());
        if !self.digest.is_empty() && digest != self.digest {
            bail!("that file did not match the digest its host reported");
        }
        self.file
            .as_file()
            .sync_all()
            .context("failed to flush the downloaded file")?;
        self.file
            .persist_noclobber(&self.destination)
            .map_err(|error| error.error)
            .with_context(|| format!("failed to save {}", self.destination.display()))?;
        Ok(self.destination)
    }
}

/// Where a saved file goes when nobody said.
///
/// The desktop convention, and the current directory only as a last resort. A
/// download that lands somewhere a person has to be told about is one they have
/// to be told about every time.
pub fn default_directory() -> PathBuf {
    if let Some(directory) = std::env::var_os("XDG_DOWNLOAD_DIR") {
        return PathBuf::from(directory);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let downloads = PathBuf::from(&home).join("Downloads");
        if downloads.is_dir() {
            return downloads;
        }
        return PathBuf::from(home);
    }
    PathBuf::from(".")
}

#[cfg(test)]
mod tests {
    use super::{FileSave, SAVE_WINDOW_CHUNKS, SaveProgress};
    use sha2::{Digest, Sha256};

    fn digest_of(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::new().chain_update(bytes).finalize())
    }

    #[test]
    fn a_verified_file_is_renamed_into_place_only_when_it_is_whole() {
        let directory = tempfile::tempdir().unwrap();
        let body = b"line one\nline two\n";
        let mut save = FileSave::begin(
            1,
            directory.path(),
            "report.txt",
            body.len() as u64,
            digest_of(body),
        )
        .unwrap();

        assert_eq!(save.start(), SaveProgress::Pull(SAVE_WINDOW_CHUNKS));
        assert_eq!(save.chunk(body).unwrap(), SaveProgress::Waiting);
        assert!(
            !directory.path().join("report.txt").exists(),
            "nothing sits where a finished file goes until it is finished"
        );

        let saved = save.finish().unwrap();

        assert_eq!(saved, directory.path().join("report.txt"));
        assert_eq!(std::fs::read(&saved).unwrap(), body);
    }

    #[test]
    fn a_file_that_fails_its_digest_leaves_nothing_behind() {
        let directory = tempfile::tempdir().unwrap();
        let body = b"line one\n";
        let mut save = FileSave::begin(
            1,
            directory.path(),
            "report.txt",
            body.len() as u64,
            "f".repeat(64),
        )
        .unwrap();
        save.start();
        save.chunk(body).unwrap();

        let error = save.finish().unwrap_err().to_string();

        assert!(error.contains("digest"), "{error}");
        assert!(!directory.path().join("report.txt").exists());
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            0,
            "a failed save leaves no temporary file either"
        );
    }

    #[test]
    fn a_transfer_that_stops_short_is_reported_rather_than_saved() {
        let directory = tempfile::tempdir().unwrap();
        let body = b"line one\nline two\n";
        let mut save =
            FileSave::begin(1, directory.path(), "report.txt", 64, digest_of(body)).unwrap();
        save.start();
        save.chunk(body).unwrap();

        let error = save.finish().unwrap_err().to_string();

        assert!(error.contains("stopped after"), "{error}");
        assert!(!directory.path().join("report.txt").exists());
    }

    #[test]
    fn a_host_sending_more_than_it_offered_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let mut save =
            FileSave::begin(1, directory.path(), "report.txt", 4, String::new()).unwrap();
        save.start();

        assert!(
            save.chunk(b"far too much").is_err(),
            "a file grown past the length its digest covers is not the file that was offered"
        );
    }

    #[test]
    fn a_name_is_reduced_to_its_last_component() {
        let directory = tempfile::tempdir().unwrap();

        let save = FileSave::begin(1, directory.path(), "../../etc/passwd", 1, String::new());

        assert_eq!(
            save.unwrap().destination(),
            directory.path().join("passwd"),
            "the protocol promises a name; trusting that promise is what this refuses to do"
        );
    }

    #[test]
    fn a_name_that_is_only_a_path_is_refused() {
        let directory = tempfile::tempdir().unwrap();

        assert!(FileSave::begin(1, directory.path(), "..", 1, String::new()).is_err());
        assert!(FileSave::begin(1, directory.path(), "/", 1, String::new()).is_err());
    }

    #[test]
    fn an_existing_file_is_not_quietly_written_around() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("report.txt"), b"old").unwrap();

        let error = FileSave::begin(1, directory.path(), "report.txt", 1, String::new())
            .unwrap_err()
            .to_string();

        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            std::fs::read(directory.path().join("report.txt")).unwrap(),
            b"old",
            "a download that quietly became report (2).txt is one somebody opens the old copy of"
        );
    }

    #[test]
    fn the_window_reopens_only_when_it_has_emptied() {
        let directory = tempfile::tempdir().unwrap();
        let mut save =
            FileSave::begin(1, directory.path(), "report.txt", 8, String::new()).unwrap();
        save.start();

        for _ in 0..SAVE_WINDOW_CHUNKS - 1 {
            assert_eq!(save.chunk(b"").unwrap(), SaveProgress::Waiting);
        }

        assert_eq!(
            save.chunk(b"").unwrap(),
            SaveProgress::Pull(SAVE_WINDOW_CHUNKS),
            "the daemon is asked for more only once it has sent what it was allowed"
        );
    }
}
