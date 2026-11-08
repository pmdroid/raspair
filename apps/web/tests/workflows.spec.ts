import { mkdir } from "node:fs/promises";
import { expect, test } from "@playwright/test";

test("recording is independent of speaker playback and adds a library entry", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("button", { name: "Record now", exact: true }).click();
  await page.getByRole("button", { name: "Stop playback", exact: true }).click();
  await expect(page.getByRole("button", { name: "Stop recording", exact: true })).toBeVisible();
  await page.getByRole("checkbox", { name: "Play in Living room" }).uncheck();
  await expect(page.getByRole("button", { name: "Play on speakers" })).toBeDisabled();
  await page.getByRole("button", { name: "Stop recording", exact: true }).click();
  await page.getByRole("button", { name: "Open library" }).click();
  await expect(
    page.getByRole("button", { name: "Play New listening session", exact: true }),
  ).toBeVisible();
});

test("settings persist and cannot interrupt an active recording", async ({ page }) => {
  await page.goto("/#settings");
  await page.getByLabel("Sample rate", { exact: true }).selectOption("96 kHz");
  await page.getByRole("button", { name: "Apply changes" }).click();
  await page.reload();
  await expect(page.getByLabel("Sample rate", { exact: true })).toHaveValue("96 kHz");
  await page.getByRole("button", { name: "Listen", exact: true }).click();
  await page.getByRole("button", { name: "Record now", exact: true }).click();
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await page.getByLabel("Bit depth", { exact: true }).selectOption("16 bit");
  await page.getByRole("button", { name: "Apply changes" }).click();
  await expect(page.getByRole("alert")).toContainText("Stop recording");
  await page
    .getByRole("navigation")
    .getByRole("button", { name: "Device setup", exact: true })
    .click();
  await expect(page.getByRole("heading", { name: "Recording in progress" })).toBeVisible();
});

test("library search, boundaries, playback and a FLAC download work", async ({ page }) => {
  await page.goto("/#recordings");
  await page.getByRole("textbox", { name: "Search recordings" }).fill("no such song");
  await expect(page.getByRole("heading", { name: "No songs found" })).toBeVisible();
  await page.getByRole("button", { name: "Show all songs" }).click();
  await expect(
    page
      .getByRole("button", { name: "Review A little further", exact: true })
      .getByText("Review", { exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Review A little further", exact: true }).click();
  await page.getByLabel("Song title").fill("Sunday take");
  await page.getByLabel("Start · seconds").fill("8");
  await page.getByLabel("End · seconds").fill("4");
  await page.getByRole("button", { name: "Save song" }).click();
  await expect(page.getByRole("alert")).toContainText("Choose an end after the start");
  await page.getByLabel("End · seconds").fill("20");
  await page.getByRole("button", { name: "Save song" }).click();
  await page.getByRole("button", { name: "Play Sunday take", exact: true }).click();
  await expect(page.getByRole("button", { name: "Pause recording", exact: true })).toBeVisible();
  await expect
    .poll(() => page.locator("audio").evaluate((audio: HTMLAudioElement) => audio.currentTime))
    .toBeGreaterThan(8);
  await page.getByRole("button", { name: "Pause recording", exact: true }).click();
  const downloadPromise = page.waitForEvent("download");
  await page.getByRole("link", { name: "Download Sunday take", exact: true }).click();
  const download = await downloadPromise;
  expect(download.suggestedFilename()).toBe("Sunday take.flac");
  expect(await download.failure()).toBeNull();
});

test("setup saves the selected board and opens Listen", async ({ page }) => {
  await page.goto("/#device-setup");
  await page.getByLabel("Audio board", { exact: true }).selectOption("HiFiBerry DAC+ ADC");
  for (let step = 0; step < 3; step++)
    await page.getByRole("button", { name: "Continue", exact: true }).click();
  await expect(page.locator(".setup-summary")).toContainText("HiFiBerry DAC+ ADC");
  await page.getByRole("button", { name: "Start listening", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Your listening room" })).toBeVisible();
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await expect(page.getByLabel("Audio board", { exact: true })).toHaveValue("HiFiBerry DAC+ ADC");
});

test("capture the complete responsive mock and check for overflow", async ({ page }, testInfo) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const folder = "../../.impeccable/review";
  await mkdir(folder, { recursive: true });
  async function capture(name: string) {
    await page.evaluate(() => document.fonts.ready);
    await page.evaluate(() =>
      Promise.all(document.getAnimations().map((animation) => animation.finished)),
    );
    expect(
      await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth),
    ).toBe(true);
    await page.screenshot({
      path: `${folder}/${name}-${testInfo.project.name}.png`,
      fullPage: true,
      animations: "disabled",
    });
  }
  await page.goto("/");
  await capture("listen");
  await page.goto("/#recordings");
  await expect(page.getByRole("heading", { name: "Your record collection" })).toBeVisible();
  await capture("recordings");
  await page.getByRole("button", { name: "Review A little further", exact: true }).click();
  await capture("review");
  await page.goto("/#settings");
  await expect(page.getByRole("heading", { name: "Make it yours" })).toBeVisible();
  await capture("settings");
  await page.getByRole("button", { name: "Streaming", exact: true }).click();
  await capture("streaming");
  await page.goto("/#device-setup");
  await expect(page.getByRole("heading", { name: "A home for your records" })).toBeVisible();
  await capture("setup");
  expect(errors).toEqual([]);
});
