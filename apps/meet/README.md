# PulseBeam Meet

A browser meeting client built with the local `@pulsebeam/react` SDK. Use it to
publish camera/microphone media, view remote participants, and exchange chat and
reactions through topics.

## Join a local room

Use a Linux x86_64 checkout with the [host prerequisites](../../docs/build.md#host-prerequisites)
installed. Run commands from the repository root; Bazel builds the Web and React
packages automatically. Initial downloads require network access.

1. Start a local development server in its own terminal:

   ```bash
   ./bazel run --config=dev //:server -- --dev
   ```

   Development mode uses fixed, public credentials. Keep it local and protect
   its listeners with your host firewall; do not expose it to the internet.

2. Start Meet in a second terminal:

   ```bash
   ./bazel run //:meet_dev -- --port 3000
   ```

   When Next.js reports that it is ready, open <http://localhost:3000>. This
   localhost origin allows browser capture without a separate HTTPS setup.

3. Mint a development token in a third terminal:

   ```bash
   ./bazel run //:cli -- token --room demo --participant alice
   ```

4. Allow camera and microphone access. The lobby requires both capture tracks
   before **Join room** becomes enabled, even if you only intend to view media.
   Confirm the camera preview appears, paste the token into **Bearer token**,
   set **Server URL** to `http://localhost:7070`, and select **Join room**.

The room should show `alice`, `Room demo`, and a `connected` status. To see remote
video, mint another token with `--participant bob` in the same room and join from
a second tab. Mute both microphones to avoid feedback. Select the remote video
tile to spotlight it, or open **Chat** to exchange messages. The complete
[local two-participant demo](../../README.md#local-development-demo) also describes server ports.

If capture fails, enable camera and microphone permissions and select **Retry**.
If admission fails, check the server URL, confirm the server uses `--dev`, and
mint a fresh token. Development tokens expire after one hour.

Select **Leave** to disconnect and stop captured media. Then close the tabs and
stop Meet and the server with Ctrl+C. The lobby requests capture again when you
return to it, so close the tab when finished.

## Build and check

```bash
./bazel test //apps/meet:check
./bazel build //:meet
./bazel run //apps/meet:preview -- --listen 3000
```

The static export uses `https://demo.pulsebeam.dev`, not the local server.
Development mode defaults to `http://localhost:7070` when
`NEXT_PUBLIC_PULSEBEAM_SERVER_URL` is unset. The lobby's **Server URL** field lets
you choose another endpoint. For the local quickstart, use the development server
rather than the static preview.

`check` covers static checks and the export artifact. The Web runtime's
[browser tests](../../agents/pulsebeam-agent-web/README.md) exercise Meet's real
media and topic behavior through `./bazel test //agents/pulsebeam-agent-web:browser`.

## Media and topics

Application identities come from admission. Remote video demand follows mounted
consumers. Reconnect retains logical media consumers, receive policies, local
publications, and topic subscriptions; replacement receivers restore playback
without remounting.

Camera and microphone controls enable or disable media handles rather than
detaching sources. Ordinary room interactions retry audio playback if the browser
has blocked it. Meet has no separate audio-unlock control.

Meet predeclares chat and reaction publisher channels before connecting. Reliable
topics use receiver-driven bounded recovery. A warning reports chat messages that
may be missing after recovery; **Retry chat** retries a failed subscription.

## Native reconnect limitation

The native DTLS reconnect simulation is excluded from ordinary acceptance under
its [documented exception](../../crates/pulsebeam-simulator/docs/native-dtls-exception.md).
It remains available through the opt-in command in that document. This limitation
does not exclude the browser media and topic recovery contracts.
