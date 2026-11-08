import { useEffect, useRef, useState } from "react";
import { Check, Cpu, Disc3, Library, Pause, Play, Radio, Settings2, X } from "lucide-react";
import Listen from "./Listen";
import Recordings from "./Recordings";
import Settings from "./Settings";
import Setup from "./Setup";
import { Artwork } from "./components";
import { initialTracks, loadSettings, time } from "./model";
import type { Page, Settings as DeviceSettings, Track } from "./model";

const navigation = [
  ["Listen", Radio],
  ["Recordings", Library],
  ["Settings", Settings2],
  ["Device setup", Cpu],
] as const;

function currentPage(): Page {
  return (
    navigation.find(
      ([name]) => window.location.hash === `#${name.toLowerCase().replace(" ", "-")}`,
    )?.[0] ?? "Listen"
  );
}

export default function App() {
  const [page, setPage] = useState(currentPage);
  const [settings, setSettings] = useState(loadSettings);
  const [tracks, setTracks] = useState(initialTracks);
  const [recording, setRecording] = useState(false);
  const [elapsed, setElapsed] = useState(0);
  const [autoRecord, setAutoRecord] = useState(false);
  const [streaming, setStreaming] = useState(true);
  const [speakers, setSpeakers] = useState(["Living room"]);
  const [notice, setNotice] = useState("");
  const [selectedTrack, setSelectedTrack] = useState<Track | null>(null);
  const [playing, setPlaying] = useState(false);
  const [position, setPosition] = useState(0);
  const audio = useRef<HTMLAudioElement>(null);
  useEffect(() => {
    const change = () => setPage(currentPage());
    window.addEventListener("hashchange", change);
    return () => window.removeEventListener("hashchange", change);
  }, []);
  useEffect(() => {
    if (!recording) return;
    const timer = window.setInterval(() => setElapsed((value) => value + 1), 1000);
    return () => window.clearInterval(timer);
  }, [recording]);
  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(""), 4000);
    return () => window.clearTimeout(timer);
  }, [notice]);
  function navigate(next: Page) {
    window.location.hash = next.toLowerCase().replace(" ", "-");
    window.scrollTo(0, 0);
  }
  function saveSettings(next: DeviceSettings) {
    try {
      localStorage.setItem("raspair-preview-settings", JSON.stringify(next));
      setSettings(next);
      setNotice("Settings saved");
      return true;
    } catch {
      setNotice("Settings could not be saved. Allow browser storage and try again.");
      return false;
    }
  }
  function toggleRecord() {
    setRecording(!recording);
    if (!recording) setElapsed(0);
    else
      setTracks((current) => [
        {
          ...initialTracks[0],
          id: Date.now(),
          title: "New listening session",
          date: "Just now",
          reviewed: false,
        },
        ...current,
      ]);
    setNotice(recording ? "Session added to Recordings" : "Recording started");
  }
  async function play(track: Track) {
    setSelectedTrack(track);
    setPosition(track.start ?? 0);
    if (!audio.current) return;
    audio.current.currentTime = track.start ?? 0;
    try {
      await audio.current.play();
    } catch {
      setNotice("Audio could not play. Try the play button again.");
    }
  }
  function saveTrack(track: Track) {
    setTracks((current) => current.map((item) => (item.id === track.id ? track : item)));
    if (selectedTrack?.id === track.id) {
      audio.current?.pause();
      if (audio.current) audio.current.currentTime = track.start ?? 0;
      setPosition(track.start ?? 0);
      setSelectedTrack(track);
    }
    setNotice("Song details and boundaries saved");
  }
  return (
    <div className={`app-shell ${selectedTrack ? "with-player" : ""}`}>
      <a className="skip-link" href="#main">
        Skip to content
      </a>
      <aside className="sidebar">
        <a className="brand" href="#listen" aria-label="RaspAir home">
          <Disc3 size={30} strokeWidth={1.7} />
          <span>
            raspair<span className="brand-dot">.</span>
          </span>
        </a>
        <nav aria-label="Main navigation">
          {navigation.map(([name, Icon]) => (
            <button
              key={name}
              className={`nav-item ${page === name ? "active" : ""}`}
              aria-current={page === name ? "page" : undefined}
              aria-label={name}
              title={name}
              onClick={() => navigate(name)}
            >
              <Icon size={19} />
              <span>{name}</span>
            </button>
          ))}
        </nav>
        <div className="sidebar-footer">
          <span className="device-mark">
            <Disc3 size={20} />
          </span>
          <div>
            <strong>Living room Pi</strong>
            <small>
              <span className="status-dot" /> Ready to play
            </small>
          </div>
        </div>
      </aside>
      <div className="main-shell">
        <header className="topbar">
          <span>
            Living room <span className="separator">/</span> {page}
          </span>
          <div>
            <span className="preview-label">Sample device</span>
            <span className="device-name">
              <span className="status-dot" /> Raspberry Pi 4
            </span>
          </div>
        </header>
        <main id="main" tabIndex={-1}>
          {page === "Listen" && (
            <Listen
              settings={settings}
              recording={recording}
              elapsed={elapsed}
              autoRecord={autoRecord}
              setAutoRecord={(value) => {
                setAutoRecord(value);
                setNotice(
                  value
                    ? "Automatic recording armed for the next audio start"
                    : "Automatic recording off",
                );
              }}
              toggleRecord={toggleRecord}
              streaming={streaming}
              setStreaming={setStreaming}
              speakers={speakers}
              toggleSpeaker={(name) =>
                setSpeakers((current) =>
                  current.includes(name)
                    ? current.filter((speaker) => speaker !== name)
                    : [...current, name],
                )
              }
              tracks={tracks}
              openLibrary={() => navigate("Recordings")}
              play={(track) => void play(track)}
            />
          )}
          {page === "Recordings" && (
            <Recordings tracks={tracks} play={(track) => void play(track)} save={saveTrack} />
          )}
          {page === "Settings" && (
            <Settings
              settings={settings}
              recording={recording}
              save={saveSettings}
              setup={() => navigate("Device setup")}
            />
          )}
          {page === "Device setup" &&
            (recording ? (
              <div className="empty-state">
                <Disc3 size={32} />
                <h1>Recording in progress</h1>
                <p>Stop recording before changing the device setup.</p>
                <button onClick={() => navigate("Listen")}>Back to Listen</button>
              </div>
            ) : (
              <Setup
                settings={settings}
                finish={(next) => {
                  if (saveSettings(next)) navigate("Listen");
                }}
              />
            ))}
        </main>
        <footer className="page-footer">
          <span>Your records. Your listening room.</span>
          <span>RaspAir · Web UI preview</span>
        </footer>
      </div>
      <audio
        ref={audio}
        src="./sample.flac"
        preload="metadata"
        onPlay={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
        onError={() => setNotice("The sample audio could not load. Reload to try again.")}
        onTimeUpdate={() => {
          if (!audio.current) return;
          const end = selectedTrack?.end ?? 24;
          setPosition(Math.min(audio.current.currentTime, end));
          if (audio.current.currentTime >= end) audio.current.pause();
        }}
      >
        <track kind="captions" src="./sample.vtt" srcLang="en" label="Instrumental audio" default />
      </audio>
      {selectedTrack && (
        <section className="audio-player" aria-label="Recording player">
          <Artwork color={selectedTrack.color} small />
          <div className="player-title">
            <strong>{selectedTrack.title}</strong>
            <small>{selectedTrack.artist}</small>
          </div>
          <button
            className="primary icon-button"
            aria-label={playing ? "Pause recording" : "Play recording"}
            onClick={() => {
              if (playing) audio.current?.pause();
              else if (position >= (selectedTrack.end ?? 24)) void play(selectedTrack);
              else
                void audio.current
                  ?.play()
                  .catch(() => setNotice("Audio could not play. Try again."));
            }}
          >
            {playing ? <Pause size={18} /> : <Play size={18} />}
          </button>
          <span className="player-time">{time(position - (selectedTrack.start ?? 0))}</span>
          <input
            type="range"
            aria-label="Recording position"
            min={selectedTrack.start ?? 0}
            max={selectedTrack.end ?? 24}
            step="0.1"
            value={position}
            onChange={(event) => {
              const value = Number(event.target.value);
              if (audio.current) audio.current.currentTime = value;
              setPosition(value);
            }}
          />
          <span className="player-time">
            {time((selectedTrack.end ?? 24) - (selectedTrack.start ?? 0))}
          </span>
          <button
            className="icon-button"
            aria-label="Close recording player"
            onClick={() => {
              audio.current?.pause();
              setSelectedTrack(null);
            }}
          >
            <X size={18} />
          </button>
        </section>
      )}
      {notice && (
        <output className="toast">
          <Check size={17} />
          {notice}
        </output>
      )}
    </div>
  );
}
