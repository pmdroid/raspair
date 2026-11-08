export type Page = "Listen" | "Recordings" | "Settings" | "Device setup";
export type Track = {
  id: number;
  title: string;
  artist: string;
  album: string;
  color: string;
  date: string;
  reviewed: boolean;
  start?: number;
  end?: number;
};

export const initialTracks: Track[] = [
  {
    id: 1,
    title: "A little further",
    artist: "The Quiet Hours",
    album: "Sunday sessions",
    color: "sage",
    date: "Today, 14:32",
    reviewed: false,
  },
  {
    id: 2,
    title: "Open water",
    artist: "Aster Mills",
    album: "Tide studies",
    color: "blue",
    date: "Today, 14:28",
    reviewed: true,
  },
  {
    id: 3,
    title: "Everything in its place",
    artist: "The Quiet Hours",
    album: "Sunday sessions",
    color: "sage",
    date: "Yesterday, 18:46",
    reviewed: true,
  },
  {
    id: 4,
    title: "After the rain",
    artist: "June Collective",
    album: "Soft focus",
    color: "rose",
    date: "Yesterday, 18:42",
    reviewed: true,
  },
  {
    id: 5,
    title: "Passing through",
    artist: "Aster Mills",
    album: "Tide studies",
    color: "blue",
    date: "Friday, 20:16",
    reviewed: true,
  },
  {
    id: 6,
    title: "The long way home",
    artist: "June Collective",
    album: "Soft focus",
    color: "rose",
    date: "Friday, 20:12",
    reviewed: true,
  },
];

export const defaultSettings = {
  board: "HiFiBerry DAC2 ADC Pro",
  input: "HiFiBerry stereo input",
  rate: "48 kHz",
  depth: "24 bit",
  gain: "0 dB",
  compression: "5 · Balanced",
  storage: "SD card · 46.8 GB free",
  mode: "Auto",
  quality: "192 kbps MP3",
  detection: "Adaptive",
  preRoll: "10 seconds",
  wifi: "Listening room",
  protocol: "AirPlay",
};

export type Settings = typeof defaultSettings;

export function loadSettings(): Settings {
  try {
    const saved = JSON.parse(localStorage.getItem("raspair-preview-settings") ?? "{}");
    return Object.fromEntries(
      Object.entries(defaultSettings).map(([key, value]) => [
        key,
        typeof saved?.[key] === "string" ? saved[key] : value,
      ]),
    ) as Settings;
  } catch {
    return { ...defaultSettings };
  }
}

export function time(seconds: number) {
  return `${Math.floor(seconds / 60)
    .toString()
    .padStart(2, "0")}:${Math.floor(seconds % 60)
    .toString()
    .padStart(2, "0")}`;
}
