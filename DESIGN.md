---
name: "RaspAir"
description: "Warm paper, olive controls, and clear audio states for the listening room."
colors:
  accent: "#646c42"
  accent-hover: "#4e5532"
  accent-soft: "#e6e7d6"
  warm: "#a25331"
  warm-soft: "#f2e1d0"
  error: "#9a4434"
  ground: "#f3eee3"
  paper: "#fffcf5"
  header: "#ebe7d9"
  ink: "#3e4238"
  muted: "#6c6e5f"
  line: "#dedace"
  recording: "#e5e6d3"
  recording-muted: "#60634f"
  nav-active: "#dfe1cb"
  focus: "#ba873b"
  on-accent: "#fffef6"
typography:
  display:
    fontFamily: "\"Manrope Variable\", sans-serif"
    fontSize: "44px"
    fontWeight: 550
    letterSpacing: "-0.04em"
  headline:
    fontFamily: "\"Manrope Variable\", sans-serif"
    fontSize: "30px"
    fontWeight: 700
    lineHeight: 1.25
    letterSpacing: "-0.035em"
  title:
    fontFamily: "\"Manrope Variable\", sans-serif"
    fontSize: "23px"
    fontWeight: 700
    letterSpacing: "-0.025em"
  section:
    fontFamily: "\"Manrope Variable\", sans-serif"
    fontSize: "15px"
    fontWeight: 750
  body:
    fontFamily: "\"Manrope Variable\", sans-serif"
    fontSize: "13px"
    fontWeight: 400
    lineHeight: 1.7
  label:
    fontFamily: "\"Manrope Variable\", sans-serif"
    fontSize: "13px"
    fontWeight: 650
  field-label:
    fontFamily: "\"Manrope Variable\", sans-serif"
    fontSize: "12px"
    fontWeight: 650
  caption:
    fontFamily: "\"Manrope Variable\", sans-serif"
    fontSize: "11px"
    fontWeight: 400
rounded:
  control: "8px"
  field: "7px"
  panel: "14px"
  speaker: "10px"
  status: "6px"
spacing:
  compact: "8px"
  small: "12px"
  field: "18px"
  panel: "24px"
  form: "30px"
  page: "42px"
components:
  button-primary:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.on-accent}"
    typography: "{typography.label}"
    rounded: "{rounded.control}"
    padding: "11px 16px"
  button-primary-hover:
    backgroundColor: "{colors.accent-hover}"
    textColor: "#ffffff"
  button-secondary:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    typography: "{typography.label}"
    rounded: "{rounded.control}"
    padding: "11px 16px"
  button-record-stop:
    backgroundColor: "#fff9f3"
    textColor: "{colors.error}"
    typography: "{typography.label}"
    rounded: "{rounded.control}"
    padding: "11px 16px"
  button-text:
    backgroundColor: "transparent"
    textColor: "{colors.accent}"
    rounded: "{rounded.control}"
    padding: "4px 0"
  field:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    rounded: "{rounded.field}"
    padding: "10px 12px"
  navigation:
    backgroundColor: "transparent"
    textColor: "{colors.muted}"
    rounded: "{rounded.control}"
    padding: "10px 15px"
  navigation-active:
    backgroundColor: "{colors.nav-active}"
    textColor: "{colors.ink}"
  status:
    backgroundColor: "#e5e6d4"
    textColor: "#5a613c"
    rounded: "{rounded.status}"
    padding: "7px 10px"
  panel:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    rounded: "{rounded.panel}"
    padding: "24px"
  toggle:
    backgroundColor: "#b7c3b1"
    rounded: "20px"
    width: "33px"
    height: "20px"
  waveform:
    backgroundColor: "transparent"
    textColor: "{colors.accent}"
    height: "118px"
---

# Design System: RaspAir

## Overview

**Creative North Star: "Listening room"**

Warm paper colors, olive controls, and Manrope text frame the music. Fine borders separate tasks, while the retained opening in the waveform gives terracotta a specific audio meaning.

The interface keeps song information beside the controls that affect it. Geometric record sleeves identify tracks; compact labels and tabular timers support listening, recording, and review.

**Key Characteristics:**

- Flat paper-colored panels with fine borders.
- Olive actions and selected states.
- Terracotta audio boundaries and retained openings.
- Manrope headings, compact controls, and tabular timers.

## Colors

### Primary

Olive `accent` carries primary buttons, checked controls, and the main waveform. `accent-hover` darkens primary buttons; `accent-soft` marks secondary hover and selected filters. `nav-active` marks the current page.

### Secondary

Terracotta `warm` marks retained audio and review boundaries. `warm-soft` fills the retained opening and restart notice. `error` identifies validation text and the stop-recording action.

### Neutral

`ground` is the page background, `header` the navigation band, and `paper` the main panel and field fill. `ink` carries headings and controls; `muted` carries supporting text. `line` divides rows and panels. The olive `recording` panel uses `recording-muted` for its description and timer caption. `on-accent` supplies primary-button text. Amber `focus` identifies keyboard focus.

**The audio color rule.** Use olive for controls and the main waveform. Use terracotta for the retained opening and boundary markers.

## Typography

Manrope Variable supplies every text role. Page headings use `headline`, song titles use `title`, panel headings use `section`, and paragraphs use `body`. Labels use sentence case. Paragraphs have a maximum width of 70ch.

The timer uses `display`. At phone width, the timer drops to 37px and the page heading to 25px. The live song title becomes 21px. Settings headings use 20px; setup headings use 23px and 22px on phones. Field text grows from 13px to 16px on phones.

**The stable timer rule.** Use tabular numerals for elapsed time, playback position, and waveform labels.

## Layout

A sticky horizontal header is 82px tall, above a 54px context bar. Main content is centered within 1480px, with 42px side padding. The Listen workspace places its live and recording panels beside a recent-song list. Speakers sit beneath the live controls. The outer grid uses a 3:1 ratio with a 26px gap; the live and recording panels use a 1.9:1 ratio with an 18px gap.

At 1200px, recent songs move below the speakers. At 700px, the header is 68px tall, navigation uses icons, and main content has 18px side padding. Live input, recording, speakers, and recent songs stack in that order. The recording timer and action retain a two-column arrangement inside their panel.

Recording rows use aligned columns and bottom rules. The album column disappears at 1150px; phones retain the song, artist, and actions. Settings use a 178px category column beside a form, then horizontal categories and stacked fields on phones. The setup flow is centered within 750px.

## Elevation & Depth

Panels remain flat. Background tones, one-pixel borders, and whitespace establish grouping. The toast has the sole shadow, `0 8px 28px #3e423826`. The sticky header and bottom audio player use borders to separate them from scrolling content.

**The flat panel rule.** Separate panels with paper tones and fine borders. Reserve the soft shadow for the floating toast.

## Shapes

Panels use `panel` corners, buttons and search use `control`, fields use `field`, speaker options use `speaker`, and status labels use `status`. Record sleeves stay square with clipped circular artwork. Circular status dots, switch thumbs, and setup step numbers identify state.

## Components

- Buttons have a 42px minimum height. Primary buttons use olive; secondary buttons use paper and a fine border. Text actions use an unfilled treatment. Stop recording uses a warm pale fill and red-brown text. Disabled buttons use 45% opacity.
- Fields have a 44px minimum height, a fine warm-gray border, and labels above them. Helper text sits below the field; validation appears near the form actions. Search keeps its icon inside the field boundary.
- Navigation pairs icons with labels on desktop and uses icons on phones. The active page and hover state share the pale olive fill. Selected filters and settings categories use the softer olive fill.
- Status labels pair a small dot with text. Unreviewed songs keep a visible Review action on phones. The dot supports the label.
- Main panels use paper; recording uses the olive panel tone. Live and review panels have 24px padding on desktop and 20px on phones. Settings and setup use more interior space for forms.
- Switches use a rounded track and a white thumb. Checking a switch turns its track olive and moves the thumb horizontally.
- The waveform uses narrow rounded bars, a terracotta retained opening, and a dashed boundary. It is 118px high in the desktop live panel and 84px on phones; song review uses 110px.
- Record sleeves use sage, blue, or rose fields with cropped circular bands. Library sleeves are 42px square; the live sleeve is 76px and becomes 60px at narrower widths.

Button colors and switch position change over 160ms with ease-out timing. The recording dot appears over 420ms with `cubic-bezier(0.16, 1, 0.3, 1)`. Reduced motion removes both transitions and the recording animation. Keyboard focus uses a three-pixel amber outline with a three-pixel offset; switches use terracotta focus.

## Do's and Don'ts

- Do keep playback and recording states visually distinct.
- Do retain the terracotta opening highlight and the geometric record sleeves.
- Do keep Review visible for songs awaiting review on phones.
- Do use the darker recording captions on the olive recording panel.
- Do preserve visible keyboard focus and reduced-motion behavior.

- Don't use shadows to separate ordinary panels.
- Don't turn the record-sleeve artwork into a rounded card.
- Don't hide an unreviewed song's action behind its status dot.
