import { PulsebeamClient } from "./lib.ts";

function generateRandomId(length: number) {
  const characters =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  let result = "";
  const charactersLength = characters.length;
  for (let i = 0; i < length; i++) {
    result += characters.charAt(Math.floor(Math.random() * charactersLength));
  }
  return result;
}

const startButton = document.getElementById("start");
startButton?.addEventListener("click", async () => {
  const client = new PulsebeamClient("http://localhost:3000");
  client.connectionState.subscribe((s) => console.log(s));
  await client.connect("default", generateRandomId(8));

  const stream = await navigator.mediaDevices.getUserMedia({ video: true });
  await client.publishTrack(stream.getTracks()[0]);
});
