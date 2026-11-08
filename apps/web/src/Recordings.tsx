import { useState } from "react";
import { Check, Download, FolderOpen, Search, SlidersHorizontal, X } from "lucide-react";
import { Artwork, Waveform } from "./components";
import { time } from "./model";
import type { Track } from "./model";

export default function Recordings({
  tracks,
  play,
  save,
}: {
  tracks: Track[];
  play: (track: Track) => void;
  save: (track: Track) => void;
}) {
  const [query, setQuery] = useState("");
  const [reviewOnly, setReviewOnly] = useState(false);
  const [editing, setEditing] = useState<Track | null>(null);
  const visible = tracks.filter(
    (track) =>
      (!reviewOnly || !track.reviewed) &&
      `${track.title} ${track.artist} ${track.album}`.toLowerCase().includes(query.toLowerCase()),
  );
  return (
    <>
      <div className="page-heading">
        <div>
          <h1>Your record collection</h1>
          <p>Good music, kept close. Every recording lives on your Pi.</p>
        </div>
        <span className="status">
          <FolderOpen size={14} /> {tracks.length} songs
        </span>
      </div>
      <div className="library-toolbar">
        <div className="segments" aria-label="Filter recordings">
          <button aria-pressed={!reviewOnly} onClick={() => setReviewOnly(false)}>
            All songs
          </button>
          <button aria-pressed={reviewOnly} onClick={() => setReviewOnly(true)}>
            Needs review <span>{tracks.filter((track) => !track.reviewed).length}</span>
          </button>
        </div>
        <label className="search">
          <Search size={17} />
          <input
            aria-label="Search recordings"
            placeholder="Find a song or artist"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
          />
        </label>
      </div>
      {editing && (
        <SongEditor
          key={editing.id}
          track={editing}
          close={() => setEditing(null)}
          save={(track) => {
            save(track);
            setEditing(null);
          }}
        />
      )}
      <div className="library-list">
        <div className="library-labels">
          <span>Song / artist</span>
          <span>Album</span>
          <span>Recorded</span>
          <span>Actions</span>
        </div>
        {visible.map((track) => (
          <article className="library-track" key={track.id}>
            <button
              className="song-name"
              onClick={() => play(track)}
              aria-label={`Play ${track.title}`}
            >
              <Artwork color={track.color} small />
              <span>
                <strong>{track.title}</strong>
                <small>{track.artist}</small>
              </span>
            </button>
            <span className="album-name">{track.album}</span>
            <span className="recorded-date">
              {track.date}
              <small>{time((track.end ?? 24) - (track.start ?? 0))} · FLAC</small>
            </span>
            <div className="track-actions">
              <button
                className={`review-button ${track.reviewed ? "" : "needs-review"}`}
                onClick={() => setEditing(track)}
                aria-label={`Review ${track.title}`}
              >
                {track.reviewed ? <SlidersHorizontal size={15} /> : <span className="review-dot" />}
                <span>{track.reviewed ? "Edit" : "Review"}</span>
              </button>
              <a
                className="icon-button"
                href="./sample.flac"
                download={`${track.title}.flac`}
                aria-label={`Download ${track.title}`}
              >
                <Download size={17} />
              </a>
            </div>
          </article>
        ))}
        {!visible.length && (
          <div className="empty-state">
            <Search size={28} />
            <h2>No songs found</h2>
            <p>Try another title or show your full collection.</p>
            <button
              onClick={() => {
                setQuery("");
                setReviewOnly(false);
              }}
            >
              Show all songs
            </button>
          </div>
        )}
      </div>
      <div className="library-note">
        <Check size={15} />
        <span>Original sessions are kept when you adjust song boundaries.</span>
      </div>
    </>
  );
}

function SongEditor({
  track,
  close,
  save,
}: {
  track: Track;
  close: () => void;
  save: (track: Track) => void;
}) {
  const [draft, setDraft] = useState({ ...track, start: track.start ?? 0, end: track.end ?? 24 });
  const [error, setError] = useState("");
  return (
    <form
      className="song-editor panel"
      onSubmit={(event) => {
        event.preventDefault();
        if (
          !Number.isFinite(draft.start) ||
          !Number.isFinite(draft.end) ||
          draft.start < 0 ||
          draft.end > 24 ||
          draft.end <= draft.start
        ) {
          setError("Choose an end after the start, within this 24-second recording.");
          return;
        }
        if (!draft.title.trim()) {
          setError("Add a song title before saving.");
          return;
        }
        save({ ...draft, title: draft.title.trim(), reviewed: true });
      }}
    >
      <div className="section-heading">
        <div>
          <h2>Review this song</h2>
          <p>Fine-tune the boundaries and fill in the sleeve.</p>
        </div>
        <button
          type="button"
          className="icon-button"
          aria-label="Close song review"
          onClick={close}
        >
          <X size={18} />
        </button>
      </div>
      <Waveform start={(draft.start / 24) * 100} end={(draft.end / 24) * 100} />
      <div className="boundary-fields">
        <label className="field">
          <span>Start · seconds</span>
          <input
            type="number"
            step="0.1"
            min="0"
            max="23.9"
            value={draft.start}
            onChange={(event) => setDraft({ ...draft, start: event.target.valueAsNumber })}
            required
          />
        </label>
        <label className="field">
          <span>End · seconds</span>
          <input
            type="number"
            step="0.1"
            min="0.1"
            max="24"
            value={draft.end}
            onChange={(event) => setDraft({ ...draft, end: event.target.valueAsNumber })}
            required
          />
        </label>
        <span className="subtle">Source session · 00:24</span>
      </div>
      <div className="metadata-fields">
        {(["title", "artist", "album"] as const).map((key) => (
          <label className="field" key={key}>
            <span>{key === "title" ? "Song title" : key === "artist" ? "Artist" : "Album"}</span>
            <input
              value={draft[key]}
              onChange={(event) => setDraft({ ...draft, [key]: event.target.value })}
              required={key === "title"}
            />
          </label>
        ))}
      </div>
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      <div className="form-actions">
        <span className="subtle">Review suggested boundaries before saving.</span>
        <button className="primary" type="submit">
          <Check size={16} /> Save song
        </button>
      </div>
    </form>
  );
}
