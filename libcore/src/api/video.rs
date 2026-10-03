//! A video preparation owns private candidates until staging accepts one of them.
//! The host probes media and runs its asynchronous codec, but never chooses policy
//! or deletes files after the constructor has accepted ownership.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::media::video::{self, VideoEncodingPlan, VideoProbe};
use crate::platform::CoreError;

#[derive(Clone, Debug, uniffi::Record)]
pub struct VideoSelection {
    pub path: String,
    pub name: String,
    pub mime: String,
    pub poster_edge: u32,
}

struct Selected {
    media: VideoSelection,
    size: u64,
}

/// These are individually owned paths, never a cache directory or shared
/// content-addressed transfer. Cleanup deliberately does not recurse.
struct Candidates {
    source: Option<PathBuf>,
    output: Option<PathBuf>,
}

impl Candidates {
    fn release(&mut self, selected: &Path) {
        if self.source.as_deref() == Some(selected) {
            self.source = None;
        }
        if self.output.as_deref() == Some(selected) {
            self.output = None;
        }
    }
}

impl Drop for Candidates {
    fn drop(&mut self) {
        for path in [&self.source, &self.output].into_iter().flatten() {
            if let Err(error) = std::fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("VIDEO: private candidate cleanup failed ({:?})", error.kind());
            }
        }
    }
}

enum State {
    Owned { candidates: Candidates, selected: Option<Selected> },
    Staged,
    Cancelled,
}

#[derive(uniffi::Object)]
pub struct VideoPreparation {
    source: String,
    output: String,
    name: String,
    mime: String,
    input: VideoProbe,
    source_size: u64,
    encoding: Option<VideoEncodingPlan>,
    state: Mutex<State>,
}

#[uniffi::export]
impl VideoPreparation {
    /// The caller supplies an independent, closed private file. A returned
    /// error leaves it with the caller; successful construction owns it.
    #[uniffi::constructor]
    pub fn new(
        source_path: String, name: String, mime: String, input: VideoProbe,
    ) -> Result<Arc<Self>, CoreError> {
        let source = PathBuf::from(&source_path);
        let source_size = file_size(&source)?;
        let source = std::fs::canonicalize(source)
            .map_err(|_| refused("The selected video is unavailable."))?;
        let source_path = source
            .to_str()
            .ok_or_else(|| refused("The video's file path is not supported."))?
            .to_owned();
        let encoding = video::plan(&input, source_size).map_err(|error| {
            log::warn!("VIDEO: input rejected: {error}; size={source_size}, display={}x{}, duration_us={}, fps={:?}, hdr={}, depth={:?}",
                input.display_width, input.display_height, input.duration_us, input.frame_rate,
                input.hdr, input.bit_depth);
            refused(error.to_string())
        })?;
        let output = allocate_output(
            source.parent().ok_or_else(|| refused("The video output could not be created."))?,
        )?;
        let output_path =
            output.to_str().expect("UTF-8 source parent and generated name").to_owned();

        // No fallible setup follows adoption of the input. Dropping this object
        // subsequently cleans up candidates even if the host abandons its UI.
        Ok(Arc::new(Self {
            source: source_path,
            output: output_path,
            name,
            mime,
            input,
            source_size,
            encoding,
            state: Mutex::new(State::Owned {
                candidates: Candidates { source: Some(source), output: Some(output) },
                selected: None,
            }),
        }))
    }

    pub fn plan(&self) -> Option<VideoEncodingPlan> {
        self.encoding.clone()
    }

    pub fn source_path(&self) -> String {
        self.source.clone()
    }

    pub fn output_path(&self) -> String {
        self.output.clone()
    }

    /// Called only once the codec/muxer has released its files. Probe the exact
    /// designated output; a missing probe is valid only for a passthrough plan.
    pub fn select(&self, output: Option<VideoProbe>) -> Result<VideoSelection, CoreError> {
        let mut state = self.state.lock();
        let State::Owned { selected, .. } = &mut *state else {
            return Err(closed(&state));
        };
        if let Some(selected) = selected {
            return Ok(selected.media.clone());
        }
        let source_size = file_size(Path::new(&self.source))?;
        if source_size != self.source_size {
            return Err(refused("The selected video changed. Please select it again."));
        }
        let use_output = if let Some(plan) = &self.encoding {
            let output_probe =
                output.ok_or_else(|| refused("The compressed video's details are missing."))?;
            let output_size = file_size(Path::new(&self.output))?;
            video::validate_output(&self.input, &output_probe, plan).map_err(|error| {
                log::warn!("VIDEO: output rejected: {error}; input={}x{}, duration_us={}, fps={:?}, hdr={}, depth={:?}; output={}x{}, duration_us={}, fps={:?}, hdr={}, depth={:?}; target={}x{}",
                    self.input.display_width, self.input.display_height, self.input.duration_us,
                    self.input.frame_rate, self.input.hdr, self.input.bit_depth,
                    output_probe.display_width, output_probe.display_height, output_probe.duration_us,
                    output_probe.frame_rate, output_probe.hdr, output_probe.bit_depth, plan.width, plan.height);
                refused(error.to_string())
            })?;
            (output_size < source_size).then_some(output_size)
        } else {
            if output.is_some() {
                return Err(refused("This video does not need an encoded output."));
            }
            None
        };
        let (path, name, mime, size) = if let Some(size) = use_output {
            let stem = self.name.rsplit_once('.').map_or(self.name.as_str(), |(stem, _)| stem);
            let stem = if stem.trim().is_empty() { "video" } else { stem };
            (self.output.clone(), format!("{stem}.mp4"), "video/mp4".to_owned(), size)
        } else {
            (self.source.clone(), self.name.clone(), self.mime.clone(), source_size)
        };
        let media = VideoSelection { path, name, mime, poster_edge: crate::media::POSTER_EDGE };
        *selected = Some(Selected { media: media.clone(), size });
        Ok(media)
    }

    /// An accepted id transfers the selected file to core staging. From that
    /// instant cancel/drop may release only the other private candidate.
    pub fn stage(
        &self, poster_rgba: Option<Vec<u8>>, width: u32, height: u32,
    ) -> Result<u64, CoreError> {
        self.stage_with(|selected| {
            crate::staging::stage_attachment(
                selected.path.clone(),
                selected.name.clone(),
                selected.mime.clone(),
                poster_rgba,
                width,
                height,
            )
            .map_err(|_| refused("The video could not be added. Please try again."))
        })
    }

    /// The host must wait for codec cancellation/release before calling this.
    /// It is safe after staging and never discards an accepted staging id.
    pub fn cancel(&self) {
        let previous = std::mem::replace(&mut *self.state.lock(), State::Cancelled);
        drop(previous);
    }
}

impl VideoPreparation {
    fn stage_with(
        &self, stage: impl FnOnce(&VideoSelection) -> Result<u64, CoreError>,
    ) -> Result<u64, CoreError> {
        let mut state = self.state.lock();
        let State::Owned { candidates, selected } = &mut *state else {
            return Err(closed(&state));
        };
        let selected = selected
            .as_ref()
            .ok_or_else(|| refused("Choose the prepared video before adding it."))?;
        if file_size(Path::new(&selected.media.path))? != selected.size {
            return Err(refused("The prepared video changed. Please select it again."));
        }
        let id = stage(&selected.media)?;
        candidates.release(Path::new(&selected.media.path));
        let previous = std::mem::replace(&mut *state, State::Staged);
        drop(state);
        drop(previous);
        Ok(id)
    }
}

fn refused(message: impl Into<String>) -> CoreError {
    CoreError::Refused { msg: message.into() }
}

fn closed(state: &State) -> CoreError {
    refused(match state {
        State::Staged => "This video has already been added.",
        _ => "Video preparation was cancelled.",
    })
}

fn file_size(path: &Path) -> Result<u64, CoreError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| refused("The video file is unavailable."))?;
    if !metadata.file_type().is_file() || metadata.len() == 0 {
        return Err(refused("The video file is empty or unavailable."));
    }
    Ok(metadata.len())
}

fn allocate_output(parent: &Path) -> Result<PathBuf, CoreError> {
    for _ in 0..4 {
        let path = parent.join(format!(".video-{}.mp4", uuid::Uuid::now_v7()));
        // Reserve a unique regular file without touching any existing entry.
        // The host muxer opens this empty file with truncation after this handle
        // closes. A sibling path needs no delayed directory cleanup when the
        // staging worker moves the accepted file to content-addressed storage.
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(refused("The video output could not be created.")),
        }
    }
    Err(refused("The video output could not be created."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn probe() -> VideoProbe {
        VideoProbe {
            container_mime: Some("video/quicktime".into()),
            video_mime: "video/avc".into(),
            video_tracks: 1,
            audio_tracks: 0,
            display_width: 1920.0,
            display_height: 1080.0,
            duration_us: 10_000_000,
            frame_rate: Some(30.0),
            video_bitrate: Some(8_000_000),
            audio: None,
            hdr: false,
            dynamic_hdr: false,
            color_primaries: Some(1),
            color_transfer: Some(1),
            full_range: Some(false),
            bit_depth: Some(8),
            hdr_static_metadata: None,
        }
    }

    fn prepare(directory: &Path) -> Arc<VideoPreparation> {
        let source = directory.join("private-import.mov");
        std::fs::write(&source, vec![0x55; 1024]).unwrap();
        VideoPreparation::new(
            source.to_str().unwrap().into(),
            "camera.mov".into(),
            "video/quicktime".into(),
            probe(),
        )
        .unwrap()
    }

    fn output_probe(preparation: &VideoPreparation) -> VideoProbe {
        let plan = preparation.plan().unwrap();
        let mut result = probe();
        result.container_mime = Some("video/mp4".into());
        result.video_mime = plan.video_mime;
        result.display_width = plan.width.into();
        result.display_height = plan.height.into();
        result.video_bitrate = Some(plan.video_bitrate);
        result
    }

    #[test]
    fn constructor_refusal_leaves_the_import_with_its_caller() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("private-import.mov");
        let sibling = directory.path().join("authoritative-attachment");
        std::fs::write(&source, [1, 2, 3]).unwrap();
        std::fs::write(&sibling, [4, 5, 6]).unwrap();
        let mut invalid = probe();
        invalid.duration_us = 0;
        assert!(
            VideoPreparation::new(
                source.to_str().unwrap().into(),
                "camera.mov".into(),
                "video/quicktime".into(),
                invalid,
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&source).unwrap(), [1, 2, 3]);
        assert_eq!(std::fs::read(&sibling).unwrap(), [4, 5, 6]);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn validation_and_handoff_failure_keep_candidates_until_explicit_cancel() {
        let directory = tempfile::tempdir().unwrap();
        let sibling = directory.path().join("authoritative-attachment");
        std::fs::write(&sibling, [4, 5, 6]).unwrap();
        let preparation = prepare(directory.path());
        let source = PathBuf::from(preparation.source_path());
        let output = PathBuf::from(preparation.output_path());
        std::fs::write(&output, [7; 128]).unwrap();
        let mut invalid = output_probe(&preparation);
        invalid.duration_us /= 2;
        assert!(preparation.select(Some(invalid)).is_err());
        assert!(source.exists() && output.exists());

        preparation.select(Some(output_probe(&preparation))).unwrap();
        assert!(preparation.stage_with(|_| Err(refused("staging unavailable"))).is_err());
        assert!(source.exists() && output.exists());

        // The codec is required to close/freeze output before selection. If it
        // still changes, reject the handoff without giving staging that file.
        std::fs::write(&output, [7; 129]).unwrap();
        assert!(preparation.stage_with(|_| panic!("changed file reached staging")).is_err());
        assert!(source.exists() && output.exists());

        preparation.cancel();
        preparation.cancel();
        assert!(!source.exists() && !output.exists());
        assert_eq!(std::fs::read(&sibling).unwrap(), [4, 5, 6]);
        assert!(
            preparation.stage_with(|_| panic!("cancelled preparation reached staging")).is_err()
        );
        assert!(preparation.select(None).is_err());
    }

    #[test]
    fn larger_output_never_replaces_original_and_accepted_source_survives_drop() {
        let directory = tempfile::tempdir().unwrap();
        let preparation = prepare(directory.path());
        let source = PathBuf::from(preparation.source_path());
        let output = PathBuf::from(preparation.output_path());
        std::fs::write(&output, [7; 1025]).unwrap();
        let selected = preparation.select(Some(output_probe(&preparation))).unwrap();
        assert_eq!(Path::new(&selected.path), source);
        assert_eq!(selected.name, "camera.mov");
        assert_eq!(selected.mime, "video/quicktime");

        // Acceptance starts an asynchronous hash; the selected source has not
        // moved yet and must survive both cleanup and the object's final drop.
        assert_eq!(preparation.stage_with(|_| Ok(19)).unwrap(), 19);
        assert!(source.exists());
        assert!(!output.exists());
        drop(preparation);
        assert!(source.exists());
    }

    #[test]
    fn acceptance_serializes_with_cancel_and_preserves_the_hash_input() {
        let directory = tempfile::tempdir().unwrap();
        let preparation = prepare(directory.path());
        let source = PathBuf::from(preparation.source_path());
        let output = PathBuf::from(preparation.output_path());
        std::fs::write(&output, [7; 128]).unwrap();
        let selected = preparation.select(Some(output_probe(&preparation))).unwrap();
        assert_eq!(Path::new(&selected.path), output);

        let (entered_tx, entered_rx) = mpsc::channel();
        let (accept_tx, accept_rx) = mpsc::channel();
        let staging = preparation.clone();
        let stage_thread = std::thread::spawn(move || {
            staging.stage_with(|_| {
                entered_tx.send(()).unwrap();
                accept_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(23)
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(preparation.state.try_lock().is_none());

        let (cancelling_tx, cancelling_rx) = mpsc::channel();
        let cancelling = preparation.clone();
        let cancel_thread = std::thread::spawn(move || {
            cancelling_tx.send(()).unwrap();
            cancelling.cancel();
        });
        cancelling_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(source.exists() && output.exists());
        accept_tx.send(()).unwrap();
        assert_eq!(stage_thread.join().unwrap().unwrap(), 23);
        cancel_thread.join().unwrap();

        assert!(!source.exists());
        assert!(output.exists());
        assert!(preparation.stage_with(|_| panic!("duplicate handoff")).is_err());
        drop(preparation);
        assert_eq!(std::fs::read(&output).unwrap(), [7; 128]);
    }

    #[test]
    fn abandoning_an_unstaged_selection_cleans_only_its_private_files() {
        let directory = tempfile::tempdir().unwrap();
        let sibling = directory.path().join("authoritative-attachment");
        std::fs::write(&sibling, [4, 5, 6]).unwrap();
        let preparation = prepare(directory.path());
        let source = PathBuf::from(preparation.source_path());
        let output = PathBuf::from(preparation.output_path());
        std::fs::write(&output, [7; 128]).unwrap();
        preparation.select(Some(output_probe(&preparation))).unwrap();
        drop(preparation);
        assert!(!source.exists() && !output.exists());
        assert_eq!(std::fs::read(&sibling).unwrap(), [4, 5, 6]);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
