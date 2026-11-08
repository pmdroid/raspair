import { useState } from "react";
import {
  ArrowRight,
  Check,
  Circle,
  Disc3,
  HardDrive,
  Radio,
  Speaker,
  Square,
  Volume2,
  Waves,
} from "lucide-react";
import { Artwork, Toggle, Waveform } from "./components";
import { time } from "./model";
import type { Settings, Track } from "./model";

type Props = {
  settings: Settings;
  recording: boolean;
  elapsed: number;
  autoRecord: boolean;
  setAutoRecord: (value: boolean) => void;
  toggleRecord: () => void;
  streaming: boolean;
  setStreaming: (value: boolean) => void;
  speakers: string[];
  toggleSpeaker: (speaker: string) => void;
  tracks: Track[];
  openLibrary: () => void;
  play: (track: Track) => void;
};

export default function Listen(props: Props) {
  const [volume, setVolume] = useState(64);
  const {
    settings,
    recording,
    elapsed,
    autoRecord,
    setAutoRecord,
    toggleRecord,
    streaming,
    setStreaming,
    speakers,
    toggleSpeaker,
    tracks,
    openLibrary,
    play,
  } = props;
  return (
    <>
      <div className="page-heading">
        <div>
          <h1>Your listening room</h1>
          <p>Drop the needle. We’ll keep the beginning.</p>
        </div>
        <span className="status">
          <span className="status-dot" /> Input active
        </span>
      </div>
      <div className="listen-workspace">
        <div className="listening-grid">
          <section className="live-panel panel">
            <div className="section-heading">
              <h2>
                <Waves size={18} /> Live input
              </h2>
              <span className="subtle">
                {settings.rate} · {settings.depth} · Stereo
              </span>
            </div>
            <div className="now-playing">
              <Artwork />
              <div>
                <h3>A little further</h3>
                <p>
                  The Quiet Hours <span>· Sunday sessions</span>
                </p>
              </div>
              <Disc3 className="record-icon" size={28} />
            </div>
            <div className="wave-heading">
              <span>
                <span className="status-dot" /> Audio detected
              </span>
              <span className="subtle">Last {settings.preRoll}</span>
            </div>
            <Waveform />
            <div className="wave-labels">
              <span>−{settings.preRoll.replace(" seconds", "s")}</span>
              <span className="preserved">
                <Check size={13} /> Opening preserved
              </span>
              <span>Now</span>
            </div>
            <div className="signal-route">
              <span>
                <Radio size={16} /> HiFiBerry input
              </span>
              <ArrowRight size={16} />
              <span>
                {streaming && speakers.length ? (
                  <>
                    <Speaker size={16} />{" "}
                    {speakers.length === 1 ? speakers[0] : `${speakers.length} rooms`}
                  </>
                ) : (
                  "Output paused"
                )}
              </span>
              <span className="route-quality">
                {settings.mode === "Buffered" ? "Buffered · 24 bit" : "Realtime · 16 bit"}
              </span>
            </div>
            <div className="playback-controls">
              <button
                className="primary"
                disabled={!speakers.length}
                onClick={() => setStreaming(!streaming)}
              >
                {streaming ? <Square size={14} /> : <Speaker size={16} />}
                {streaming ? "Stop playback" : "Play on speakers"}
              </button>
              <label className="volume">
                <Volume2 size={18} />
                <span className="sr-only">Speaker volume</span>
                <input
                  type="range"
                  min="0"
                  max="100"
                  value={volume}
                  aria-valuetext={`${volume}%`}
                  onChange={(event) => setVolume(Number(event.target.value))}
                />
                <span className="subtle">{volume}%</span>
              </label>
            </div>
          </section>
          <section className={`recording-panel panel ${recording ? "is-recording" : ""}`}>
            <div className="section-heading">
              <h2>Keep this session</h2>
              <Circle size={16} className={recording ? "record-dot" : "subtle"} />
            </div>
            <p>Record the original audio, just as it comes in.</p>
            <div className="record-clock">
              {time(elapsed)}
              <span>{recording ? "Recording on your Pi" : "Ready when you are"}</span>
            </div>
            <button className={recording ? "record-stop" : "primary"} onClick={toggleRecord}>
              {recording ? <Square size={15} /> : <Circle size={15} />}
              {recording ? "Stop recording" : "Record now"}
            </button>
            <div className="record-format">
              <span>Lossless FLAC</span>
              <span>
                {settings.rate} / {settings.depth}
              </span>
            </div>
            <Toggle checked={autoRecord} onChange={setAutoRecord}>
              Record automatically
            </Toggle>
            <div className="storage-note">
              <HardDrive size={15} />
              <span>
                46.8 GB available<span>Saved on your Raspberry Pi</span>
              </span>
            </div>
          </section>
        </div>
        <section className="outputs">
          <div className="section-heading">
            <h2>Where it plays</h2>
            <span className="subtle">
              {settings.protocol} · {settings.mode}
            </span>
          </div>
          <div className="speaker-options">
            {["Living room", "Bedroom", "Kitchen"].map((speaker, index) => (
              <label
                className={`speaker-option ${speakers.includes(speaker) ? "selected" : ""}`}
                key={speaker}
              >
                <span className="speaker-glyph">
                  <Speaker size={25} />
                  {index === 0 && <Speaker size={25} />}
                </span>
                <span>
                  <strong>{speaker}</strong>
                  <small>
                    {index === 0
                      ? "HomePod stereo pair"
                      : index === 1
                        ? "HomePod mini"
                        : "AirPlay speaker"}
                  </small>
                </span>
                <input
                  type="checkbox"
                  aria-label={`Play in ${speaker}`}
                  checked={speakers.includes(speaker)}
                  onChange={() => toggleSpeaker(speaker)}
                />
              </label>
            ))}
          </div>
        </section>
        <section className="recent">
          <div className="section-heading">
            <h2>Recently recorded</h2>
            <button className="text-button" onClick={openLibrary}>
              Open library <ArrowRight size={16} />
            </button>
          </div>
          <div className="recent-list">
            {tracks.slice(0, 3).map((track) => (
              <button className="recent-track" key={track.id} onClick={() => play(track)}>
                <Artwork color={track.color} small />
                <span>
                  <strong>{track.title}</strong>
                  <small>{track.artist}</small>
                </span>
                <span className="track-format">FLAC</span>
                <span className="subtle">{track.date}</span>
              </button>
            ))}
          </div>
        </section>
      </div>
    </>
  );
}
