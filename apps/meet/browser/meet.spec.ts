import { expect, test } from "@playwright/test";

test("real Meet server exposes the lobby", async ({ page }) => {
  test.skip(!process.env.MEET_URL, "set MEET_URL to a served Meet export and start the PulseBeam SFU separately");
  await page.goto("/");
  await expect(page.getByPlaceholder("Room ID")).toBeVisible();
  await expect(page.getByRole("button", { name: "Join Room" })).toBeDisabled();
});
