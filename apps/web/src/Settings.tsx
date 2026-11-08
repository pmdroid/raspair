import { useState } from "react";
import { Check, Cpu, HardDrive, Radio, RotateCw, Waves, Wifi } from "lucide-react";
import { Field } from "./components";
import type { Settings as DeviceSettings } from "./model";

export const options = {
  board: ["HiFiBerry DAC2 ADC Pro", "HiFiBerry DAC+ ADC", "USB audio interface"],
  input: ["HiFiBerry stereo input", "USB stereo input"],
  rate: ["44.1 kHz", "48 kHz", "96 kHz"],
  depth: ["16 bit", "24 bit"],
  gain: ["−6 dB", "0 dB", "+6 dB", "+12 dB"],
  compression: ["0 · Fastest", "5 · Balanced", "8 · Smallest files"],
  storage: ["SD card · 46.8 GB free", "USB drive · 218 GB free"],
  mode: ["Auto", "Realtime", "Buffered"],
  quality: ["128 kbps MP3", "192 kbps MP3", "320 kbps MP3", "Original FLAC"],
  detection: ["Adaptive", "Manual threshold"],
  preRoll: ["5 seconds", "10 seconds", "20 seconds"],
  wifi: ["Listening room", "Home network"],
  protocol: ["AirPlay", "HTTP audio"],
};

export default function Settings({
  settings,
  recording,
  save,
  setup,
}: {
  settings: DeviceSettings;
  recording: boolean;
  save: (settings: DeviceSettings) => boolean;
  setup: () => void;
}) {
  const [tab, setTab] = useState("Audio input");
  const [draft, setDraft] = useState(settings);
  const [error, setError] = useState("");
  const [pendingRestart, setPendingRestart] = useState(false);
  const [restarting, setRestarting] = useState(false);
  const dirty = JSON.stringify(draft) !== JSON.stringify(settings);
  const tabs = [
    ["Audio input", Waves],
    ["Recording", HardDrive],
    ["Streaming", Radio],
    ["Device", Cpu],
  ] as const;
  function field(key: keyof DeviceSettings, label: string, hint?: string) {
    return (
      <Field
        key={key}
        label={label}
        value={draft[key]}
        options={options[key]}
        hint={hint}
        onChange={(value) => setDraft({ ...draft, [key]: value })}
      />
    );
  }
  return (
    <>
      <div className="page-heading">
        <div>
          <h1>Make it yours</h1>
          <p>Your input, your speakers, your way of listening.</p>
        </div>
        <button onClick={setup}>
          <Cpu size={16} /> Device setup
        </button>
      </div>
      <div className="settings-layout">
        <div className="settings-tabs" aria-label="Settings categories">
          {tabs.map(([name, Icon]) => (
            <button key={name} aria-pressed={tab === name} onClick={() => setTab(name)}>
              <Icon size={17} />
              {name}
            </button>
          ))}
        </div>
        <form
          className="settings-form panel"
          onSubmit={(event) => {
            event.preventDefault();
            if (recording) {
              setError(
                "Stop recording before applying device settings. Your changes are still here.",
              );
              return;
            }
            if (save(draft)) {
              setPendingRestart(draft.board !== settings.board);
              setError("");
            }
          }}
        >
          <div className="settings-section-heading">
            <h2>{tab}</h2>
            <p>
              {tab === "Audio input"
                ? "Choose the source and the detail you keep."
                : tab === "Recording"
                  ? "Keep the original audio and protect quiet openings."
                  : tab === "Streaming"
                    ? "Choose how audio reaches your speakers and phone."
                    : "Manage your Raspberry Pi and its connection."}
            </p>
          </div>
          <div className="settings-fields">
            {tab === "Audio input" && (
              <>
                {field("board", "Audio board", "Changing the board requires a device restart.")}
                {field("input", "Input device")}
                {field("rate", "Sample rate")}
                {field("depth", "Bit depth")}
                {field("gain", "Input gain", "Leave headroom for the loudest part of a record.")}
              </>
            )}
            {tab === "Recording" && (
              <>
                {field("storage", "Save recordings to")}
                {field(
                  "compression",
                  "FLAC compression",
                  "Every level is lossless. Higher levels use more processing.",
                )}
                {field(
                  "detection",
                  "Audio detection",
                  "Adaptive detection follows the input noise floor.",
                )}
                {field(
                  "preRoll",
                  "Keep audio before detection",
                  "The buffer preserves the opening while speakers connect.",
                )}
              </>
            )}
            {tab === "Streaming" && (
              <>
                {field("protocol", "Streaming protocol")}
                {field(
                  "mode",
                  "AirPlay mode",
                  "Auto chooses a mode supported by the selected speaker.",
                )}
                {field(
                  "quality",
                  "HTTP streaming quality",
                  "Independent of the original FLAC recording.",
                )}
                <div className="setting-explanation">
                  <Radio size={20} />
                  <div>
                    <strong>Selected speaker · Living room</strong>
                    <p>
                      {draft.mode === "Buffered"
                        ? "Buffered AirPlay · 44.1 kHz / 24 bit"
                        : "Realtime AirPlay · 44.1 kHz / 16 bit"}
                    </p>
                    <small>Each speaker's supported format determines the stream.</small>
                  </div>
                </div>
              </>
            )}
            {tab === "Device" && (
              <>
                <div className="device-details">
                  <Cpu size={32} />
                  <div>
                    <strong>Living room Pi</strong>
                    <p>Raspberry Pi 4 · Raspberry Pi OS 64-bit</p>
                    <span className="subtle">raspair.local · 192.168.1.42</span>
                  </div>
                </div>
                {field("wifi", "Wi-Fi network")}
                <div className="connection-note">
                  <Wifi size={16} /> Connected · Strong signal
                </div>
              </>
            )}
          </div>
          {error && (
            <p role="alert" className="form-error">
              {error}
            </p>
          )}
          {pendingRestart && (
            <div className="restart-note">
              <span>Audio board saved. Restart to apply it.</span>
              <button
                type="button"
                disabled={restarting}
                onClick={async () => {
                  setRestarting(true);
                  await new Promise((resolve) => setTimeout(resolve, 900));
                  setRestarting(false);
                  setPendingRestart(false);
                }}
              >
                <RotateCw size={15} />
                {restarting ? "Restarting…" : "Restart sample device"}
              </button>
            </div>
          )}
          <div className="form-actions">
            <span className="subtle">
              {dirty ? "You have unsaved changes" : "All changes saved"}
            </span>
            <button type="submit" className="primary" disabled={!dirty}>
              <Check size={16} /> Apply changes
            </button>
          </div>
        </form>
      </div>
    </>
  );
}
