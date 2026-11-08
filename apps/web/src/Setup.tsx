import { useState } from "react";
import { ArrowLeft, ArrowRight, Check, Cpu, Speaker } from "lucide-react";
import { Field } from "./components";
import { options } from "./Settings";
import type { Settings } from "./model";

export default function Setup({
  settings,
  finish,
}: {
  settings: Settings;
  finish: (settings: Settings) => void;
}) {
  const [step, setStep] = useState(0);
  const [draft, setDraft] = useState(settings);
  const steps = ["Audio board", "Input", "Storage", "Ready"];
  function field(key: keyof Settings, label: string) {
    return (
      <Field
        label={label}
        value={draft[key]}
        options={options[key]}
        onChange={(value) => setDraft({ ...draft, [key]: value })}
      />
    );
  }
  return (
    <div className="setup-page">
      <div className="page-heading">
        <div>
          <h1>A home for your records</h1>
          <p>Let's get your Raspberry Pi ready to listen.</p>
        </div>
      </div>
      <ol className="setup-progress">
        {steps.map((label, index) => (
          <li
            key={label}
            aria-current={step === index ? "step" : undefined}
            className={index <= step ? "reached" : ""}
          >
            <span>{index < step ? <Check size={14} /> : index + 1}</span>
            {label}
          </li>
        ))}
      </ol>
      <section className="setup-card panel">
        <div className="setup-symbol">
          {step === 3 ? (
            <Check size={30} />
          ) : step === 1 ? (
            <Speaker size={30} />
          ) : (
            <Cpu size={30} />
          )}
        </div>
        <h2>
          {
            [
              "Start with your audio board",
              "Set your recording format",
              "Choose where the music lives",
              "You're ready to drop the needle",
            ][step]
          }
        </h2>
        <p>
          {
            [
              "Choose the board connected to your Pi. You can change this in Settings.",
              "Capture the detail once. Streaming quality can be changed separately.",
              "Keep complete sessions in lossless FLAC, ready to review and download.",
              "Open Listen to select your speakers and start your first recording.",
            ][step]
          }
        </p>
        <div className="setup-fields">
          {step === 0 && (
            <>
              {field("board", "Audio board")}
              <div className="connection-note">
                <Check size={16} /> Raspberry Pi OS 64-bit detected
              </div>
            </>
          )}
          {step === 1 && (
            <>
              {field("input", "Input device")}
              <div className="metadata-fields">
                {field("rate", "Sample rate")}
                {field("depth", "Bit depth")}
              </div>
            </>
          )}
          {step === 2 && (
            <>
              {field("storage", "Recording storage")}
              {field("compression", "FLAC compression")}
            </>
          )}
          {step === 3 && (
            <dl className="setup-summary">
              <div>
                <dt>Audio board</dt>
                <dd>{draft.board}</dd>
              </div>
              <div>
                <dt>Capture</dt>
                <dd>
                  {draft.rate} · {draft.depth} · Stereo
                </dd>
              </div>
              <div>
                <dt>Recording</dt>
                <dd>Lossless FLAC</dd>
              </div>
              <div>
                <dt>Opening buffer</dt>
                <dd>{draft.preRoll}</dd>
              </div>
            </dl>
          )}
        </div>
        <div className="form-actions">
          <button disabled={step === 0} onClick={() => setStep(step - 1)}>
            <ArrowLeft size={16} /> Back
          </button>
          <button
            className="primary"
            onClick={() => (step === 3 ? finish(draft) : setStep(step + 1))}
          >
            {step === 3 ? "Start listening" : "Continue"}
            <ArrowRight size={16} />
          </button>
        </div>
      </section>
    </div>
  );
}
