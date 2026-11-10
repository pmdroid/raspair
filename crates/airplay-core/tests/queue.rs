use airplay_core::audio::{scan, Queue, QueueError, ScanError, ScanOptions, SkipReason};
use airplay_core::Cancellation;
use std::fs;
use std::path::{Path, PathBuf};

const FLAC: &str = "pcm24_48k_stereo.flac";
const MP3: &str = "tone_44k_stereo.mp3";

fn put(dir: &Path, rel: impl AsRef<Path>, fixture: &str) -> PathBuf {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture), &path).unwrap();
    path
}

fn rel(root: &Path, paths: impl IntoIterator<Item = PathBuf>) -> Vec<String> {
    paths.into_iter().map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned()).collect()
}

fn library() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for name in ["Track 10.flac", "track 2.flac", "Track 1.FLAC", "b/01.flac", "a/01.flac"] {
        put(dir.path(), name, FLAC);
    }
    put(dir.path(), "a/02.mp3", MP3);
    fs::write(dir.path().join("broken.flac"), b"not audio").unwrap();
    fs::write(dir.path().join("notes.txt"), b"liner notes").unwrap();
    dir
}

#[test]
fn folders_use_natural_order_and_recursion_is_opt_in() {
    let dir = library();
    let root = dir.path().to_path_buf();
    let flat = scan(std::slice::from_ref(&root), &ScanOptions::default(), &Cancellation::default()).unwrap();
    assert_eq!(rel(&root, flat.tracks.iter().map(|t| t.path.clone())), ["Track 1.FLAC", "track 2.flac", "Track 10.flac"]);
    let deep = scan(std::slice::from_ref(&root), &ScanOptions { recursive: true, ..Default::default() }, &Cancellation::default()).unwrap();
    assert_eq!(
        rel(&root, deep.tracks.iter().map(|t| t.path.clone())),
        ["a/01.flac", "a/02.mp3", "b/01.flac", "Track 1.FLAC", "track 2.flac", "Track 10.flac"]
    );
    let titles: Vec<_> = deep.tracks.iter().map(|t| t.metadata.title.as_str()).collect();
    assert_eq!(titles, ["01", "Fixture Tone", "01", "Track 1", "track 2", "Track 10"]);
    assert_eq!(deep.tracks[1].metadata.artist.as_deref(), Some("raspair"));
    assert_eq!(deep.tracks[0].metadata.duration, Some(std::time::Duration::from_millis(100)));
}

#[test]
fn invalid_audio_is_reported_and_never_queued() {
    let dir = library();
    let broken = dir.path().join("broken.flac");
    let report = scan(&[dir.path().to_path_buf()], &ScanOptions::default(), &Cancellation::default()).unwrap();
    assert!(report.tracks.iter().all(|t| t.path != broken));
    assert!(matches!(&report.skipped[..], [s] if s.path == broken && matches!(s.reason, SkipReason::Unsupported(_))));
    let missing = dir.path().join("missing.flac");
    let report = scan(&[missing.clone(), dir.path().join("notes.txt")], &ScanOptions::default(), &Cancellation::default()).unwrap();
    assert!(report.tracks.is_empty());
    assert!(matches!(&report.skipped[0], s if s.path == missing && matches!(s.reason, SkipReason::Unreadable(_))));
    assert!(matches!(report.skipped[1].reason, SkipReason::Unsupported(_)));
}

#[test]
fn explicit_files_keep_argument_order_and_duplicates() {
    let dir = library();
    let (a, b) = (dir.path().join("a/01.flac"), dir.path().join("b/01.flac"));
    let paths = [b.clone(), a.clone(), b.clone()];
    let report = scan(&paths, &ScanOptions::default(), &Cancellation::default()).unwrap();
    assert_eq!(report.tracks.iter().map(|t| t.path.clone()).collect::<Vec<_>>(), paths);
    let mut queue = Queue::new();
    let ids = queue.append(report.tracks).unwrap();
    assert!(ids[0] != ids[2] && queue.get(ids[0]).unwrap().metadata == queue.get(ids[2]).unwrap().metadata);
}

#[cfg(unix)]
#[test]
fn symlink_loops_are_not_followed_and_non_utf8_paths_survive() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let odd = std::ffi::OsStr::from_bytes(b"caf\xe9.flac");
    let odd = put(dir.path(), odd, FLAC);
    std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
    std::os::unix::fs::symlink("self.flac", dir.path().join("self.flac")).unwrap();
    let options = ScanOptions { recursive: true, ..Default::default() };
    let report = scan(&[dir.path().to_path_buf()], &options, &Cancellation::default()).unwrap();
    assert_eq!(report.tracks.len(), 1);
    assert_eq!(report.tracks[0].path, odd);
    assert_eq!(report.tracks[0].metadata.title, "caf\u{fffd}");
    assert!(matches!(&report.skipped[..], [s] if s.path.ends_with("self.flac") && matches!(s.reason, SkipReason::Unreadable(_))));
}

#[test]
fn cancelled_and_over_limit_scans_leave_the_queue_intact() {
    let dir = library();
    let mut queue = Queue::new();
    queue.append(scan(&[dir.path().join("a")], &ScanOptions::default(), &Cancellation::default()).unwrap().tracks).unwrap();
    let before = queue.entries().to_vec();
    let cancel = Cancellation::default();
    cancel.cancel();
    let root = [dir.path().to_path_buf()];
    let attempts = [
        scan(&root, &ScanOptions::default(), &cancel),
        scan(&root, &ScanOptions { recursive: true, max_entries: 3 }, &Cancellation::default()),
    ];
    assert_eq!(attempts[0], Err(ScanError::Cancelled));
    assert_eq!(attempts[1], Err(ScanError::TooManyEntries { limit: 3 }));
    for report in attempts.into_iter().flatten() {
        queue.append(report.tracks).unwrap();
    }
    assert_eq!(queue.entries(), &before[..]);
}

#[test]
fn edits_keep_ids_and_playback_identity() {
    let dir = library();
    let options = ScanOptions { recursive: true, ..Default::default() };
    let tracks = scan(&[dir.path().to_path_buf()], &options, &Cancellation::default()).unwrap().tracks;
    let mut queue = Queue::new();
    let ids = queue.append(tracks[..4].to_vec()).unwrap();
    assert_eq!(queue.retreat(), None);
    assert_eq!(queue.advance().unwrap().id, ids[0]);
    assert_eq!(queue.advance().unwrap().id, ids[1]);
    queue.move_to(ids[1], 3).unwrap();
    assert_eq!(queue.current().unwrap().id, ids[1]);
    assert_eq!(queue.entries().iter().map(|e| e.id).collect::<Vec<_>>(), [ids[0], ids[2], ids[3], ids[1]]);
    assert_eq!(queue.peek_previous(), Some(ids[3]));
    assert_eq!(queue.peek_next(), None);
    queue.move_to(ids[1], 1).unwrap();
    queue.remove(ids[1]).unwrap();
    assert_eq!(queue.current(), None);
    assert_eq!((queue.peek_previous(), queue.peek_next()), (Some(ids[0]), Some(ids[2])));
    queue.remove(ids[2]);
    assert_eq!(queue.advance().unwrap().id, ids[3]);
    queue.remove(ids[3]);
    assert_eq!(queue.peek_next(), None);
    let more = queue.append(tracks[4..].to_vec()).unwrap();
    assert!(more.iter().all(|id| !ids.contains(id)));
    assert_eq!(queue.advance().unwrap().id, more[0]);
    assert_eq!(queue.move_to(ids[1], 0), Err(QueueError::UnknownEntry(ids[1])));
    queue.clear();
    assert!(queue.is_empty() && queue.advance().is_none());
    let again = queue.append(tracks[..1].to_vec()).unwrap();
    assert!(again[0] > more[1]);
    assert_eq!(queue.advance().unwrap().id, again[0]);
}
